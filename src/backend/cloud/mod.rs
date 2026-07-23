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

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use crate::report::{check_protocol_version, Event};
use serde_json::Value;

use crate::backend::{display_rel, EventSink, SuitePlan};
use crate::config::CloudTarget;
use crate::error::ToolError;

use api::{Session, Transport, UreqTransport};
use bundle::{BundleInput, SpecEntry};

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
    let transport = UreqTransport::new();
    run_with_transport(
        plan,
        &transport,
        &api_key,
        &universe_id,
        &place_id,
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
    on_event: &mut EventSink,
) -> Result<(), ToolError> {
    let session = Session::new(transport, api_key, universe_id, place_id);

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
            deadline_ms,
        };
        let bundle::Bundle { script, unresolved } =
            bundle::bundle_with_cache(&input, &mut sources)?;
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
            let event: Event = serde_json::from_value(value).map_err(|e| {
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
fn unresolved_warning(miss: &bundle::UnresolvedRequire, root: &Path) -> String {
    format!(
        "the string require of '{}' at {}:{} does not resolve ({}) and is not bundled — it \
         will error if reached in the engine",
        miss.spec,
        display_rel(&miss.file, root),
        miss.line,
        miss.reason
    )
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
            run_with_transport(&plan, &transport, "key", "1", "2", &mut sink).unwrap();
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
        let err = run_with_transport(&plan, &transport, "key", "1", "2", &mut sink).unwrap_err();
        assert!(err.to_string().contains("no test outcomes"), "{err}");
    }

    #[test]
    fn missing_target_is_a_clear_error() {
        let root = repo_root();
        let plan = plan_for(&root, root.join("tests/core/expect.spec.luau"));
        let err = resolve_target(&plan, &CloudTarget::default()).unwrap_err();
        assert!(err.to_string().contains("universe_id"), "{err}");
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
