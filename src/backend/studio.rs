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
use crate::backend::runtime::{classify, passthrough, Decoded, DONE_SENTINEL, SENTINEL};
use crate::backend::{display_rel, EventSink, SuitePlan};
use crate::config::CloudTarget;
use crate::error::ToolError;
use crate::report::{check_protocol_version, Event, Failure};
use crate::studio::bridge::{Bridge, PingOutcome, RunOutcome};

/// How long the pre-run ping waits for a session to identify itself. The
/// plugin's idle poll ceiling is 2s; the margin covers a poll mid-flight
/// and one suspect-widened gap.
const SESSION_WAIT: Duration = Duration::from_secs(4);

/// How long a bound bridge waits for the identified session to fetch the
/// run job. Short: the ping already proved a plugin is polling.
const FETCH_WAIT: Duration = Duration::from_secs(10);

/// Fixed allowance for arming plus the human Run press, on top of the
/// per-spec budgets. Generous on purpose: the CLI says it is waiting, and a
/// user who wandered off deserves better than a race against a stopwatch.
const PRESS_RUN_ALLOWANCE: Duration = Duration::from_secs(120);

pub fn run(
    plan: &SuitePlan,
    target: &CloudTarget,
    on_event: &mut EventSink,
) -> Result<(), ToolError> {
    if crate::is_ci() {
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

    let bridge = Bridge::bind(port, &secret)?;

    // Identify the session before arming anything: the ping answer carries
    // the place and plugin version, which is what makes a wrong-place run a
    // warning up front instead of a mystery of failing delegated requires,
    // and version skew an instruction instead of a silent difference.
    match bridge.ping(SESSION_WAIT)? {
        PingOutcome::Session(session) => {
            if let Some(note) = crate::studio::version_note(&session.plugin_version) {
                crate::report::warn_to_stderr(&note);
            }
            if let (Some(expected), Some(actual)) = (&target.place_id, session.place_id) {
                if expected != &actual.to_string() {
                    crate::report::warn_to_stderr(&format!(
                        "the open Studio place ({} — id {actual}) is not this suite's \
                         configured place_id {expected}; mapped requires may not resolve",
                        session.place_name
                    ));
                }
            }
        }
        PingOutcome::RefusedSecret => {
            return Err(ToolError(
                "a Studio plugin answered with a mismatched install — run `lest studio \
                 install`, restart Studio, and re-run"
                    .into(),
            ));
        }
        PingOutcome::Silent => {
            return Err(ToolError(
                "no Studio session answered — open your place in Studio with the lest plugin \
                 installed (`lest studio status` checks), then re-run"
                    .into(),
            ));
        }
    }

    // One playtest runs every spec, so the budget scales with the suite —
    // the N-scaling the cloud backend avoids per-task is inherent to one
    // session here, exactly as it is for the spawned runtimes.
    let spec_count = u32::try_from(plan.specs.len().max(1)).unwrap_or(u32::MAX);
    let run_budget = PRESS_RUN_ALLOWANCE.saturating_add(budget.saturating_mul(spec_count));

    crate::report::note_to_stderr("handing the suite to the Studio session…");

    let mut decoder = LineDecoder::new(plan);
    let mut on_line = |line: &str| decoder.feed(line, on_event);
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
    let (outcomes, saw_protocol, current_spec) = decoder.into_parts();

    match outcome {
        RunOutcome::Finished { stopped } => {
            if !stopped {
                crate::report::note_to_stderr(
                    "the playtest is still running — press Stop in Studio when you're done",
                );
            }
            // The same false-green guard every backend carries: files ran,
            // nothing was decided — an error, not a pass. Disarmed under a
            // name filter, which legitimately selects zero tests (core
            // drops non-matching tests without a skip event).
            if outcomes == 0 && !plan.specs.is_empty() && plan.name_filter.is_none() {
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
            "the Studio session stopped polling before it picked up the suite — is Studio \
             still open? re-run once it is"
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
                    "the suite was armed in Studio but no test events arrived — press Run (F8) \
                     while the CLI is waiting; if you did, the bundle likely failed to load, \
                     and Studio's output window has the error"
                        .into(),
                ))
            }
        }
    }
}

/// The done marker only *frames* a line when no event marker precedes it: a
/// legitimate event payload (a snapshot's text, a failure message) may
/// contain the marker characters, and dropping that line would lose a real
/// verdict. The first-marker-wins rule from `classify`, applied to the third
/// marker; the plugin applies the same test before treating a line as
/// completion.
fn is_done_framed(line: &str) -> bool {
    match line.find(DONE_SENTINEL) {
        None => false,
        Some(done_at) => line
            .find(SENTINEL)
            .is_none_or(|event_at| done_at < event_at),
    }
}

/// Decodes relayed sentinel lines into protocol events, tracking the state
/// the run outcome needs afterward. Split from `run` so the decode rules —
/// boundary mapping, the done-framing skip, protocol validation, outcome
/// counting — are testable without a bridge or a Studio.
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
        // The done marker is the plugin's signal; the /done post carries it
        // to this side. As a relayed line it is framing, not output — but
        // only when it actually frames the line (see `is_done_framed`).
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
                        "the studio session sent the spec-boundary marker \"{raw}\", which is \
                         not a 1-based index into suite \"{}\"'s {} spec file(s) — the injected \
                         bundle and the CLI disagree about the spec list",
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
                        "undecodable protocol line from the studio session while running suite \
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
                            "framework/CLI protocol mismatch from the studio session: {mismatch}"
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
                // A relayed line always carried a marker (the plugin filters),
                // but a payload could smuggle plain text here; echo it like
                // every backend echoes test output.
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
                // Framing marker: dropped, no event.
                "@@LEST_STUDIO_DONE@@",
                // The marker inside an event payload: a real event that must
                // survive (the forged-teardown/lost-verdict hazard).
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
}
