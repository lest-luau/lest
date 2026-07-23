use std::io::Write;
use std::time::Duration;

use super::snapshot::SnapshotSummary;
use super::{Event, Failure, Totals};

/// How many of the slowest tests the summary lists.
const SLOWEST_COUNT: usize = 5;

/// Terminal reporter: nested describe blocks, inline failure details, a
/// slowest-tests block, and a Jest-style summary. Consumes the merged event
/// stream regardless of which backend produced it; write errors are swallowed
/// because a broken stdout must never turn a test run into a tool error.
pub struct Pretty<W: Write> {
    out: W,
    color: bool,
    printed_path: Vec<String>,
    suites_total: u32,
    suites_failed: u32,
    current_suite_failed: bool,
    /// A blank line owed after a failure block, flushed before the next test so
    /// failures breathe — but dropped by sections that space themselves, so it
    /// never doubles up before the slowest-tests or summary block.
    pending_blank: bool,
    /// Every test's describe-path prefix (kept regular) and test name (dimmed)
    /// plus its duration, aggregated for the "slowest tests" block at the end.
    durations: Vec<(String, String, f64)>,
}

// ANSI codes for the lest palette. Color carries meaning (pass/fail/skip) and
// nothing else; BOLD is reserved for section labels — `Tests:`, `Coverage:`,
// and clap's `Usage:`/`Options:` in `main::help_styles` — so every heading in
// the CLI reads the same weight regardless of who printed it.
//
// The CLI speaks in exactly two voices, and nothing else:
//
//  - **Diagnostics** — a fully colored bold label, then the message rendered
//    as a capitalized sentence with no trailing period (`report::sentence`):
//    `Error:`/`Failure:` in BOLD_RED (exit 2 / exit 1), `Warning:` in
//    BOLD_YELLOW (no exit code, always on stderr). All three render through
//    `report::diagnostic`, so the shape cannot drift.
//  - **Notes** — dim lowercase fragments without trailing periods.
//
// In particular there is no `lest:`-prefix voice; anything that used to wear
// one is a note or a warning.
pub const BOLD: &str = "1";
/// Bold + red in one SGR sequence, for the `Error:`/`Failure:` labels.
pub const BOLD_RED: &str = "1;31";
/// Bold + yellow in one SGR sequence, for the `Warning:` label.
pub(crate) const BOLD_YELLOW: &str = "1;33";
pub(crate) const GREEN: &str = "32";
pub(crate) const RED: &str = "31";
pub(crate) const YELLOW: &str = "33";
pub(crate) const DIM: &str = "2";

/// Wraps `text` in an ANSI SGR sequence when `color` is on, otherwise returns it
/// untouched. Shared so every reporter paints the same palette.
pub fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

impl<W: Write> Pretty<W> {
    pub fn new(out: W, color: bool) -> Self {
        Self {
            out,
            color,
            printed_path: Vec::new(),
            suites_total: 0,
            suites_failed: 0,
            current_suite_failed: false,
            pending_blank: false,
            durations: Vec::new(),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        paint(self.color, code, text)
    }

    /// Emits the blank line owed after a failure block, once, immediately before
    /// the next test line.
    fn flush_pending(&mut self) {
        if self.pending_blank {
            let _ = writeln!(self.out);
            self.pending_blank = false;
        }
    }

    pub fn begin_suite(&mut self, name: &str, backend: &str) {
        self.printed_path.clear();
        self.suites_total += 1;
        self.current_suite_failed = false;
        // The suite header prints its own leading blank, so drop any owed one.
        self.pending_blank = false;
        let header = format!("{} {}", name, self.paint(DIM, &format!("({backend})")));
        let _ = writeln!(self.out, "\n{header}");
    }

    /// Informational line outside any suite (e.g. an include glob that
    /// matched nothing).
    pub fn note(&mut self, message: &str) {
        let line = self.paint(DIM, message);
        let _ = writeln!(self.out, "{line}");
    }

    /// The fallback surface for a snapshot failure with no inline home — its
    /// test failed on its own, or never streamed a verdict at all (the
    /// backend died mid-test). A mismatch under a passing test never lands
    /// here: the run loop rewrites that test's `test_pass` into a failure and
    /// the diff renders inside the tree. `detail` is already-rendered diff
    /// text, so it is printed verbatim rather than dimmed.
    pub fn snapshot_failure(&mut self, spec: &str, key: &str, detail: &str) {
        let _ = writeln!(self.out);
        let header = self.paint(DIM, &format!("Snapshot \"{key}\" in {spec}:"));
        let _ = writeln!(self.out, "{header}");
        // A diff gets a legend — nothing on the `-`/`+` marks says which side
        // is the file and which is the run. Non-diff details (a duplicate
        // key's explanation, say) skip it.
        if detail
            .lines()
            .any(|line| line.starts_with("- ") || line.starts_with("+ "))
        {
            let legend = self.paint(DIM, "  - stored snapshot  + received");
            let _ = writeln!(self.out, "{legend}");
        }
        // `detail` already ends in a newline when it is a rendered diff.
        let _ = writeln!(self.out, "{}", detail.trim_end_matches('\n'));
    }

    /// A suite-level failure the event stream cannot carry: the backend died
    /// mid-suite (a runtime that would not start, a cloud task that never
    /// decoded). A dead backend emits no `test_fail`, so without this the
    /// suite counted as *passed* in the very summary printed above the fatal
    /// error. Renders a failure line inside the suite and fixes the
    /// `Test Suites:` count; the detailed diagnostic still follows the report.
    pub fn suite_error(&mut self, message: &str) {
        self.flush_pending();
        self.mark_suite_failed();
        let line = format!("{} {}", self.paint(RED, "✗"), self.paint(RED, message));
        let _ = writeln!(self.out, "{}{line}", indent(1));
    }

    fn mark_suite_failed(&mut self) {
        if !self.current_suite_failed {
            self.current_suite_failed = true;
            self.suites_failed += 1;
        }
    }

    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::SuiteStart { path } => {
                self.flush_pending();
                self.sync_path(path);
            }
            Event::TestPass {
                path,
                name,
                duration_ms,
            } => {
                self.flush_pending();
                self.record_duration(path, name, *duration_ms);
                self.sync_path(path);
                let line = format!(
                    "{} {} {}",
                    self.paint(GREEN, "✓"),
                    self.paint(DIM, name),
                    self.paint(DIM, &format!("({})", fmt_duration(*duration_ms)))
                );
                self.test_line(path, &line);
            }
            Event::TestFail {
                path,
                name,
                duration_ms,
                failure,
                origin,
            } => {
                self.flush_pending();
                // `(load)` is synthesized by every backend when a spec file
                // fails to load: nothing ran, so its hard-zero duration is a
                // construction detail, and "(0.0ms)" dresses it up as a
                // measurement. It renders without a timing and stays out of
                // the slowest-tests ranking. The other synthesized outcomes —
                // `(timeout)`, `(error)` — keep theirs, which report how long
                // the spec really ran before it was cut short.
                let load_failure = name == "(load)";
                if !load_failure {
                    self.record_duration(path, name, *duration_ms);
                }
                if !self.current_suite_failed {
                    self.current_suite_failed = true;
                    self.suites_failed += 1;
                }
                self.sync_path(path);
                let line = if load_failure {
                    format!("{} {}", self.paint(RED, "✗"), self.paint(RED, name))
                } else {
                    format!(
                        "{} {} {}",
                        self.paint(RED, "✗"),
                        self.paint(RED, name),
                        self.paint(DIM, &format!("({})", fmt_duration(*duration_ms)))
                    )
                };
                self.test_line(path, &line);
                self.failure_block(path.len(), failure, origin.as_deref());
            }
            Event::TestSkip { path, name, reason } => {
                self.flush_pending();
                self.sync_path(path);
                let suffix = match reason {
                    Some(reason) => format!("(skipped: {reason})"),
                    None => "(skipped)".to_string(),
                };
                let line = format!(
                    "{} {} {}",
                    self.paint(YELLOW, "○"),
                    self.paint(DIM, name),
                    self.paint(DIM, &suffix)
                );
                self.test_line(path, &line);
            }
            // Facts the pretty view doesn't render directly. Snapshot outcomes
            // are decided by the host and arrive via the `SnapshotSummary` in
            // `finish`, so the raw event is a no-op here.
            Event::Snapshot { .. }
            | Event::RunStart { .. }
            | Event::TestStart { .. }
            | Event::RunEnd { .. } => {}
        }
    }

    /// Renders the trailing summary block. `snapshots` is built by the host from
    /// its own snapshot comparisons (the framework only reports facts).
    pub fn finish(&mut self, totals: &Totals, snapshots: &SnapshotSummary, elapsed: Duration) {
        self.slowest_block();

        let suites_passed = self.suites_total.saturating_sub(self.suites_failed);
        let suites = self.count_line(self.suites_failed, 0, suites_passed, self.suites_total);
        let tests = self.count_line(totals.failed, totals.skipped, totals.passed, totals.total());
        let snapshots = self.snapshot_line(snapshots);
        let time = format!("{:.2}s", elapsed.as_secs_f64());

        let _ = writeln!(self.out);
        // The padding sits outside the painted label so the values stay in one
        // column whether or not the bold escape is emitted.
        let _ = writeln!(self.out, "{} {suites}", self.paint(BOLD, "Test Suites:"));
        let _ = writeln!(self.out, "{}       {tests}", self.paint(BOLD, "Tests:"));
        let _ = writeln!(self.out, "{}   {snapshots}", self.paint(BOLD, "Snapshots:"));
        let _ = writeln!(self.out, "{}        {time}", self.paint(BOLD, "Time:"));
        let _ = self.out.flush();
    }

    /// `1 failed, 2 skipped, 3 passed, 6 total` with zero segments omitted
    /// (passed always shown).
    fn count_line(&self, failed: u32, skipped: u32, passed: u32, total: u32) -> String {
        let mut parts = Vec::new();
        if failed > 0 {
            parts.push(self.paint(RED, &format!("{failed} failed")));
        }
        if skipped > 0 {
            parts.push(self.paint(YELLOW, &format!("{skipped} skipped")));
        }
        parts.push(self.paint(GREEN, &format!("{passed} passed")));
        parts.push(format!("{total} total"));
        parts.join(", ")
    }

    /// `1 failed, 3 obsolete, 2 written, 1 updated, 4 passed, 8 total`; every
    /// zero segment is omitted, so an untouched run reads simply `0 total`.
    fn snapshot_line(&self, s: &SnapshotSummary) -> String {
        let mut parts = Vec::new();
        if s.failed > 0 {
            parts.push(self.paint(RED, &format!("{} failed", s.failed)));
        }
        if s.obsolete > 0 {
            parts.push(self.paint(YELLOW, &format!("{} obsolete", s.obsolete)));
        }
        if s.written > 0 {
            parts.push(self.paint(GREEN, &format!("{} written", s.written)));
        }
        if s.updated > 0 {
            parts.push(self.paint(GREEN, &format!("{} updated", s.updated)));
        }
        if s.matched > 0 {
            parts.push(self.paint(GREEN, &format!("{} passed", s.matched)));
        }
        parts.push(format!("{} total", s.total()));
        parts.join(", ")
    }

    fn record_duration(&mut self, path: &[String], name: &str, duration_ms: f64) {
        let mut prefix = path.join(" › ");
        if !prefix.is_empty() {
            prefix.push_str(" › ");
        }
        self.durations.push((prefix, name.to_string(), duration_ms));
    }

    /// Prints the slowest tests, longest first. Silent when nothing ran.
    fn slowest_block(&mut self) {
        if self.durations.is_empty() {
            return;
        }
        let mut ranked = self.durations.clone();
        ranked.sort_by(|a, b| b.2.total_cmp(&a.2));
        ranked.truncate(SLOWEST_COUNT);

        // This block prints its own leading blank, so drop any owed one.
        self.pending_blank = false;
        let _ = writeln!(self.out, "\n{}", self.paint(BOLD, "Slowest Tests:"));
        for (prefix, name, ms) in ranked {
            let name = self.paint(DIM, &name);
            let timing = self.paint(DIM, &format!("({})", fmt_duration(ms)));
            let _ = writeln!(self.out, "  {prefix}{name} {timing}");
        }
    }

    /// Prints headers for describe segments not yet on screen, so nesting
    /// renders correctly whether or not a backend emitted `suite_start`.
    fn sync_path(&mut self, path: &[String]) {
        let mut common = 0;
        while common < self.printed_path.len()
            && common < path.len()
            && self.printed_path[common] == path[common]
        {
            common += 1;
        }
        self.printed_path.truncate(common);
        while self.printed_path.len() < path.len() {
            let depth = self.printed_path.len();
            let segment = &path[self.printed_path.len()];
            let _ = writeln!(self.out, "{}{}", indent(depth + 1), segment);
            self.printed_path.push(segment.clone());
        }
    }

    fn test_line(&mut self, path: &[String], line: &str) {
        let _ = writeln!(self.out, "{}{line}", indent(path.len() + 1));
    }

    fn failure_block(&mut self, depth: usize, failure: &Failure, origin: Option<&str>) {
        let pad = indent(depth + 2);
        // A blank line above the detail; the one below is deferred so it never
        // doubles up before a self-spacing section (see `pending_blank`).
        let _ = writeln!(self.out);
        // A failure from a hook, not the test body: without this label the
        // reader has to notice the failing line sits outside the test to
        // realize setup broke.
        if let Some(origin) = origin {
            let label = self.paint(DIM, &format!("failed in {origin}:"));
            let _ = writeln!(self.out, "{pad}{label}");
        }
        match failure {
            Failure::Assertion {
                message,
                expected,
                received,
            } => {
                // Expected/Received tell the story on their own; the raw message
                // is only shown when neither is present (e.g. `toBeTruthy`).
                if expected.is_none() && received.is_none() {
                    let _ = writeln!(self.out, "{pad}{}", self.paint(RED, message));
                }
                if let Some(expected) = expected {
                    let value = self.paint(GREEN, expected);
                    let _ = writeln!(self.out, "{pad}Expected: {value}");
                }
                if let Some(received) = received {
                    let value = self.paint(RED, received);
                    let _ = writeln!(self.out, "{pad}Received: {value}");
                }
            }
            Failure::Error { message, trace } => {
                let _ = writeln!(self.out, "{pad}{}", self.paint(RED, message));
                for line in trace.iter().flat_map(|trace| trace.lines()) {
                    let line = self.paint(DIM, line.trim_end());
                    let _ = writeln!(self.out, "{pad}{line}");
                }
            }
            // Host-synthesized when a passing test's snapshots mismatched.
            // Each mismatch names its key — one test can hold several
            // snapshots, so "Expected/Received" alone would not say which
            // call diverged — and the body is the stored-vs-received line
            // diff, already colored, indented one level under its header.
            // The header stays regular weight, like the Expected/Received
            // labels: it is the failure's substance, not a dim aside.
            Failure::Snapshot { mismatches } => {
                for mismatch in mismatches {
                    let _ = writeln!(
                        self.out,
                        "{pad}Snapshot \"{}\" did not match:",
                        mismatch.key
                    );
                    for line in mismatch.detail.trim_end_matches('\n').lines() {
                        let _ = writeln!(self.out, "{pad}  {line}");
                    }
                }
            }
        }
        // Defer the trailing blank: the next test flushes it, self-spacing
        // sections (slowest block, summary, next suite) drop it — no doubling.
        self.pending_blank = true;
    }
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

/// Formats a millisecond duration, promoting to seconds at (and above) 1000ms.
/// Bucket boundaries are chosen on the *rounded* value so a duration never
/// renders as e.g. `1000ms` — `[999.5, 1000)` rounds up and promotes to
/// `1.00s`, and `[9.95, 10)` renders `10ms` rather than `10.0ms`.
fn fmt_duration(ms: f64) -> String {
    if ms < 9.95 {
        format!("{ms:.1}ms")
    } else if ms < 999.5 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::SnapshotMismatch;

    fn render(events: &[Event]) -> String {
        render_with(events, &SnapshotSummary::default())
    }

    fn render_with(events: &[Event], snapshots: &SnapshotSummary) -> String {
        let mut buf = Vec::new();
        {
            let mut pretty = Pretty::new(&mut buf, false);
            pretty.begin_suite("unit", "native");
            let mut totals = Totals::default();
            for event in events {
                totals.record(event);
                pretty.on_event(event);
            }
            pretty.finish(&totals, snapshots, Duration::from_millis(120));
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn renders_nested_describes_once() {
        let out = render(&[
            Event::TestPass {
                path: vec!["math".into(), "add".into()],
                name: "adds".into(),
                duration_ms: 0.5,
            },
            Event::TestPass {
                path: vec!["math".into(), "add".into()],
                name: "adds negatives".into(),
                duration_ms: 0.5,
            },
        ]);
        // Assert against the test tree only; the slowest-tests block legitimately
        // repeats the describe path.
        let tree = out.split("Slowest Tests:").next().unwrap();
        assert_eq!(tree.matches("math").count(), 1);
        assert_eq!(tree.matches("add\n").count(), 1);
        assert!(tree.contains("✓ adds"));
    }

    #[test]
    fn renders_failure_details_and_summary() {
        let out = render(&[Event::TestFail {
            path: vec!["math".into()],
            name: "breaks".into(),
            duration_ms: 1.0,
            failure: Failure::Assertion {
                message: "expected 1 to be 2".into(),
                expected: Some("2".into()),
                received: Some("1".into()),
            },
            origin: None,
        }]);
        assert!(out.contains("✗ breaks"));
        assert!(out.contains("Expected: 2"));
        assert!(out.contains("Received: 1"));
        assert!(out.contains("Test Suites: 1 failed, 0 passed, 1 total"));
        assert!(out.contains("Tests:       1 failed, 0 passed, 1 total"));
        assert!(out.contains("Snapshots:   0 total"));
        assert!(out.contains("Time:        0.12s"));
    }

    /// A hook failure names its origin — the reader should not have to
    /// notice that the failing line sits outside the test body to realize
    /// setup broke, not the test.
    #[test]
    fn hook_failures_are_labeled_with_their_origin() {
        let out = render(&[Event::TestFail {
            path: vec!["doomed".into()],
            name: "never runs".into(),
            duration_ms: 0.0,
            failure: Failure::Error {
                message: "the database is down".into(),
                trace: None,
            },
            origin: Some("beforeAll".into()),
        }]);
        assert!(out.contains("failed in beforeAll:"), "{out}");
        assert!(out.contains("the database is down"), "{out}");
    }

    /// `(load)` is a synthesized outcome — nothing ran, so a timing would be
    /// noise. It still counts as a failure and still prints its details.
    #[test]
    fn load_failures_render_without_a_duration_and_stay_out_of_slowest() {
        let out = render(&[
            Event::TestFail {
                path: vec!["tests/engine/foo.spec.luau".into()],
                name: "(load)".into(),
                duration_ms: 0.0,
                failure: Failure::Error {
                    message: "boom".into(),
                    trace: None,
                },
                origin: None,
            },
            // A real test, so the slowest block renders and can be inspected.
            Event::TestPass {
                path: vec!["s".into()],
                name: "fast".into(),
                duration_ms: 0.2,
            },
        ]);
        assert!(out.contains("✗ (load)\n"), "no timing suffix, got: {out}");
        assert!(out.contains("boom"));
        assert!(out.contains("Tests:       1 failed, 1 passed, 2 total"));
        let slowest = out.split("Slowest Tests:").nth(1).unwrap();
        assert!(
            !slowest.contains("(load)"),
            "(load) must not rank as a slow test, got: {slowest}"
        );
    }

    #[test]
    fn skipped_tests_appear_in_the_summary() {
        let out = render(&[Event::TestSkip {
            path: vec![],
            name: "later".into(),
            reason: None,
        }]);
        assert!(out.contains("○ later"));
        assert!(out.contains("Tests:       1 skipped, 0 passed, 1 total"));
    }

    #[test]
    fn snapshot_summary_reports_written_updated_failed_obsolete() {
        let out = render_with(
            &[Event::TestPass {
                path: vec![],
                name: "a".into(),
                duration_ms: 0.1,
            }],
            &SnapshotSummary {
                matched: 3,
                written: 1,
                updated: 2,
                failed: 1,
                obsolete: 4,
            },
        );
        assert!(out.contains(
            "Snapshots:   1 failed, 4 obsolete, 1 written, 2 updated, 3 passed, 7 total"
        ));
    }

    #[test]
    fn slowest_tests_block_is_ordered_and_capped() {
        let events: Vec<Event> = (0..7)
            .map(|i| Event::TestPass {
                path: vec!["s".into()],
                name: format!("t{i}"),
                duration_ms: i as f64,
            })
            .collect();
        let out = render(&events);
        assert!(out.contains("Slowest Tests:"));
        // Cap is five entries; the fastest two are excluded.
        let block = out.split("Slowest Tests:").nth(1).unwrap();
        assert_eq!(block.matches("s › t").count(), SLOWEST_COUNT);
        // Slowest (t6) listed before a faster one (t2).
        let t6 = block.find("s › t6").unwrap();
        let t2 = block.find("s › t2").unwrap();
        assert!(t6 < t2);
    }

    /// The regression: a backend that died mid-suite emitted no test_fail, so
    /// the summary said `Test Suites: 1 passed` right above the fatal error.
    #[test]
    fn suite_error_prints_and_fails_the_suite() {
        let mut buf = Vec::new();
        {
            let mut pretty = Pretty::new(&mut buf, false);
            pretty.begin_suite("scripts", "lune");
            pretty.suite_error("the suite did not finish: lune exited with exit code: 1");
            pretty.finish(
                &Totals::default(),
                &SnapshotSummary::default(),
                Duration::from_millis(10),
            );
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("✗ the suite did not finish"), "{out}");
        assert!(
            out.contains("Test Suites: 1 failed, 0 passed, 1 total"),
            "{out}"
        );
    }

    /// Snapshot comparison happens host-side; the run loop rewrites a passing
    /// test whose snapshots mismatched into a `test_fail` carrying the diffs.
    /// The tree renders it like any other failure — ✗ on the owning test,
    /// each mismatch labeled with its key, the diff indented beneath.
    #[test]
    fn snapshot_mismatches_render_inline_under_the_owning_test() {
        let out = render_with(
            &[Event::TestFail {
                path: vec!["snapshot store".into()],
                name: "stores scalars".into(),
                duration_ms: 0.1,
                failure: Failure::Snapshot {
                    mismatches: vec![SnapshotMismatch {
                        key: "snapshot store > stores scalars 4".into(),
                        detail: "- nil\n+ 3\n".into(),
                    }],
                },
                origin: None,
            }],
            &SnapshotSummary {
                failed: 1,
                ..SnapshotSummary::default()
            },
        );
        assert!(out.contains("✗ stores scalars"), "{out}");
        assert!(
            out.contains(
                "      Snapshot \"snapshot store > stores scalars 4\" did not match:\n\
                 \x20       - nil\n\
                 \x20       + 3\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("Test Suites: 1 failed, 0 passed, 1 total"),
            "{out}"
        );
        assert!(
            out.contains("Tests:       1 failed, 0 passed, 1 total"),
            "{out}"
        );
        assert!(out.contains("Snapshots:   1 failed, 1 total"), "{out}");
    }

    /// One test can hold several snapshots; every mismatch renders under the
    /// single ✗ line, each labeled with its own key.
    #[test]
    fn multiple_mismatches_stack_under_one_test() {
        let out = render(&[Event::TestFail {
            path: vec![],
            name: "twice".into(),
            duration_ms: 0.1,
            failure: Failure::Snapshot {
                mismatches: vec![
                    SnapshotMismatch {
                        key: "twice 1".into(),
                        detail: "- 1\n+ 2\n".into(),
                    },
                    SnapshotMismatch {
                        key: "twice 2".into(),
                        detail: "- 3\n+ 4\n".into(),
                    },
                ],
            },
            origin: None,
        }]);
        assert!(out.contains("Snapshot \"twice 1\" did not match:"), "{out}");
        assert!(out.contains("Snapshot \"twice 2\" did not match:"), "{out}");
        assert!(
            out.contains("Tests:       1 failed, 0 passed, 1 total"),
            "{out}"
        );
    }

    #[test]
    fn suites_passed_never_underflows() {
        // A run with no suites must not panic on `total - failed`.
        let mut buf = Vec::new();
        {
            let mut pretty = Pretty::new(&mut buf, false);
            pretty.finish(
                &Totals::default(),
                &SnapshotSummary::default(),
                Duration::from_millis(0),
            );
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Test Suites: 0 passed, 0 total"));
    }

    #[test]
    fn duration_boundaries_promote_cleanly() {
        assert_eq!(fmt_duration(5.0), "5.0ms");
        assert_eq!(fmt_duration(9.95), "10ms");
        assert_eq!(fmt_duration(123.4), "123ms");
        assert_eq!(fmt_duration(999.4), "999ms");
        assert_eq!(fmt_duration(999.5), "1.00s");
        assert_eq!(fmt_duration(1500.0), "1.50s");
    }
}
