//! The gargantuan backend: the Gargantuan engine as a spawned runtime.
//!
//! [Gargantuan](https://github.com/teamfireworks/gargantuan) is an
//! independent, Roblox-shaped game engine scripted with Luau. Its CLI runs a
//! script headlessly (`--script <file> --headless`), which makes the backend
//! a hybrid of two existing shapes: the suite is bundled exactly like
//! cloud/studio (the engine's own `require` is still settling, so every
//! module inlines and nothing delegates), and the process is driven exactly
//! like lune/lute — sentinel-framed events decoded live off a stdout pipe.
//!
//! One thing is unlike either: the run's ending is the CLI's to arrange.
//! The done marker is the completion authority, and what follows it is
//! two-mode. On engines with `ProcessService`, the head calls
//! `ExitAsync(0)` right after the marker: the engine exits cleanly, the
//! exit flushes stdio, and the CLI just reaps the child. On engines that
//! predate the service, nothing can stop the loop headless — the head pads
//! stdout in its place (a killed process discards its stdio buffer and
//! Luau's `print` never flushes, so an unpadded marker could sit in the
//! pipe buffer forever) and the CLI kills the process after a short grace
//! wait, deliberately.
//!
//! Experimental, stated plainly: the engine is pre-release, unversioned, and
//! restructuring quickly. The spawn contract this backend leans on
//! (`--script`, `--headless`, exit 1 on a failed load) was verified against
//! its 2026-08 tree; a `[gargantuan] binary` that predates or postdates that
//! contract fails loudly through the same guards every backend carries.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::backend::cloud::bundle::{self, BundleInput, Head, SpecEntry};
use crate::backend::runtime::{classify, is_done_framed, passthrough, Decoded};
use crate::backend::{display_rel, EventSink, SuitePlan};
use crate::error::ToolError;
use crate::report::{check_protocol_version, Event, Failure};

/// Fixed allowance for the engine to boot (headless: no GPU, no window, but
/// still SDL init and library setup) on top of the per-spec budgets.
const BOOT_ALLOWANCE: Duration = Duration::from_secs(30);

/// How many trailing stderr lines are retained for diagnosing a process that
/// dies before completing. The engine logs errors and criticals to stderr
/// (a failed `--script` load among them), so the tail usually names the
/// cause. A bound, not a log — lines still stream to the terminal live.
const STDERR_TAIL_LINES: usize = 20;

/// How long after the done marker the CLI waits for the engine to exit on
/// its own before killing it. An engine whose `ProcessService:ExitAsync`
/// works exits within milliseconds of the marker; one without the service —
/// or with the current upstream argument-index bug that makes `ExitAsync`
/// raise — runs the head's padding fallback instead and spends the full
/// grace before the kill. The cost of not needing a version probe.
const EXIT_GRACE: Duration = Duration::from_secs(3);

pub fn run(plan: &SuitePlan, on_event: &mut EventSink) -> Result<(), ToolError> {
    let exe = gargantuan_executable(plan)?;

    let entries: Vec<SpecEntry> = plan
        .specs
        .iter()
        .map(|spec| SpecEntry {
            name: display_rel(spec, &plan.root),
            path: spec.clone(),
        })
        .collect();

    // Per-spec deadline inside the engine: the studio rule (single-spec
    // budget plus fixed slack), for the studio reasons — the scheduler
    // cannot preempt a stuck spec, only abandon it at the deadline.
    let budget = plan.timeout.saturating_add(Duration::from_secs(10));
    let deadline_ms = u64::try_from(budget.as_millis().max(1)).unwrap_or(u64::MAX);

    // `place: None` always — there is no Roblox place to delegate requires
    // into, so `[place] rojo` is deliberately not consulted and every module
    // bundles. (The engine is growing its own require; revisit when it
    // settles.)
    let input = BundleInput {
        core_entry: &plan.core_entry,
        specs: &entries,
        name_filter: plan.name_filter.as_deref(),
        head: Head::Gargantuan,
        deadline_ms,
        place: None,
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
    let script_path = work_dir.join("gargantuan-run.luau");
    std::fs::write(&script_path, &built.script)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", script_path.display())))?;

    let spec_count = u32::try_from(plan.specs.len().max(1)).unwrap_or(u32::MAX);
    let overall = BOOT_ALLOWANCE.saturating_add(budget.saturating_mul(spec_count));

    let mut child = Command::new(&exe)
        .arg("--script")
        .arg(&script_path)
        .arg("--headless")
        .current_dir(&plan.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError(format!(
                    "cannot find `{}` — the engine has no releases yet, so build it from \
                     source (https://github.com/teamfireworks/gargantuan) and point \
                     `[gargantuan] binary` in lest.toml at the built executable",
                    exe.display()
                ))
            } else {
                ToolError(format!("cannot start {}: {e}", exe.display()))
            }
        })?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel::<std::io::Result<String>>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // stderr streams to the terminal as it arrives and keeps a bounded tail:
    // the engine logs errors there (a bundle that fails to load is one
    // Critical line), and quoting that line beats a bare exit status.
    let stderr = child.stderr.take().expect("stderr was piped");
    let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_reader = {
        let tail = Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                eprintln!("{line}");
                let mut tail = tail.lock().unwrap();
                if tail.len() >= STDERR_TAIL_LINES {
                    tail.remove(0);
                }
                tail.push(line);
            }
        })
    };

    let deadline = Instant::now()
        .checked_add(overall)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));

    type Readers = (std::thread::JoinHandle<()>, std::thread::JoinHandle<()>);
    let finish =
        |mut child: Child, rx: mpsc::Receiver<std::io::Result<String>>, readers: Readers| {
            let _ = child.kill();
            let _ = child.wait();
            drop(rx);
            let _ = readers.0.join();
            let _ = readers.1.join();
        };

    let mut state = StreamState::new(plan);
    let ending = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break Ending::Deadline;
        }
        match rx.recv_timeout(remaining) {
            Ok(Ok(line)) => match state.feed(&line, on_event) {
                Ok(Feed::Continue) => {}
                Ok(Feed::Done) => break Ending::Done,
                Err(err) => {
                    finish(child, rx, (reader, stderr_reader));
                    return Err(err);
                }
            },
            Ok(Err(err)) => {
                finish(child, rx, (reader, stderr_reader));
                return Err(ToolError(format!(
                    "cannot read {} output: {err}",
                    exe.display()
                )));
            }
            // recv_timeout returned early; the loop re-checks the deadline.
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break Ending::Eof,
        }
    };

    match ending {
        // The good path: the suite completed. Grace-wait for the engine to
        // exit itself (the head calls ProcessService:ExitAsync(0) after the
        // marker on engines that have it) before killing — the wait is what
        // distinguishes a modern engine from one running the padding
        // fallback, without a version probe. `finish` runs either way: its
        // kill is a no-op on an exited child, and the joins are still owed.
        // The exit status is deliberately ignored here, unlike Eof's: every
        // verdict already streamed before the marker, so a teardown crash
        // after it has nothing left to change.
        Ending::Done => {
            let grace = Instant::now() + EXIT_GRACE;
            loop {
                // Keep draining (and discarding) the channel: an engine on
                // the padding fallback prints ~1 KiB per unthrottled
                // headless frame, and an undrained channel would buffer
                // tens of megabytes across the grace.
                while rx.try_recv().is_ok() {}
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() >= grace => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                    Err(_) => break,
                }
            }
            finish(child, rx, (reader, stderr_reader));
        }
        // The budget expired. Which kind of failure that is depends on
        // whether the protocol ever started: the engine cannot exit on its
        // own, so a bundle that fails to load (compile error, a raise at
        // top level) is logged to stderr and then the engine steps frames
        // *silently forever* — from out here that is indistinguishable from
        // a hang except by the absence of protocol traffic. No protocol at
        // all means the run never happened: a tool error (exit 2), with the
        // stderr tail that names the cause. Protocol followed by silence
        // means something under test hung: a test failure (exit 1),
        // matching every other backend's budget expiry.
        Ending::Deadline => {
            finish(child, rx, (reader, stderr_reader));
            if !state.saw_protocol {
                let tail = stderr_tail.lock().unwrap().join("\n  ");
                let tail = if tail.is_empty() {
                    String::new()
                } else {
                    format!("\n  {tail}")
                };
                return Err(ToolError(format!(
                    "gargantuan never spoke the protocol within suite \"{}\"'s budget ({}s) — \
                     the bundle likely failed to load (the engine cannot exit on its own, so a \
                     load failure looks like silence); bundle kept at {}{tail}",
                    plan.name,
                    overall.as_secs(),
                    script_path.display()
                )));
            }
            let spec = state.current_spec.map(|i| plan.specs[i].as_path());
            let path = spec
                .map(|p| display_rel(p, &plan.root))
                .unwrap_or_else(|| plan.name.clone());
            let event = Event::TestFail {
                path: vec![path],
                name: "(timeout)".to_string(),
                duration_ms: overall.as_millis() as f64,
                failure: Failure::Error {
                    message: format!(
                        "gargantuan exceeded suite \"{}\"'s budget ({}s) and was killed — a \
                         hung test, or an engine that stopped stepping scripts",
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
        // The process exited before the done marker — the one ending the
        // engine can produce on its own, and it always means failure: a
        // launch that died at boot, a bundle the engine could not load
        // (exit 1 with a Critical stderr line), or a crash mid-suite.
        Ending::Eof => {
            let _ = reader.join();
            let status = wait_bounded(&mut child, deadline);
            let _ = stderr_reader.join();
            let status_text = status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "killed".into());
            let tail = stderr_tail.lock().unwrap().join("\n  ");
            let tail = if tail.is_empty() {
                String::new()
            } else {
                format!("\n  {tail}")
            };
            if state.outcomes == 0 {
                if state.saw_protocol {
                    return Err(ToolError(format!(
                        "gargantuan ({status_text}) died mid-suite before any test finished \
                         in \"{}\" — bundle kept at {}{tail}",
                        plan.name,
                        script_path.display()
                    )));
                }
                return Err(ToolError(format!(
                    "gargantuan exited ({status_text}) without running suite \"{}\" — the \
                     launch or the bundle load failed; bundle kept at {}{tail}",
                    plan.name,
                    script_path.display()
                )));
            }
            // Partial results then death: report it against the spec that
            // was running, keep what streamed.
            let spec = state.current_spec.map(|i| plan.specs[i].as_path());
            let path = spec
                .map(|p| display_rel(p, &plan.root))
                .unwrap_or_else(|| plan.name.clone());
            let event = Event::TestFail {
                path: vec![path],
                name: "(aborted)".to_string(),
                duration_ms: 0.0,
                failure: Failure::Error {
                    message: format!(
                        "gargantuan exited ({status_text}) before suite \"{}\" finished — \
                         bundle kept at {}{tail}",
                        plan.name,
                        script_path.display()
                    ),
                    trace: None,
                },
                origin: None,
            };
            on_event(spec, &event);
            return Ok(());
        }
    }

    // The same false-green guard every backend carries, disarmed under a
    // name filter (which legitimately selects zero tests).
    if state.outcomes == 0 && !plan.specs.is_empty() && plan.name_filter.is_none() {
        return Err(ToolError(format!(
            "gargantuan ran {} spec file(s) for suite \"{}\" but produced no test outcomes — \
             bundle kept at {}",
            plan.specs.len(),
            plan.name,
            script_path.display()
        )));
    }

    // Success: the generated bundle is noise now.
    let _ = std::fs::remove_file(&script_path);
    Ok(())
}

/// How the stream loop ended.
enum Ending {
    /// The done marker arrived: the suite completed.
    Done,
    /// The overall budget expired first.
    Deadline,
    /// stdout closed before the done marker: the process died.
    Eof,
}

/// Waits for the child, bounded by the run's own deadline — an engine that
/// closed stdout but wedged instead of exiting must not hang the CLI.
fn wait_bounded(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return None,
        }
    }
}

/// What one fed line meant for the run.
#[derive(Debug, PartialEq, Eq)]
enum Feed {
    Continue,
    /// The done marker: stop reading and kill the engine.
    Done,
}

/// Decodes live stdout lines into protocol events, tracking the state the
/// run's ending needs. Split from `run` so the rules — boundary mapping,
/// done-marker completion, chatter suppression, outcome counting — are
/// testable without an engine binary on the machine.
struct StreamState<'p> {
    plan: &'p SuitePlan,
    outcomes: usize,
    saw_protocol: bool,
    current_spec: Option<usize>,
}

impl<'p> StreamState<'p> {
    fn new(plan: &'p SuitePlan) -> Self {
        StreamState {
            plan,
            outcomes: 0,
            saw_protocol: false,
            current_spec: None,
        }
    }

    fn feed(&mut self, line: &str, on_event: &mut EventSink) -> Result<Feed, ToolError> {
        if is_done_framed(line) {
            return Ok(Feed::Done);
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
                        Ok(Feed::Continue)
                    }
                    None => Err(ToolError(format!(
                        "gargantuan sent the spec-boundary marker \"{raw}\", which is not a \
                         1-based index into suite \"{}\"'s {} spec file(s) — the bundle and \
                         the CLI disagree about the spec list",
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
                        "undecodable protocol line from gargantuan while running suite \
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
                            "framework/CLI protocol mismatch from gargantuan: {mismatch}"
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
                Ok(Feed::Continue)
            }
            Decoded::Output => {
                // The engine's own boot chatter logs to stdout before the
                // suite runs (`Gargantuan[Info] …`); the first boundary
                // marker is where test output becomes possible. Echo only
                // from there, like the studio decoder does.
                if self.saw_protocol {
                    println!("{line}");
                }
                Ok(Feed::Continue)
            }
        }
    }
}

/// Resolves the engine binary: the `[gargantuan] binary` path when set (and
/// checked to exist, so a typo fails as config rather than as a spawn), or
/// the bare name for a PATH lookup.
fn gargantuan_executable(plan: &SuitePlan) -> Result<PathBuf, ToolError> {
    match &plan.gargantuan_binary {
        Some(path) => {
            if path.is_file() {
                Ok(path.clone())
            } else {
                Err(ToolError(format!(
                    "the configured [gargantuan] binary does not exist: {}",
                    path.display()
                )))
            }
        }
        None => Ok(PathBuf::from("gargantuan")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn plan() -> SuitePlan {
        SuitePlan {
            name: "engine-gg".into(),
            specs: vec![PathBuf::from("a.spec.luau"), PathBuf::from("b.spec.luau")],
            root: PathBuf::from("."),
            core_entry: PathBuf::from("core/init.luau"),
            timeout: Duration::from_secs(5),
            workers: 0,
            name_filter: None,
            coverage: false,
            rojo_project: None,
            studio_executable: None,
            gargantuan_binary: None,
        }
    }

    /// One recorded sink call: the spec attribution and an event kind tag.
    type Seen = (Option<PathBuf>, &'static str);

    /// Feeds lines and records (spec attribution, event kind tag) pairs plus
    /// the final feed result.
    fn feed_all(state: &mut StreamState, lines: &[&str]) -> Result<(Vec<Seen>, Feed), ToolError> {
        let mut seen = Vec::new();
        let mut last = Feed::Continue;
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
            last = state.feed(line, &mut sink)?;
        }
        Ok((seen, last))
    }

    #[test]
    fn boundaries_attribute_events_and_done_stops_the_stream() {
        let plan = plan();
        let mut state = StreamState::new(&plan);
        let (seen, last) = feed_all(
            &mut state,
            &[
                "Gargantuan[Info] Constructed engine",
                "@@LEST_SPEC@@1",
                r#"@@LEST@@{"kind":"test_pass","path":[],"name":"a","durationMs":1}"#,
                "@@LEST_SPEC@@2",
                r#"@@LEST@@{"kind":"test_fail","path":[],"name":"b","durationMs":1,"failure":{"type":"error","message":"x"}}"#,
                "@@LEST_STUDIO_DONE@@",
            ],
        )
        .expect("feed");
        assert_eq!(
            seen,
            vec![
                (Some(PathBuf::from("a.spec.luau")), "test_pass"),
                (Some(PathBuf::from("b.spec.luau")), "test_fail"),
            ]
        );
        assert_eq!(last, Feed::Done);
        assert_eq!(state.outcomes, 2);
        assert!(state.saw_protocol);
    }

    #[test]
    fn a_done_marker_inside_a_payload_is_not_completion() {
        let plan = plan();
        let mut state = StreamState::new(&plan);
        let (seen, last) = feed_all(
            &mut state,
            &[r#"@@LEST@@{"kind":"test_fail","path":[],"name":"has @@LEST_STUDIO_DONE@@ inside","durationMs":1,"failure":{"type":"error","message":"x"}}"#],
        )
        .expect("feed");
        assert_eq!(seen.len(), 1);
        assert_eq!(last, Feed::Continue);
    }

    #[test]
    fn a_bad_boundary_is_a_tool_error() {
        let plan = plan();
        let mut state = StreamState::new(&plan);
        let err = feed_all(&mut state, &["@@LEST_SPEC@@9"]).expect_err("must fail");
        assert!(err.to_string().contains("spec-boundary marker"));
    }

    #[test]
    fn undecodable_json_and_protocol_mismatch_abort() {
        let plan = plan();
        let mut state = StreamState::new(&plan);
        let err = feed_all(&mut state, &["@@LEST@@{not json"]).expect_err("must fail");
        assert!(err.to_string().contains("undecodable protocol line"));

        let mut state = StreamState::new(&plan);
        let err = feed_all(
            &mut state,
            &[r#"@@LEST@@{"kind":"run_start","specCount":1,"protocolVersion":99}"#],
        )
        .expect_err("must fail");
        assert!(err.to_string().contains("protocol mismatch"));
    }

    #[test]
    fn a_configured_binary_that_does_not_exist_is_a_config_error() {
        let mut plan = plan();
        plan.gargantuan_binary = Some(PathBuf::from("definitely/not/here/gargantuan.exe"));
        let err = gargantuan_executable(&plan).expect_err("must fail");
        assert!(err.to_string().contains("does not exist"));

        let unset = self::plan();
        assert_eq!(
            gargantuan_executable(&unset).expect("PATH name"),
            PathBuf::from("gargantuan")
        );
    }
}
