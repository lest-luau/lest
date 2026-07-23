//! JUnit XML reporter for CI annotations.
//!
//! Consumes the same merged event stream as [`Pretty`](super::Pretty) and
//! [`Json`](super::Json), but because JUnit's `<testsuite>`/`<testsuites>`
//! elements carry aggregate counts in their opening tags, cases are buffered
//! and the whole document is written in [`finish`](Junit::finish).

use std::io::Write;
use std::time::Duration;

use super::{Event, Failure, Totals};

/// Buffers protocol events into JUnit `<testsuite>`s (one per `begin_suite`) and
/// emits a single `<testsuites>` document on `finish`. Same surface shape as the
/// other reporters so the CLI can treat them interchangeably.
pub struct Junit<W: Write> {
    out: W,
    suites: Vec<JunitSuite>,
}

struct JunitSuite {
    name: String,
    backend: String,
    cases: Vec<JunitCase>,
}

struct JunitCase {
    classname: String,
    name: String,
    time_secs: f64,
    outcome: Outcome,
}

enum Outcome {
    Pass,
    Fail {
        message: String,
        kind: &'static str,
        detail: String,
    },
    /// The harness broke, as opposed to an assertion verdict — JUnit's
    /// `<failure>`/`<error>` split exists for exactly this distinction.
    Error {
        message: String,
    },
    Skip {
        reason: Option<String>,
    },
}

impl<W: Write> Junit<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            suites: Vec::new(),
        }
    }

    pub fn begin_suite(&mut self, name: &str, backend: &str) {
        self.suites.push(JunitSuite {
            name: name.to_string(),
            backend: backend.to_string(),
            cases: Vec::new(),
        });
    }

    /// Notes have no natural home in JUnit XML, so they are dropped — the
    /// method exists only to match the reporter surface. Snapshot failures are
    /// *not* notes; see [`snapshot_failure`](Self::snapshot_failure).
    pub fn note(&mut self, _message: &str) {}

    /// Records a snapshot mismatch as a real `<testcase>` carrying a
    /// `<failure type="snapshot">`.
    ///
    /// Snapshots are compared CLI-side, so a mismatch never arrives as a
    /// protocol event — yet the exit code gates on it. Routing them to `note`
    /// (a no-op here) produced a document claiming `failures="0"` alongside
    /// exit 1: a CI system trusting the XML read the run as green and the
    /// annotation pointed at nothing, which is the whole purpose of this
    /// reporter. They arrive after the last suite closes, so they attach to the
    /// suite that ran last.
    pub fn snapshot_failure(&mut self, spec: &str, key: &str, detail: &str) {
        self.push_case(JunitCase {
            classname: spec.to_string(),
            name: format!("snapshot: {key}"),
            time_secs: 0.0,
            outcome: Outcome::Fail {
                message: format!("snapshot \"{key}\" did not match the stored value"),
                kind: "snapshot",
                detail: detail.to_string(),
            },
        });
    }

    /// A suite-level tool failure — the backend died mid-suite — as a
    /// synthesized `<testcase>` carrying an `<error>` element. A dead backend
    /// emits no events, so without this the document claimed the suite passed
    /// beside exit 2. CI systems annotate testcases, so it gets one to point
    /// at, the same trick [`snapshot_failure`](Self::snapshot_failure) uses.
    pub fn suite_error(&mut self, message: &str) {
        let classname = self
            .suites
            .last()
            .map(|suite| suite.name.clone())
            .unwrap_or_else(|| "lest".to_string());
        self.push_case(JunitCase {
            classname,
            name: "(suite)".to_string(),
            time_secs: 0.0,
            outcome: Outcome::Error {
                message: message.to_string(),
            },
        });
    }

    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::TestPass {
                path,
                name,
                duration_ms,
            } => self.push(path, name, *duration_ms, Outcome::Pass),
            Event::TestFail {
                path,
                name,
                duration_ms,
                failure,
                ..
            } => {
                let outcome = match failure {
                    Failure::Assertion {
                        message,
                        expected,
                        received,
                    } => {
                        let mut detail = String::new();
                        if let Some(expected) = expected {
                            detail.push_str(&format!("expected: {expected}\n"));
                        }
                        if let Some(received) = received {
                            detail.push_str(&format!("received: {received}"));
                        }
                        Outcome::Fail {
                            message: message.clone(),
                            kind: "assertion",
                            detail: detail.trim_end().to_string(),
                        }
                    }
                    Failure::Error { message, trace } => Outcome::Fail {
                        message: message.clone(),
                        kind: "error",
                        detail: trace.clone().unwrap_or_default(),
                    },
                };
                self.push(path, name, *duration_ms, outcome);
            }
            Event::TestSkip { path, name, reason } => self.push(
                path,
                name,
                0.0,
                Outcome::Skip {
                    reason: reason.clone(),
                },
            ),
            Event::SuiteStart { .. }
            | Event::Snapshot { .. }
            | Event::RunStart { .. }
            | Event::TestStart { .. }
            | Event::RunEnd { .. } => {}
        }
    }

    pub fn finish(&mut self, _totals: &Totals, elapsed: Duration) {
        let total_tests: usize = self.suites.iter().map(|s| s.cases.len()).sum();
        let total_failures: usize = self
            .suites
            .iter()
            .flat_map(|s| &s.cases)
            .filter(|c| matches!(c.outcome, Outcome::Fail { .. }))
            .count();
        let total_errors: usize = self
            .suites
            .iter()
            .flat_map(|s| &s.cases)
            .filter(|c| matches!(c.outcome, Outcome::Error { .. }))
            .count();
        let total_skipped: usize = self
            .suites
            .iter()
            .flat_map(|s| &s.cases)
            .filter(|c| matches!(c.outcome, Outcome::Skip { .. }))
            .count();

        let _ = writeln!(self.out, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        let _ = writeln!(
            self.out,
            r#"<testsuites name="lest" tests="{}" failures="{}" skipped="{}" errors="{}" time="{:.3}">"#,
            total_tests,
            total_failures,
            total_skipped,
            total_errors,
            elapsed.as_secs_f64(),
        );

        for suite in &self.suites {
            write_suite(&mut self.out, suite);
        }

        let _ = writeln!(self.out, "</testsuites>");
        let _ = self.out.flush();
    }

    fn push(&mut self, path: &[String], name: &str, duration_ms: f64, outcome: Outcome) {
        self.push_case(JunitCase {
            classname: path.join(" › "),
            name: name.to_string(),
            time_secs: duration_ms / 1000.0,
            outcome,
        });
    }

    fn push_case(&mut self, case: JunitCase) {
        // Lazily open a default suite if the CLI never called `begin_suite`.
        if self.suites.is_empty() {
            self.suites.push(JunitSuite {
                name: "lest".to_string(),
                backend: String::new(),
                cases: Vec::new(),
            });
        }
        self.suites.last_mut().unwrap().cases.push(case);
    }
}

fn write_suite<W: Write>(out: &mut W, suite: &JunitSuite) {
    let failures = suite
        .cases
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Fail { .. }))
        .count();
    let errors = suite
        .cases
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Error { .. }))
        .count();
    let skipped = suite
        .cases
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Skip { .. }))
        .count();
    let time: f64 = suite.cases.iter().map(|c| c.time_secs).sum();

    let name = if suite.backend.is_empty() {
        escape_attr(&suite.name)
    } else {
        escape_attr(&format!("{} ({})", suite.name, suite.backend))
    };
    let _ = writeln!(
        out,
        r#"  <testsuite name="{}" tests="{}" failures="{}" skipped="{}" errors="{}" time="{:.3}">"#,
        name,
        suite.cases.len(),
        failures,
        skipped,
        errors,
        time,
    );

    for case in &suite.cases {
        write_case(out, case);
    }

    let _ = writeln!(out, "  </testsuite>");
}

fn write_case<W: Write>(out: &mut W, case: &JunitCase) {
    let open = format!(
        r#"    <testcase name="{}" classname="{}" time="{:.3}""#,
        escape_attr(&case.name),
        escape_attr(&case.classname),
        case.time_secs,
    );
    match &case.outcome {
        Outcome::Pass => {
            let _ = writeln!(out, "{open}/>");
        }
        Outcome::Fail {
            message,
            kind,
            detail,
        } => {
            let _ = writeln!(out, "{open}>");
            let _ = writeln!(
                out,
                r#"      <failure message="{}" type="{}">{}</failure>"#,
                escape_attr(message),
                kind,
                escape_text(detail),
            );
            let _ = writeln!(out, "    </testcase>");
        }
        Outcome::Error { message } => {
            let _ = writeln!(out, "{open}>");
            let _ = writeln!(
                out,
                r#"      <error message="{}" type="tool"/>"#,
                escape_attr(message),
            );
            let _ = writeln!(out, "    </testcase>");
        }
        Outcome::Skip { reason } => {
            let _ = writeln!(out, "{open}>");
            match reason {
                Some(reason) => {
                    let _ = writeln!(out, r#"      <skipped message="{}"/>"#, escape_attr(reason));
                }
                None => {
                    let _ = writeln!(out, "      <skipped/>");
                }
            }
            let _ = writeln!(out, "    </testcase>");
        }
    }
}

/// Escapes text content for XML (`&`, `<`, `>`) and drops the control
/// characters XML 1.0 forbids outright.
///
/// Only `\t`, `\n` and `\r` are legal below `U+0020`; there is no escape for
/// the rest — not even a numeric one — so a document containing a raw ANSI
/// escape or stray control byte is rejected by every conforming parser. Test
/// names and `error()` payloads are user data and routinely carry both, and a
/// CI run that turns a test failure into an unreadable pipeline error is worse
/// than one that loses a byte, so they are replaced with `U+FFFD` (visible,
/// rather than silently vanishing).
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// Escapes an attribute value for XML: everything [`escape_text`] does, plus
/// quotes and the whitespace characters attribute-value normalization would
/// otherwise destroy.
///
/// Per XML 1.0, a parser rewrites a literal newline, carriage return or tab
/// inside an attribute value to a single space *before* the application sees
/// it — so a multi-line failure message written raw into `message="…"` reaches
/// CI as one run-on line. Character references survive normalization intact,
/// which is why these three are encoded numerically.
fn escape_attr(text: &str) -> String {
    escape_text(text)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
        .replace('\t', "&#9;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(events: &[Event]) -> String {
        let mut buf = Vec::new();
        {
            let mut junit = Junit::new(&mut buf);
            junit.begin_suite("unit", "native");
            let mut totals = Totals::default();
            for event in events {
                totals.record(event);
                junit.on_event(event);
            }
            junit.finish(&totals, Duration::from_millis(120));
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn emits_valid_shell_with_counts() {
        let out = render(&[
            Event::TestPass {
                path: vec!["math".into()],
                name: "adds".into(),
                duration_ms: 1.5,
            },
            Event::TestSkip {
                path: vec![],
                name: "later".into(),
                reason: Some("wip".into()),
            },
        ]);
        assert!(out.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
        assert!(out.contains(r#"<testsuites name="lest" tests="2" failures="0" skipped="1""#));
        assert!(out.contains(r#"<testsuite name="unit (native)" tests="2""#));
        assert!(out.contains(r#"<testcase name="adds" classname="math" time="0.002"/>"#));
        assert!(out.contains(r#"<skipped message="wip"/>"#));
        assert!(out.trim_end().ends_with("</testsuites>"));
    }

    #[test]
    fn writes_failure_element_with_escaped_content() {
        let out = render(&[Event::TestFail {
            path: vec!["a & b".into()],
            name: "breaks <it>".into(),
            duration_ms: 2.0,
            failure: Failure::Assertion {
                message: "expected \"x\" to be \"y\"".into(),
                expected: Some("y".into()),
                received: Some("x".into()),
            },
            origin: None,
        }]);
        assert!(out.contains(r#"<testsuites name="lest" tests="1" failures="1""#));
        assert!(out.contains("classname=\"a &amp; b\""));
        assert!(out.contains("name=\"breaks &lt;it&gt;\""));
        assert!(out.contains("&quot;x&quot;"));
        assert!(out.contains(r#"type="assertion""#));
        assert!(out.contains("expected: y"));
        assert!(out.contains("received: x"));
    }

    #[test]
    fn error_failure_carries_trace() {
        let out = render(&[Event::TestFail {
            path: vec!["s".into()],
            name: "explodes".into(),
            duration_ms: 0.0,
            failure: Failure::Error {
                message: "boom".into(),
                trace: Some("at foo\nat bar".into()),
            },
            origin: None,
        }]);
        assert!(out.contains(r#"type="error""#));
        assert!(out.contains("at foo\nat bar"));
    }

    #[test]
    fn snapshot_failure_is_a_real_testcase_and_counts() {
        // The regression: a snapshot-only failure used to ship XML claiming
        // failures="0" beside exit 1.
        let mut buf = Vec::new();
        {
            let mut junit = Junit::new(&mut buf);
            junit.begin_suite("unit", "native");
            junit.snapshot_failure("src/math.spec.luau", "adds > result", "- 3\n+ 4\n");
            junit.finish(&Totals::default(), Duration::from_millis(5));
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains(r#"<testsuites name="lest" tests="1" failures="1""#));
        assert!(out.contains(r#"name="snapshot: adds &gt; result""#));
        assert!(out.contains(r#"type="snapshot""#));
        assert!(out.contains("+ 4"));
    }

    /// The regression: a backend that died mid-suite left a document claiming
    /// the suite passed beside exit 2. It is an `<error>` (harness broke),
    /// not a `<failure>` (assertion verdict) — that is JUnit's own split.
    #[test]
    fn a_suite_error_is_an_error_element_with_counts() {
        let mut buf = Vec::new();
        {
            let mut junit = Junit::new(&mut buf);
            junit.begin_suite("scripts", "lune");
            junit.suite_error("the suite did not finish: lune exited with exit code: 1");
            junit.finish(&Totals::default(), Duration::from_millis(5));
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains(r#"errors="1""#), "{out}");
        assert!(
            out.contains(r#"<testcase name="(suite)" classname="scripts""#),
            "{out}"
        );
        assert!(
            out.contains(r#"<error message="the suite did not finish: lune exited with exit code: 1" type="tool"/>"#),
            "{out}"
        );
        // An error is not a failure; the counts stay distinct.
        assert!(
            out.contains(r#"tests="1" failures="0" skipped="0" errors="1""#),
            "{out}"
        );
    }

    #[test]
    fn strips_control_characters_and_encodes_attribute_whitespace() {
        let out = render(&[Event::TestFail {
            // An ANSI escape in a test name would otherwise produce a document
            // no XML parser will accept.
            path: vec!["ansi".into()],
            name: "colors \u{1b}[31mred\u{1b}[0m".into(),
            duration_ms: 0.0,
            failure: Failure::Error {
                message: "line one\nline two".into(),
                trace: None,
            },
            origin: None,
        }]);
        assert!(!out.contains('\u{1b}'), "raw ESC survived into the XML");
        // A literal newline in an attribute would be normalized to a space.
        assert!(out.contains(r#"message="line one&#10;line two""#));
        assert!(!out.contains("message=\"line one\nline two\""));
    }
}
