//! The studio backend: Roblox Studio as a spawned runtime.
//!
//! Studio's official CLI runs a script after a place loads
//! (`--task RunScript --runScriptFile … --outputFile … --quitAfterExecution`),
//! which makes the whole backend symmetric with lune/lute: bundle the suite
//! (the same cloud bundler, with the studio head printing sentinel-framed
//! events), launch Studio on the configured place, wait, decode the output
//! file with the spawned-runtime decoder. Zero interaction — no plugin, no
//! bridge, no permission prompts. The costs, stated plainly: every run pays
//! a Studio boot, the script executes against the *place file or published
//! place* (never an unsaved open session), and execution is Studio's
//! RunScript context rather than a stepping Run-mode playtest.
//!
//! (An earlier warm-session design — a companion plugin, arm-and-press-Run —
//! ran real playtests with ~1s dispatch; it was shelved for the zero-click
//! model. The archived design lives in the repo's working notes.)
//!
//! Never a CI backend: Studio is a GUI application with a login; engine
//! suites in CI belong on cloud. The guard is loud so a misconfigured
//! pipeline cannot hang on a Studio that will never exist.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::backend::cloud::bundle::{self, BundleInput, Head, SpecEntry};
use crate::backend::runtime::{classify, passthrough, Decoded, DONE_SENTINEL, SENTINEL};
use crate::backend::{display_rel, EventSink, SuitePlan};
use crate::config::CloudTarget;
use crate::error::ToolError;
use crate::report::{check_protocol_version, Event, Failure};

/// Fixed allowance for Studio to boot and load the place, on top of the
/// per-spec budgets. Boots are machine- and place-dependent; generous
/// beats a race.
const BOOT_ALLOWANCE: Duration = Duration::from_secs(180);

/// How often the child process is polled for exit.
const WAIT_POLL: Duration = Duration::from_millis(250);

pub fn run(
    plan: &SuitePlan,
    target: &CloudTarget,
    on_event: &mut EventSink,
) -> Result<(), ToolError> {
    if crate::is_ci() {
        return Err(ToolError(
            "the studio backend launches the Studio application and cannot run in CI — engine \
             suites in CI belong on the cloud backend"
                .into(),
        ));
    }

    let exe = studio_executable(plan.studio_executable.as_deref())?;
    let place = place_source(target, &plan.root)?;

    let entries: Vec<SpecEntry> = plan
        .specs
        .iter()
        .map(|spec| SpecEntry {
            name: display_rel(spec, &plan.root),
            path: spec.clone(),
        })
        .collect();

    // `[settings] rojo`, honored exactly as the cloud backend honors it: the
    // launched place is the one mapped requires delegate into.
    let place_map = match &plan.rojo_project {
        Some(project) => Some(
            crate::resolve::VirtualDataModel::from_project_file(project)
                .map_err(|e| ToolError(e.to_string()))?,
        ),
        None => None,
    };

    // Per-spec deadline: the cloud rule (single-spec budget plus fixed
    // slack), for the cloud reasons — the engine cannot preempt.
    let budget = plan.timeout.saturating_add(Duration::from_secs(10));
    let deadline_ms = u64::try_from(budget.as_millis().max(1)).unwrap_or(u64::MAX);

    let input = BundleInput {
        core_entry: &plan.core_entry,
        specs: &entries,
        name_filter: plan.name_filter.as_deref(),
        head: Head::Studio,
        deadline_ms,
        place: place_map.as_ref(),
    };
    let mut sources = bundle::SourceCache::default();
    let built = bundle::bundle_with_cache(&input, &mut sources)?;
    let mut warned: HashSet<bundle::UnresolvedRequire> = HashSet::new();
    for miss in &built.unresolved {
        if warned.insert(miss.clone()) {
            crate::report::warn_to_stderr(&crate::backend::cloud::unresolved_warning(
                miss, &plan.root,
            ));
        }
    }

    let work_dir = plan.root.join(".lest");
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| ToolError(format!("cannot create {}: {e}", work_dir.display())))?;
    let script_path = work_dir.join("studio-run.luau");
    let output_path = work_dir.join("studio-output.txt");
    std::fs::write(&script_path, &built.script)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", script_path.display())))?;
    // A stale output file from an earlier run must never be decoded as this
    // run's results.
    if output_path.exists() {
        std::fs::remove_file(&output_path)
            .map_err(|e| ToolError(format!("cannot clear {}: {e}", output_path.display())))?;
    }

    let spec_count = u32::try_from(plan.specs.len().max(1)).unwrap_or(u32::MAX);
    let overall = BOOT_ALLOWANCE.saturating_add(budget.saturating_mul(spec_count));

    crate::report::note_to_stderr(&format!(
        "launching Roblox Studio (a boot takes a while; budget {}s)…",
        overall.as_secs()
    ));

    let mut child = Command::new(&exe)
        .args(launch_args(&script_path, &output_path, &place))
        .current_dir(&plan.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ToolError(format!("cannot launch {}: {e}", exe.display())))?;

    // Studio is a GUI process: no streaming to decode, so completion is its
    // exit (--quitAfterExecution) bounded by the budget.
    let deadline = Instant::now()
        .checked_add(overall)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_status)) => break false,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break true;
                }
                std::thread::sleep(WAIT_POLL);
            }
            Err(e) => {
                let _ = child.kill();
                return Err(ToolError(format!("cannot wait for Studio: {e}")));
            }
        }
    };

    // Decode whatever made it to disk — on a timeout the partial file still
    // holds every event Studio flushed, and discarding streamed outcomes
    // behind an error would hide everything that already ran.
    let contents = std::fs::read_to_string(&output_path).unwrap_or_default();
    let mut decoder = LineDecoder::new(plan);
    let mut done_seen = false;
    for line in contents.lines() {
        if is_done_framed(line) {
            done_seen = true;
        }
        decoder.feed(line, on_event)?;
    }
    let (outcomes, _saw_protocol, current_spec) = decoder.into_parts();

    if timed_out {
        // A hang inside the suite is a test failure (exit 1), matching the
        // other backends' budget expiries.
        let spec = current_spec.map(|i| plan.specs[i].as_path());
        let path = spec
            .map(|p| display_rel(p, &plan.root))
            .unwrap_or_else(|| plan.name.clone());
        let event = Event::TestFail {
            path: vec![path],
            name: "(timeout)".to_string(),
            duration_ms: overall.as_millis() as f64,
            failure: Failure::Error {
                message: format!(
                    "Studio exceeded suite \"{}\"'s budget ({}s) and was closed — a slow boot, \
                     a login prompt, or a hung test",
                    plan.name,
                    overall.as_secs()
                ),
                trace: None,
            },
            origin: None,
        };
        on_event(spec, &event);
        return Ok(());
    }

    if !done_seen {
        if outcomes == 0 {
            return Err(ToolError(format!(
                "Studio exited without completing suite \"{}\" — the bundle likely failed to \
                 load; its output (if any) is kept at {}",
                plan.name,
                output_path.display()
            )));
        }
        // Partial results then silence: report the death against the spec
        // that was running, keep what streamed.
        let spec = current_spec.map(|i| plan.specs[i].as_path());
        let path = spec
            .map(|p| display_rel(p, &plan.root))
            .unwrap_or_else(|| plan.name.clone());
        let event = Event::TestFail {
            path: vec![path],
            name: "(aborted)".to_string(),
            duration_ms: 0.0,
            failure: Failure::Error {
                message: format!(
                    "Studio exited before suite \"{}\" finished — output kept at {}",
                    plan.name,
                    output_path.display()
                ),
                trace: None,
            },
            origin: None,
        };
        on_event(spec, &event);
        return Ok(());
    }

    // The same false-green guard every backend carries, disarmed under a
    // name filter (which legitimately selects zero tests — core drops
    // non-matching tests without a skip event).
    if outcomes == 0 && !plan.specs.is_empty() && plan.name_filter.is_none() {
        return Err(ToolError(format!(
            "Studio ran {} spec file(s) for suite \"{}\" but produced no test outcomes — \
             output kept at {}",
            plan.specs.len(),
            plan.name,
            output_path.display()
        )));
    }

    // Success: the generated artifacts are noise now.
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&output_path);
    Ok(())
}

/// Where the suite runs: a local place file, or a published place.
#[derive(Debug, PartialEq, Eq)]
enum PlaceSource {
    File(PathBuf),
    Published { place: String, universe: String },
}

/// Resolves the place the launch opens. Studio must be told a place; there
/// is no "whatever happens to be open" in the launch model.
fn place_source(target: &CloudTarget, root: &Path) -> Result<PlaceSource, ToolError> {
    if let Some(file) = &target.place_file {
        let path = root.join(file);
        if !path.is_file() {
            return Err(ToolError(format!(
                "the configured place_file does not exist: {}",
                path.display()
            )));
        }
        return Ok(PlaceSource::File(path));
    }
    if let (Some(place), Some(universe)) = (&target.place_id, &target.universe_id) {
        return Ok(PlaceSource::Published {
            place: place.clone(),
            universe: universe.clone(),
        });
    }
    Err(ToolError(
        "the studio backend needs a place to launch — set `[cloud] place_file` (a built .rbxl) \
         or `place_id` + `universe_id` in lest.toml"
            .into(),
    ))
}

/// The Studio CLI arguments for one run, per the documented flags.
fn launch_args(script: &Path, output: &Path, place: &PlaceSource) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--task".into(),
        "RunScript".into(),
        "--runScriptFile".into(),
        script.as_os_str().to_owned(),
        "--outputFile".into(),
        output.as_os_str().to_owned(),
        "--quitAfterExecution".into(),
    ];
    match place {
        PlaceSource::File(path) => {
            args.push("--localPlaceFile".into());
            args.push(path.as_os_str().to_owned());
        }
        PlaceSource::Published { place, universe } => {
            args.push("--placeId".into());
            args.push(place.into());
            args.push("--universeId".into());
            args.push(universe.into());
        }
    }
    args
}

/// Finds the Studio executable: the `[studio] executable` override first,
/// then the platform's install location.
fn studio_executable(explicit: Option<&Path>) -> Result<PathBuf, ToolError> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(ToolError(format!(
            "the configured studio executable does not exist: {}",
            path.display()
        )));
    }
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                ToolError("cannot locate Roblox Studio ($LOCALAPPDATA is not set)".into())
            })?;
        let versions = base.join("Roblox").join("Versions");
        newest_studio_exe(&versions).ok_or_else(|| {
            ToolError(format!(
                "cannot find RobloxStudioBeta.exe under {} — is Studio installed? (or set \
                 `[studio] executable` in lest.toml)",
                versions.display()
            ))
        })
    } else if cfg!(target_os = "macos") {
        let path = PathBuf::from("/Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio");
        if path.is_file() {
            Ok(path)
        } else {
            Err(ToolError(
                "cannot find Roblox Studio in /Applications — is it installed? (or set \
                 `[studio] executable` in lest.toml)"
                    .into(),
            ))
        }
    } else {
        Err(ToolError(
            "Roblox Studio does not run on this platform — the studio backend needs Windows or \
             macOS (engine suites can still run anywhere through the cloud backend)"
                .into(),
        ))
    }
}

/// The newest per-version directory containing the Studio binary. Roblox
/// installs each build under Versions/<hash>/; modification time picks the
/// current one.
fn newest_studio_exe(versions: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(versions).ok()? {
        let entry = entry.ok()?;
        let exe = entry.path().join("RobloxStudioBeta.exe");
        if exe.is_file() {
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, exe));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// The done marker only *frames* a line when no event marker precedes it: a
/// legitimate event payload (a snapshot's text, a failure message) may
/// contain the marker characters, and dropping that line would lose a real
/// verdict. The first-marker-wins rule from `classify`, applied here.
fn is_done_framed(line: &str) -> bool {
    match line.find(DONE_SENTINEL) {
        None => false,
        Some(done_at) => line
            .find(SENTINEL)
            .is_none_or(|event_at| done_at < event_at),
    }
}

/// Decodes output-file sentinel lines into protocol events, tracking the
/// state the run outcome needs afterward. Split from `run` so the decode
/// rules — boundary mapping, the done-framing skip, protocol validation,
/// outcome counting — are testable without launching anything.
struct LineDecoder<'p> {
    plan: &'p SuitePlan,
    outcomes: usize,
    saw_protocol: bool,
    current_spec: Option<usize>,
}

impl<'p> LineDecoder<'p> {
    fn new(plan: &'p SuitePlan) -> Self {
        LineDecoder {
            plan,
            outcomes: 0,
            saw_protocol: false,
            current_spec: None,
        }
    }

    fn into_parts(self) -> (usize, bool, Option<usize>) {
        (self.outcomes, self.saw_protocol, self.current_spec)
    }

    fn feed(&mut self, line: &str, on_event: &mut EventSink) -> Result<(), ToolError> {
        // The done marker is framing, not output — but only when it actually
        // frames the line (see `is_done_framed`).
        if is_done_framed(line) {
            return Ok(());
        }
        match classify(line) {
            Decoded::SpecBoundary { leading, index } => {
                passthrough(leading);
                self.saw_protocol = true;
                let raw = index.trim();
                let resolved = raw
                    .parse::<usize>()
                    .ok()
                    .and_then(|one_based| one_based.checked_sub(1))
                    .filter(|&i| i < self.plan.specs.len());
                match resolved {
                    Some(index) => {
                        self.current_spec = Some(index);
                        Ok(())
                    }
                    None => Err(ToolError(format!(
                        "Studio's output carries the spec-boundary marker \"{raw}\", which is \
                         not a 1-based index into suite \"{}\"'s {} spec file(s) — the bundle \
                         and the CLI disagree about the spec list",
                        self.plan.name,
                        self.plan.specs.len()
                    ))),
                }
            }
            Decoded::Event { leading, json } => {
                passthrough(leading);
                self.saw_protocol = true;
                let event = serde_json::from_str::<Event>(json).map_err(|err| {
                    ToolError(format!(
                        "undecodable protocol line in Studio's output while running suite \
                         \"{}\": {err}",
                        self.plan.name
                    ))
                })?;
                if let Event::RunStart {
                    protocol_version, ..
                } = event
                {
                    check_protocol_version(protocol_version).map_err(|mismatch| {
                        ToolError(format!(
                            "framework/CLI protocol mismatch from the Studio run: {mismatch}"
                        ))
                    })?;
                }
                if matches!(
                    event,
                    Event::TestPass { .. } | Event::TestFail { .. } | Event::TestSkip { .. }
                ) {
                    self.outcomes += 1;
                }
                let spec = self.current_spec.map(|i| self.plan.specs[i].as_path());
                on_event(spec, &event);
                Ok(())
            }
            Decoded::Output => {
                // Test prints and Studio's own chatter both land in the
                // output file; echo them like every backend echoes output.
                println!("{line}");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn plan() -> SuitePlan {
        SuitePlan {
            name: "studio-suite".into(),
            specs: vec![PathBuf::from("a.spec.luau"), PathBuf::from("b.spec.luau")],
            root: PathBuf::from("."),
            core_entry: PathBuf::from("core/init.luau"),
            timeout: Duration::from_secs(5),
            workers: 0,
            name_filter: None,
            coverage: false,
            rojo_project: None,
            studio_executable: None,
        }
    }

    /// Feeds lines and records (spec attribution, event kind tag) pairs.
    fn feed_all(
        decoder: &mut LineDecoder,
        lines: &[&str],
    ) -> Result<Vec<(Option<PathBuf>, &'static str)>, ToolError> {
        let mut seen = Vec::new();
        for line in lines {
            let mut sink = |spec: Option<&Path>, event: &Event| {
                let tag = match event {
                    Event::RunStart { .. } => "run_start",
                    Event::TestPass { .. } => "test_pass",
                    Event::TestFail { .. } => "test_fail",
                    _ => "other",
                };
                seen.push((spec.map(Path::to_path_buf), tag));
            };
            decoder.feed(line, &mut sink)?;
        }
        Ok(seen)
    }

    #[test]
    fn boundaries_attribute_events_to_the_right_spec() {
        let plan = plan();
        let mut decoder = LineDecoder::new(&plan);
        let seen = feed_all(
            &mut decoder,
            &[
                "@@LEST_SPEC@@1",
                r#"@@LEST@@{"kind":"test_pass","path":[],"name":"a","durationMs":1}"#,
                "@@LEST_SPEC@@2",
                r#"@@LEST@@{"kind":"test_pass","path":[],"name":"b","durationMs":1}"#,
            ],
        )
        .expect("feed");
        assert_eq!(
            seen,
            vec![
                (Some(PathBuf::from("a.spec.luau")), "test_pass"),
                (Some(PathBuf::from("b.spec.luau")), "test_pass"),
            ]
        );
        let (outcomes, saw, current) = decoder.into_parts();
        assert_eq!(outcomes, 2);
        assert!(saw);
        assert_eq!(current, Some(1));
    }

    #[test]
    fn a_bad_boundary_is_a_tool_error() {
        let plan = plan();
        let mut decoder = LineDecoder::new(&plan);
        let err = feed_all(&mut decoder, &["@@LEST_SPEC@@7"]).expect_err("must fail");
        assert!(err.to_string().contains("spec-boundary marker"));
    }

    #[test]
    fn a_done_framed_line_is_skipped_but_a_done_in_a_payload_is_not() {
        let plan = plan();
        let mut decoder = LineDecoder::new(&plan);
        let seen = feed_all(
            &mut decoder,
            &[
                "@@LEST_STUDIO_DONE@@",
                r#"@@LEST@@{"kind":"test_fail","path":[],"name":"has @@LEST_STUDIO_DONE@@ inside","durationMs":1,"failure":{"type":"error","message":"x"}}"#,
            ],
        )
        .expect("feed");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, "test_fail");
    }

    #[test]
    fn done_framing_rule_matches_first_marker_wins() {
        assert!(is_done_framed("@@LEST_STUDIO_DONE@@"));
        assert!(is_done_framed("leading text @@LEST_STUDIO_DONE@@"));
        assert!(!is_done_framed(
            r#"@@LEST@@{"received":"@@LEST_STUDIO_DONE@@"}"#
        ));
        assert!(!is_done_framed("no markers at all"));
    }

    #[test]
    fn a_protocol_mismatch_aborts() {
        let plan = plan();
        let mut decoder = LineDecoder::new(&plan);
        let err = feed_all(
            &mut decoder,
            &[r#"@@LEST@@{"kind":"run_start","specCount":1,"protocolVersion":99}"#],
        )
        .expect_err("must fail");
        assert!(err.to_string().contains("protocol mismatch"));
    }

    #[test]
    fn undecodable_json_aborts() {
        let plan = plan();
        let mut decoder = LineDecoder::new(&plan);
        let err = feed_all(&mut decoder, &["@@LEST@@{not json"]).expect_err("must fail");
        assert!(err.to_string().contains("undecodable protocol line"));
    }

    #[test]
    fn place_source_prefers_the_file_and_requires_something() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("place.rbxl");
        std::fs::write(&file, "x").unwrap();

        let with_file = CloudTarget {
            universe_id: Some("1".into()),
            place_id: Some("2".into()),
            place_file: Some("place.rbxl".into()),
        };
        assert_eq!(
            place_source(&with_file, dir.path()).expect("file"),
            PlaceSource::File(file)
        );

        let published = CloudTarget {
            universe_id: Some("1".into()),
            place_id: Some("2".into()),
            place_file: None,
        };
        assert_eq!(
            place_source(&published, dir.path()).expect("published"),
            PlaceSource::Published {
                place: "2".into(),
                universe: "1".into()
            }
        );

        let nothing = CloudTarget::default();
        let err = place_source(&nothing, dir.path()).expect_err("must fail");
        assert!(err.to_string().contains("place_file"));
    }

    #[test]
    fn launch_args_carry_the_documented_flags() {
        let args = launch_args(
            Path::new("s.luau"),
            Path::new("o.txt"),
            &PlaceSource::Published {
                place: "22".into(),
                universe: "11".into(),
            },
        );
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "--task",
                "RunScript",
                "--runScriptFile",
                "s.luau",
                "--outputFile",
                "o.txt",
                "--quitAfterExecution",
                "--placeId",
                "22",
                "--universeId",
                "11",
            ]
        );
    }

    #[test]
    fn newest_studio_exe_picks_the_freshest_version_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("version-old");
        let new = dir.path().join("version-new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("RobloxStudioBeta.exe"), "x").unwrap();
        // Ensure a strictly newer directory mtime for the second version.
        std::thread::sleep(Duration::from_millis(30));
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("RobloxStudioBeta.exe"), "x").unwrap();

        let found = newest_studio_exe(dir.path()).expect("found");
        assert_eq!(found, new.join("RobloxStudioBeta.exe"));

        assert_eq!(newest_studio_exe(&dir.path().join("missing")), None);
    }
}
