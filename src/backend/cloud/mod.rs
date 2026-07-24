//! The cloud backend: run engine code in a real Roblox place via Open Cloud.
//!
//! For code that touches real engine APIs (Instances, services, `task`), there
//! is no faking it — it runs in the engine. The CLI bundles lest/core and the
//! suite's specs (from the project) together with its own compiled-in in-engine
//! runtime (see [`bundle`]) into one self-contained Luau script, submits it as
//! an Open Cloud Luau-execution task, polls to completion, and decodes the
//! returned protocol events into the same results bus every other backend
//! feeds. Nothing beyond lest/core need be installed to run engine tests.
//!
//! Attribution: cloud runs **one task per spec file**, so each spec's events
//! arrive already isolated and the sink can attribute them (and their
//! snapshots) to that spec's path — the same per-file attribution native gets
//! for free — at the cost of one round trip per spec. Cloud is opt-in locally,
//! auto-enabled in CI, and always ignored by watch mode, so the extra latency
//! never touches the fast local loop.

pub mod api;
pub mod bundle;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::report::{check_protocol_version, Event, Failure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::{display_rel, EventSink, SuitePlan};
use crate::config::CloudTarget;
use crate::error::ToolError;

use api::{Session, Transport, UreqTransport};
use bundle::{BundleInput, Head, SpecEntry};

/// Environment variables holding the Open Cloud API key, in priority order.
/// The key is a secret: read only from the environment, never config or logs.
const API_KEY_VARS: [&str; 2] = ["ROBLOX_API_KEY", "LEST_API_KEY"];

/// Runs a cloud-backend suite. Reads the API key from the environment,
/// validates the target ids from config, and executes each spec as its own
/// Open Cloud task, streaming decoded events into `on_event`.
pub fn run(
    plan: &SuitePlan,
    target: &CloudTarget,
    on_event: &mut EventSink,
) -> Result<(), ToolError> {
    let api_key = api_key()?;
    let (universe_id, place_id) = resolve_target(plan, target)?;
    let place_file = target.place_file.as_ref().map(|file| plan.root.join(file));
    let transport = UreqTransport::new();
    run_with_transport(
        plan,
        &transport,
        &api_key,
        &universe_id,
        &place_id,
        place_file.as_deref(),
        on_event,
    )
}

/// Core orchestration, generic over the transport so it is exercised without a
/// network in tests (see the mock in `api`).
fn run_with_transport<T: Transport>(
    plan: &SuitePlan,
    transport: &T,
    api_key: &str,
    universe_id: &str,
    place_id: &str,
    place_file: Option<&Path>,
    on_event: &mut EventSink,
) -> Result<(), ToolError> {
    let mut session = Session::new(transport, api_key, universe_id, place_id);

    // `plan.timeout` is a *per-test* budget, but the in-engine scheduler
    // deadline governs a whole spec file — the engine has no preemption, so
    // there is no per-test granularity to hand it. Unlike `backend::runtime`,
    // where one process runs all N specs and the budget scales by N, each
    // cloud task runs exactly ONE spec file (that is what buys per-spec
    // attribution) — so the deadline here is the single-spec budget plus
    // generous fixed slack. Scaling it by the suite's spec count let
    // worst-case wall time grow ~N²·timeout: N tasks, each allowed an
    // N-scaled deadline. Saturating, because `timeout_ms` is unvalidated user
    // config and `Duration`'s `Add` panics on overflow — exit 101 would
    // bypass the exit-code policy entirely.
    let budget = plan.timeout.saturating_add(Duration::from_secs(10));
    let deadline_ms = u64::try_from(budget.as_millis().max(1)).unwrap_or(u64::MAX);
    // The overall HTTP deadline covers the in-engine budget plus queueing and
    // network on top of it.
    let overall = budget.saturating_add(Duration::from_secs(120));

    // The upload comes before anything runs, and every task pins the version
    // it produced (or the one already stamped): a run is only honest about
    // the place it claims to test if the place actually holds that content.
    if let Some(file) = place_file {
        // Guarded like every other `Instant + Duration` on unvalidated config.
        let deadline = Instant::now()
            .checked_add(overall)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        let version =
            ensure_place_version(&session, file, &plan.root, universe_id, place_id, deadline)?;
        session.pin_place_version(version);
    }

    // `[settings] rojo`, consumed: string requires whose targets the project
    // file maps into the place delegate to the engine's require instead of
    // silently bundling a private copy. Parsed once per suite; a missing or
    // malformed project file is a tool error, not a silent fall-back to
    // bundling — a config that names a project expects it honored.
    let place_map = match &plan.rojo_project {
        Some(project) => Some(
            crate::resolve::VirtualDataModel::from_project_file(project)
                .map_err(|e| ToolError(e.to_string()))?,
        ),
        None => None,
    };

    let mut outcomes = 0usize;
    // One task per spec file means lest/core is emitted into every bundle;
    // sharing the cache keeps that from re-reading the framework off disk once
    // per spec.
    let mut sources = bundle::SourceCache::default();
    // …and shared modules re-report the same unresolvable require once per
    // bundle, so warnings are deduplicated across the suite.
    let mut warned: HashSet<bundle::UnresolvedRequire> = HashSet::new();

    for spec in &plan.specs {
        let name = display_rel(spec, &plan.root);
        let specs = [SpecEntry {
            name: name.clone(),
            path: spec.clone(),
        }];
        let input = BundleInput {
            core_entry: &plan.core_entry,
            specs: &specs,
            name_filter: plan.name_filter.as_deref(),
            head: Head::Cloud,
            deadline_ms,
            place: place_map.as_ref(),
        };
        let bundle::Bundle {
            script,
            unresolved,
            source_map,
        } = bundle::bundle_with_cache(&input, &mut sources)?;
        for miss in unresolved {
            if warned.insert(miss.clone()) {
                crate::report::warn_to_stderr(&unresolved_warning(&miss, &plan.root));
            }
        }

        let results = session.run_script(&script, overall)?;
        let events = extract_events(&results).ok_or_else(|| {
            ToolError(format!(
                "the cloud task for spec {name} returned no event array in output.results[0]"
            ))
        })?;

        for value in events {
            // Prefixless, context-in-sentence — one protocol error, one
            // spelling across backends (mirrors `backend::runtime`).
            let mut event: Event = serde_json::from_value(value).map_err(|e| {
                ToolError(format!(
                    "undecodable protocol event from the cloud task for spec {name}: {e}"
                ))
            })?;
            if let Event::RunStart {
                protocol_version, ..
            } = event
            {
                check_protocol_version(protocol_version).map_err(|mismatch| {
                    ToolError(format!(
                        "framework/CLI protocol mismatch in {name}: {mismatch}"
                    ))
                })?;
            }
            // Engine error positions arrive as bundle coordinates — this
            // bundle's, so the rewrite uses the map built alongside it.
            if let Event::TestFail { failure, .. } = &mut event {
                remap_failure(failure, &source_map, &plan.root);
            }
            if matches!(
                event,
                Event::TestPass { .. } | Event::TestFail { .. } | Event::TestSkip { .. }
            ) {
                outcomes += 1;
            }
            on_event(Some(spec), &event);
        }
    }

    // A run that produced spec files but not one outcome means the bundle
    // loaded nothing runnable — a false green we refuse, matching the native
    // and spawned-runtime guards. A name filter is the one benign way to reach
    // zero outcomes, so it disarms the guard and the CLI reports the no-match
    // case itself.
    if !plan.specs.is_empty() && outcomes == 0 && plan.name_filter.is_none() {
        return Err(ToolError(format!(
            "cloud suite \"{}\" ran {} spec file(s) but produced no test outcomes — the bundle \
             likely failed to load lest/core or the specs",
            plan.name,
            plan.specs.len()
        )));
    }
    Ok(())
}

/// `output.results[0]` is the events array our entrypoint returned.
fn extract_events(results: &[Value]) -> Option<Vec<Value>> {
    results.first().and_then(Value::as_array).cloned()
}

/// The warning body for a string require the bundler could not resolve: the
/// real source position that the eventual in-engine error — a bundle
/// coordinate — cannot carry. A warning rather than an error because the
/// require may be legal dead code in the engine; the shim still errs loudly
/// if it is reached.
pub(crate) fn unresolved_warning(miss: &bundle::UnresolvedRequire, root: &Path) -> String {
    format!(
        "the string require of '{}' at {}:{} does not resolve ({}) and is not bundled — it \
         will error if reached in the engine",
        miss.spec,
        display_rel(&miss.file, root),
        miss.line,
        miss.reason
    )
}

/// The `.lest/place-versions.json` stamp: for each `universe/place` target,
/// the content hash of the last place file uploaded there and the version it
/// saved as. A matching hash skips the upload and pins to the recorded
/// version; a missing or unreadable stamp merely costs one redundant upload.
#[derive(Default, Serialize, Deserialize)]
struct UploadStamps {
    #[serde(flatten)]
    targets: HashMap<String, UploadStamp>,
}

#[derive(Serialize, Deserialize)]
struct UploadStamp {
    hash: String,
    version: u64,
}

/// The place version the suite's tasks should pin: the stamped one when the
/// file's content hash is unchanged, otherwise a fresh upload's. Progress goes
/// to stderr in the note voice — an upload can take a while, and a run
/// pinned to a version the reader cannot see invites "which place did this
/// even test?".
fn ensure_place_version<T: Transport>(
    session: &Session<T>,
    place_file: &Path,
    root: &Path,
    universe_id: &str,
    place_id: &str,
    deadline: Instant,
) -> Result<u64, ToolError> {
    let bytes = std::fs::read(place_file).map_err(|e| {
        ToolError(format!(
            "cannot read the place file {}: {e}",
            place_file.display()
        ))
    })?;
    let hash = format!("{:016x}", crate::resolve::hash_bytes(&bytes));
    let stamp_path = root.join(".lest").join("place-versions.json");
    let key = format!("{universe_id}/{place_id}");

    let mut stamps: UploadStamps = std::fs::read_to_string(&stamp_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    if let Some(stamp) = stamps.targets.get(&key) {
        if stamp.hash == hash {
            crate::report::note_to_stderr(&format!(
                "place file unchanged — tasks pinned to version {}",
                stamp.version
            ));
            return Ok(stamp.version);
        }
    }

    // rojo builds XML places as `.rbxlx`; everything else is the binary form.
    let content_type = match place_file.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("rbxlx") => "application/xml",
        _ => "application/octet-stream",
    };
    crate::report::note_to_stderr(&format!(
        "uploading {} as a new place version",
        display_rel(place_file, root)
    ));
    let version = session.upload_place(bytes, content_type, deadline)?;
    crate::report::note_to_stderr(&format!(
        "saved place version {version} — tasks pinned to it"
    ));

    stamps.targets.insert(key, UploadStamp { hash, version });
    let write = std::fs::create_dir_all(root.join(".lest")).and_then(|()| {
        // Unwrap is safe: UploadStamps is a string-keyed map of plain fields.
        std::fs::write(&stamp_path, serde_json::to_string_pretty(&stamps).unwrap())
    });
    if let Err(e) = write {
        // The upload itself succeeded; failing the run over bookkeeping would
        // be backwards. But a silently unwritten stamp re-uploads every run,
        // which reads as "the skip is broken" — so say why.
        crate::report::warn_to_stderr(&format!(
            "cannot record the uploaded place version in {}: {e} — the next run will upload again",
            stamp_path.display()
        ));
    }
    Ok(version)
}

/// Rewrites every bundle coordinate in a failure's text fields to the disk
/// file and line it came from. Assertion messages come from matchers rather
/// than the engine, but a caught engine error quoted into one still carries
/// coordinates worth translating — and the rewrite is a no-op on text
/// without any.
fn remap_failure(failure: &mut Failure, map: &bundle::SourceMap, root: &Path) {
    match failure {
        Failure::Assertion { message, .. } => {
            *message = remap_bundle_coordinates(message, map, root);
        }
        Failure::Error { message, trace } => {
            *message = remap_bundle_coordinates(message, map, root);
            if let Some(trace) = trace {
                *trace = remap_bundle_coordinates(trace, map, root);
            }
        }
        // Never decoded from a task: the host synthesizes this variant after
        // comparison, and a snapshot diff carries no bundle coordinates.
        Failure::Snapshot { .. } => {}
    }
}

/// The chunk name Open Cloud's Luau execution gives the submitted script,
/// and therefore the name every engine error position carries.
const BUNDLE_CHUNK: &str = "TaskScript:";

/// Replaces each `TaskScript:<line>` in `text` with the module file
/// (root-relative) and line it maps to — `tests/engine/foo.spec.luau:12`
/// instead of `TaskScript:1598`. A coordinate that falls in bundler
/// scaffolding (the prelude, a require shim, the entrypoint) has no source
/// line and is left as it arrived, as is one that is not a coordinate at all.
fn remap_bundle_coordinates(text: &str, map: &bundle::SourceMap, root: &Path) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(found) = rest.find(BUNDLE_CHUNK) {
        result.push_str(&rest[..found]);
        let after = &rest[found + BUNDLE_CHUNK.len()..];
        let digits = &after[..after.bytes().take_while(u8::is_ascii_digit).count()];
        let mapped = digits
            .parse::<usize>()
            .ok()
            .and_then(|line| map.resolve(line));
        match mapped {
            Some((span, line)) => {
                let origin = match &span.file {
                    Some(file) => display_rel(file, root),
                    None => span.label.clone(),
                };
                result.push_str(&format!("{origin}:{line}"));
            }
            None => {
                result.push_str(BUNDLE_CHUNK);
                result.push_str(digits);
            }
        }
        rest = &after[digits.len()..];
    }
    result.push_str(rest);
    result
}

/// Reads the Open Cloud API key from the environment. A missing key for a cloud
/// run is a clear tool error with setup guidance — never a silent skip.
fn api_key() -> Result<String, ToolError> {
    for var in API_KEY_VARS {
        if let Ok(value) = std::env::var(var) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }
    Err(ToolError(format!(
        "cloud backend needs an Open Cloud API key — set {} in your environment (or a .env file \
         at the project root). Create one at https://create.roblox.com/dashboard/credentials with \
         the universe-places Luau-execution scope. The key is read from the environment only; it \
         never belongs in lest.toml.",
        API_KEY_VARS.join(" or ")
    )))
}

/// Resolves the universe/place ids for a cloud suite, or a clear tool error
/// naming exactly what is missing.
fn resolve_target(plan: &SuitePlan, target: &CloudTarget) -> Result<(String, String), ToolError> {
    let missing = |field: &str| {
        ToolError(format!(
            "cloud suite \"{}\" is missing `{field}` — add it under [cloud] in lest.toml (or on \
             the suite as [suites.{}.cloud]). These are non-secret Roblox ids; find them in the \
             Creator Dashboard URL for your experience and place.",
            plan.name, plan.name
        ))
    };
    let universe = target
        .universe_id
        .clone()
        .ok_or_else(|| missing("universe_id"))?;
    let place = target.place_id.clone().ok_or_else(|| missing("place_id"))?;
    Ok((universe, place))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use api::{HttpRequest, HttpResponse, Method};

    /// The crate is the repo, so the manifest directory *is* the root.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Canned transport returning a COMPLETE task whose results echo a small,
    /// valid event stream — enough to drive the decode/attribution path.
    struct FakeCloud {
        events_json: String,
        seen: RefCell<usize>,
    }

    impl Transport for FakeCloud {
        fn send(&self, request: HttpRequest, _key: &str) -> Result<HttpResponse, ToolError> {
            *self.seen.borrow_mut() += 1;
            let body = match request.method {
                Method::Post => {
                    r#"{"path":"universes/1/places/2/tasks/abc","state":"QUEUED"}"#.to_string()
                }
                Method::Get => format!(
                    r#"{{"path":"universes/1/places/2/tasks/abc","state":"COMPLETE",
                        "output":{{"results":[{}]}}}}"#,
                    self.events_json
                ),
            };
            Ok(HttpResponse::new(200, body))
        }
    }

    fn plan_for(root: &Path, spec: PathBuf) -> SuitePlan {
        SuitePlan {
            name: "engine".to_string(),
            specs: vec![spec],
            root: root.to_path_buf(),
            core_entry: crate::resolve::normalize(&root.join("luau/core/init.luau")),
            timeout: Duration::from_secs(5),
            workers: 0,
            name_filter: None,
            coverage: false,
            rojo_project: None,
            studio_executable: None,
        }
    }

    #[test]
    fn decodes_events_and_attributes_to_spec() {
        let root = repo_root();
        let spec = root.join("tests/core/expect.spec.luau");
        let plan = plan_for(&root, spec.clone());

        // A minimal, valid event stream: run_start (with matching protocol
        // version), one pass, run_end.
        let events_json = r#"[
            {"kind":"run_start","specCount":1,"protocolVersion":1},
            {"kind":"test_pass","path":["math"],"name":"adds","durationMs":0.1},
            {"kind":"run_end","passed":1,"failed":0,"skipped":0}
        ]"#;
        let transport = FakeCloud {
            events_json: events_json.to_string(),
            seen: RefCell::new(0),
        };

        let mut seen_paths: Vec<Option<PathBuf>> = Vec::new();
        let mut passes = 0;
        {
            let mut sink = |p: Option<&Path>, e: &Event| {
                seen_paths.push(p.map(Path::to_path_buf));
                if matches!(e, Event::TestPass { .. }) {
                    passes += 1;
                }
            };
            run_with_transport(&plan, &transport, "key", "1", "2", None, &mut sink).unwrap();
        }
        assert_eq!(passes, 1);
        // Every event was attributed to the spec file.
        assert!(seen_paths
            .iter()
            .all(|p| p.as_deref() == Some(spec.as_path())));
    }

    #[test]
    fn refuses_a_false_green_with_no_outcomes() {
        let root = repo_root();
        let spec = root.join("tests/core/expect.spec.luau");
        let plan = plan_for(&root, spec);
        // Only framing, no test outcomes.
        let events_json = r#"[
            {"kind":"run_start","specCount":0,"protocolVersion":1},
            {"kind":"run_end","passed":0,"failed":0,"skipped":0}
        ]"#;
        let transport = FakeCloud {
            events_json: events_json.to_string(),
            seen: RefCell::new(0),
        };
        let mut sink = |_: Option<&Path>, _: &Event| {};
        let err =
            run_with_transport(&plan, &transport, "key", "1", "2", None, &mut sink).unwrap_err();
        assert!(err.to_string().contains("no test outcomes"), "{err}");
    }

    #[test]
    fn missing_target_is_a_clear_error() {
        let root = repo_root();
        let plan = plan_for(&root, root.join("tests/core/expect.spec.luau"));
        let err = resolve_target(&plan, &CloudTarget::default()).unwrap_err();
        assert!(err.to_string().contains("universe_id"), "{err}");
    }

    /// The field report's headline complaint: engine failures pointed at
    /// `TaskScript:1598`, meaningless without reverse-engineering the bundle.
    /// A decoded failure must name the disk file and line instead — and leave
    /// coordinates in bundler scaffolding untouched.
    #[test]
    fn engine_failure_coordinates_are_rewritten_to_disk_positions() {
        let root = repo_root();
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("boom.spec.luau");
        std::fs::write(
            &spec,
            "--!strict\nlocal function explode ()\n\terror('boom')\nend\nexplode()\nreturn nil\n",
        )
        .unwrap();
        let plan = plan_for(&root, spec.clone());

        // Bundle the same modules the runner will (the entrypoint tail cannot
        // shift them) to learn which bundle line the spec's line 3 — the
        // `error` call — landed on.
        let specs = [bundle::SpecEntry {
            name: display_rel(&spec, &root),
            path: spec.clone(),
        }];
        let input = bundle::BundleInput {
            core_entry: &plan.core_entry,
            specs: &specs,
            name_filter: None,
            head: bundle::Head::Cloud,
            deadline_ms: 1,
            place: None,
        };
        let built = bundle::bundle(&input).unwrap();
        let spec_key = crate::resolve::normalize(&spec);
        let coordinate = (1..=built.script.lines().count())
            .find(|&line| {
                matches!(
                    built.source_map.resolve(line),
                    Some((span, 3)) if span.file.as_deref() == Some(spec_key.as_path())
                )
            })
            .expect("the spec's line 3 must be somewhere in the bundle");

        let events_json = format!(
            r#"[
            {{"kind":"run_start","specCount":1,"protocolVersion":1}},
            {{"kind":"test_fail","path":["boom"],"name":"explodes","durationMs":0.1,
              "failure":{{"type":"error","message":"TaskScript:{coordinate}: boom",
                          "trace":"TaskScript:{coordinate} function explode\nTaskScript:1 "}}}},
            {{"kind":"run_end","passed":0,"failed":1,"skipped":0}}
        ]"#
        );
        let transport = FakeCloud {
            events_json,
            seen: RefCell::new(0),
        };

        let mut failures: Vec<(String, Option<String>)> = Vec::new();
        {
            let mut sink = |_: Option<&Path>, e: &Event| {
                if let Event::TestFail {
                    failure: Failure::Error { message, trace },
                    ..
                } = e
                {
                    failures.push((message.clone(), trace.clone()));
                }
            };
            run_with_transport(&plan, &transport, "key", "1", "2", None, &mut sink).unwrap();
        }
        assert_eq!(failures.len(), 1);
        let (message, trace) = &failures[0];
        assert!(message.contains("boom.spec.luau:3: boom"), "{message}");
        let trace = trace.as_deref().unwrap();
        assert!(
            trace.contains("boom.spec.luau:3 function explode"),
            "{trace}"
        );
        // Line 1 is the prelude comment — scaffolding, left as it arrived.
        assert!(trace.contains("TaskScript:1 "), "{trace}");
    }

    /// Routes by URL instead of a canned sequence: uploads (the universes/v1
    /// family) return a version number, task posts return QUEUED, polls return
    /// COMPLETE with one passing outcome. JSON bodies are kept so a test can
    /// inspect the submitted script.
    struct RoutedCloud {
        seen: RefCell<Vec<(Method, String)>>,
        bodies: RefCell<Vec<String>>,
    }

    impl Transport for RoutedCloud {
        fn send(&self, request: HttpRequest, _key: &str) -> Result<HttpResponse, ToolError> {
            self.seen
                .borrow_mut()
                .push((request.method, request.url.clone()));
            if let Some(api::RequestBody::Json(body)) = &request.body {
                self.bodies.borrow_mut().push(body.clone());
            }
            let body = if request.url.contains("/universes/v1/") {
                r#"{"versionNumber":41}"#.to_string()
            } else {
                match request.method {
                    Method::Post => {
                        r#"{"path":"universes/1/places/2/tasks/abc","state":"QUEUED"}"#.to_string()
                    }
                    Method::Get => r#"{"path":"universes/1/places/2/tasks/abc","state":"COMPLETE",
                        "output":{"results":[[
                            {"kind":"run_start","specCount":1,"protocolVersion":1},
                            {"kind":"test_pass","path":["m"],"name":"t","durationMs":0.1},
                            {"kind":"run_end","passed":1,"failed":0,"skipped":0}]]}}"#
                        .to_string(),
                }
            };
            Ok(HttpResponse::new(200, body))
        }
    }

    impl RoutedCloud {
        fn new() -> Self {
            RoutedCloud {
                seen: RefCell::new(Vec::new()),
                bodies: RefCell::new(Vec::new()),
            }
        }

        fn upload_count(&self) -> usize {
            self.seen
                .borrow()
                .iter()
                .filter(|(_, url)| url.contains("/universes/v1/"))
                .count()
        }

        fn first_task_submit(&self) -> String {
            self.seen
                .borrow()
                .iter()
                .find(|(method, url)| *method == Method::Post && !url.contains("/universes/v1/"))
                .map(|(_, url)| url.clone())
                .expect("a task must have been submitted")
        }
    }

    /// The stale-place footgun, end to end: the first run uploads and pins,
    /// an unchanged file skips the upload but still pins the stamped version,
    /// and edited content uploads again.
    #[test]
    fn place_file_uploads_once_skips_unchanged_and_pins_tasks() {
        let repo = repo_root();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let spec = root.join("engine.spec.luau");
        std::fs::write(&spec, "--!strict\nreturn nil\n").unwrap();
        let place = root.join("test-place.rbxl");
        std::fs::write(&place, b"binary place v1").unwrap();
        let plan = SuitePlan {
            name: "engine".to_string(),
            specs: vec![spec],
            root: root.clone(),
            core_entry: crate::resolve::normalize(&repo.join("luau/core/init.luau")),
            timeout: Duration::from_secs(5),
            workers: 0,
            name_filter: None,
            coverage: false,
            rojo_project: None,
            studio_executable: None,
        };
        let mut sink = |_: Option<&Path>, _: &Event| {};

        let first = RoutedCloud::new();
        run_with_transport(&plan, &first, "key", "1", "2", Some(&place), &mut sink).unwrap();
        assert_eq!(first.upload_count(), 1);
        assert!(
            first
                .first_task_submit()
                .contains("/places/2/versions/41/luau-execution-session-tasks"),
            "{}",
            first.first_task_submit()
        );
        assert!(root.join(".lest/place-versions.json").is_file());

        // Same bytes: the stamp short-circuits the upload, the pin remains.
        let second = RoutedCloud::new();
        run_with_transport(&plan, &second, "key", "1", "2", Some(&place), &mut sink).unwrap();
        assert_eq!(second.upload_count(), 0);
        assert!(second
            .first_task_submit()
            .contains("/places/2/versions/41/"));

        // Edited bytes: the hash misses and the upload happens again.
        std::fs::write(&place, b"binary place v2").unwrap();
        let third = RoutedCloud::new();
        run_with_transport(&plan, &third, "key", "1", "2", Some(&place), &mut sink).unwrap();
        assert_eq!(third.upload_count(), 1);
    }

    /// `[settings] rojo` travels config → plan → bundler: the script actually
    /// submitted to Open Cloud carries the delegation for a mapped require.
    #[test]
    fn rojo_project_in_the_plan_reaches_the_bundler() {
        let repo = repo_root();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("fixtures")).unwrap();
        std::fs::write(
            root.join("engine.spec.luau"),
            "--!strict\nlocal R = require('./fixtures/recorder')\nreturn nil\n",
        )
        .unwrap();
        std::fs::write(root.join("fixtures/recorder.luau"), "return {}\n").unwrap();
        std::fs::write(
            root.join("default.project.json"),
            r#"{"name":"t","tree":{"$className":"DataModel",
                "ServerStorage":{"ChiefTests":{"$path":"fixtures"}}}}"#,
        )
        .unwrap();
        let plan = SuitePlan {
            name: "engine".to_string(),
            specs: vec![root.join("engine.spec.luau")],
            root: root.clone(),
            core_entry: crate::resolve::normalize(&repo.join("luau/core/init.luau")),
            timeout: Duration::from_secs(5),
            workers: 0,
            name_filter: None,
            coverage: false,
            rojo_project: Some(root.join("default.project.json")),
            studio_executable: None,
        };
        let transport = RoutedCloud::new();
        let mut sink = |_: Option<&Path>, _: &Event| {};
        run_with_transport(&plan, &transport, "key", "1", "2", None, &mut sink).unwrap();
        let submitted = transport.bodies.borrow().join("\n");
        assert!(
            submitted.contains("'ServerStorage', 'ChiefTests', 'recorder'"),
            "the delegation table must reach the submitted script"
        );
    }

    /// The warning carries what the eventual in-engine error cannot: the
    /// requiring file (root-relative) and the call-site line.
    #[test]
    fn unresolved_warning_names_file_line_spec_and_reason() {
        let root = repo_root();
        let miss = bundle::UnresolvedRequire {
            file: crate::resolve::normalize(&root.join("tests/engine/foo.spec.luau")),
            line: 12,
            spec: "src".to_string(),
            reason: "no matching file on disk".to_string(),
        };
        let body = unresolved_warning(&miss, &root);
        assert!(body.contains("'src'"), "{body}");
        assert!(body.contains("foo.spec.luau:12"), "{body}");
        assert!(body.contains("no matching file on disk"), "{body}");
        assert!(
            body.contains("will error if reached in the engine"),
            "{body}"
        );
    }
}
