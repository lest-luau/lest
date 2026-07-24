//! The studio backend: runs a suite in a live Roblox Studio session.
//!
//! The same bundle machinery as cloud (module factories, rojo delegation,
//! source map) with the studio head: events leave the playtest as
//! sentinel-framed print lines, the companion plugin relays them over the
//! loopback bridge, and this module decodes them with the spawned-runtime
//! decoder. The hand-verified platform reality shapes the flow: the CLI
//! cannot start a playtest itself (the Run() API is a no-op), so the plugin
//! arms the suite, attempts the start anyway, and the user's Run press is
//! the expected trigger — the CLI waits, saying so.
//!
//! Never a CI backend: engine suites in CI belong on cloud. The guard here
//! is loud so a misconfigured pipeline cannot hang waiting for a Studio
//! that will never exist.

use std::collections::HashSet;
use std::time::Duration;

use crate::backend::cloud::bundle::{self, BundleInput, Head, SpecEntry};
use crate::backend::runtime::{classify, passthrough, Decoded, DONE_SENTINEL};
use crate::backend::{display_rel, EventSink, SuitePlan};
use crate::error::ToolError;
use crate::report::{check_protocol_version, Event, Failure};
use crate::studio::bridge::{Bridge, RunOutcome};

/// How long a bound bridge waits for any Studio session to fetch the job
/// before concluding none is listening. The plugin's idle poll ceiling is
/// 2s; the margin covers a poll mid-flight and one suspect-widened gap.
const FETCH_WAIT: Duration = Duration::from_secs(10);

/// Fixed allowance for arming plus the human Run press, on top of the
/// per-spec budgets. Generous on purpose: the CLI says it is waiting, and a
/// user who wandered off deserves better than a race against a stopwatch.
const PRESS_RUN_ALLOWANCE: Duration = Duration::from_secs(120);

pub fn run(plan: &SuitePlan, on_event: &mut EventSink) -> Result<(), ToolError> {
    if std::env::var_os("CI").is_some() {
        return Err(ToolError(
            "the studio backend needs a live Studio session and cannot run in CI — engine \
             suites in CI belong on the cloud backend"
                .into(),
        ));
    }
    let (port, secret) = crate::studio::credentials()?;

    let entries: Vec<SpecEntry> = plan
        .specs
        .iter()
        .map(|spec| SpecEntry {
            name: display_rel(spec, &plan.root),
            path: spec.clone(),
        })
        .collect();

    // `[settings] rojo`, honored exactly as the cloud backend honors it: the
    // suite runs against the open place, so mapped requires delegate to it.
    let place_map = match &plan.rojo_project {
        Some(project) => Some(
            crate::resolve::VirtualDataModel::from_project_file(project)
                .map_err(|e| ToolError(e.to_string()))?,
        ),
        None => None,
    };

    // Per-spec in-engine deadline: the cloud rule (single-spec budget plus
    // fixed slack), for the cloud reasons — the engine cannot preempt.
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
    // One bundle per run (all specs share it), so the cache's cross-bundle
    // reuse buys nothing here — it exists because `bundle_with_cache` is the
    // one production entry point.
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

    // One playtest runs every spec, so the budget scales with the suite —
    // the N-scaling the cloud backend avoids per-task is inherent to one
    // session here, exactly as it is for the spawned runtimes.
    let spec_count = u32::try_from(plan.specs.len().max(1)).unwrap_or(u32::MAX);
    let run_budget = PRESS_RUN_ALLOWANCE.saturating_add(budget.saturating_mul(spec_count));

    let bridge = Bridge::bind(port, &secret)?;
    crate::report::note_to_stderr(
        "waiting for a Studio session to pick up the suite (open your place in Studio; \
         `lest studio status` checks the connection)",
    );

    let mut outcomes = 0usize;
    let mut saw_protocol = false;
    let mut current_spec: Option<usize> = None;

    let mut on_line = |line: &str| -> Result<(), ToolError> {
        // The done marker is the plugin's signal; the /done post carries it
        // to this side. As a relayed line it is framing, not output.
        if line.contains(DONE_SENTINEL) {
            return Ok(());
        }
        match classify(line) {
            Decoded::SpecBoundary { leading, index } => {
                passthrough(leading);
                saw_protocol = true;
                let raw = index.trim();
                let resolved = raw
                    .parse::<usize>()
                    .ok()
                    .and_then(|one_based| one_based.checked_sub(1))
                    .filter(|&i| i < plan.specs.len());
                match resolved {
                    Some(index) => {
                        current_spec = Some(index);
                        Ok(())
                    }
                    None => Err(ToolError(format!(
                        "the studio session sent the spec-boundary marker \"{raw}\", which is \
                         not a 1-based index into suite \"{}\"'s {} spec file(s) — the injected \
                         bundle and the CLI disagree about the spec list",
                        plan.name,
                        plan.specs.len()
                    ))),
                }
            }
            Decoded::Event { leading, json } => {
                passthrough(leading);
                saw_protocol = true;
                let event = serde_json::from_str::<Event>(json).map_err(|err| {
                    ToolError(format!(
                        "undecodable protocol line from the studio session while running suite \
                         \"{}\": {err}",
                        plan.name
                    ))
                })?;
                if let Event::RunStart {
                    protocol_version, ..
                } = event
                {
                    check_protocol_version(protocol_version).map_err(|mismatch| {
                        ToolError(format!(
                            "framework/CLI protocol mismatch from the studio session: {mismatch}"
                        ))
                    })?;
                }
                if matches!(
                    event,
                    Event::TestPass { .. } | Event::TestFail { .. } | Event::TestSkip { .. }
                ) {
                    outcomes += 1;
                }
                let spec = current_spec.map(|i| plan.specs[i].as_path());
                on_event(spec, &event);
                Ok(())
            }
            Decoded::Output => {
                // A relayed line always carried a marker (the plugin filters),
                // but a payload could smuggle plain text here; echo it like
                // every backend echoes test output.
                println!("{line}");
                Ok(())
            }
        }
    };
    let mut on_armed = || {
        crate::report::note_to_stderr("suite armed in Studio — press Run (F8) there to execute it");
    };

    let outcome = bridge.run_suite(
        &built.script,
        FETCH_WAIT,
        run_budget,
        &mut on_line,
        &mut on_armed,
    )?;

    match outcome {
        RunOutcome::Finished { stopped } => {
            if !stopped {
                crate::report::note_to_stderr(
                    "the playtest is still running — press Stop in Studio when you're done",
                );
            }
            // The same false-green guard every backend carries: files ran,
            // nothing was decided — that is an error, not a pass.
            if outcomes == 0 && !plan.specs.is_empty() {
                return Err(ToolError(format!(
                    "the studio session ran {} spec file(s) for suite \"{}\" but produced no \
                     test outcomes — the bundle likely failed to load; check Studio's output \
                     window",
                    plan.specs.len(),
                    plan.name
                )));
            }
            Ok(())
        }
        RunOutcome::RefusedSecret => Err(ToolError(
            "a Studio plugin answered with a mismatched install — run `lest studio install`, \
             restart Studio, and re-run"
                .into(),
        )),
        RunOutcome::NeverFetched => Err(ToolError(
            "no Studio session picked up the suite — open your place in Studio with the lest \
             plugin installed (`lest studio status` checks), then re-run"
                .into(),
        )),
        RunOutcome::Refused(reason) => Err(ToolError(format!(
            "the Studio plugin refused the run: {reason}"
        ))),
        RunOutcome::Died => {
            if saw_protocol {
                // The suite started and went quiet: a hang inside the tests
                // is a test failure (exit 1), matching the other backends'
                // budget expiries — discarding the streamed outcomes behind
                // a tool error would hide everything that already ran.
                let spec = current_spec.map(|i| plan.specs[i].as_path());
                let path = spec
                    .map(|p| display_rel(p, &plan.root))
                    .unwrap_or_else(|| plan.name.clone());
                let event = Event::TestFail {
                    path: vec![path],
                    name: "(timeout)".to_string(),
                    duration_ms: run_budget.as_millis() as f64,
                    failure: Failure::Error {
                        message: format!(
                            "the studio session stopped streaming before suite \"{}\" finished \
                             — was the playtest stopped early?",
                            plan.name
                        ),
                        trace: None,
                    },
                    origin: None,
                };
                on_event(spec, &event);
                Ok(())
            } else {
                Err(ToolError(
                    "the suite was armed in Studio but never ran — press Run (F8) while the \
                     CLI is waiting, and keep the session open until it finishes"
                        .into(),
                ))
            }
        }
    }
}
