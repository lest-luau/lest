use std::io::Write;
use std::time::Duration;

use serde_json::json;

use super::{Event, Totals};

/// Machine-readable reporter: one JSON object per line. Every protocol event
/// is emitted verbatim with `suite` and `backend` fields injected, framed by
/// `note` and final `summary` objects. CI consumes this; humans get `pretty`.
pub struct Json<W: Write> {
    out: W,
    suite: Option<(String, String)>,
}

impl<W: Write> Json<W> {
    pub fn new(out: W) -> Self {
        Self { out, suite: None }
    }

    pub fn begin_suite(&mut self, name: &str, backend: &str) {
        self.suite = Some((name.to_string(), backend.to_string()));
    }

    pub fn note(&mut self, message: &str) {
        let _ = writeln!(
            self.out,
            "{}",
            json!({ "kind": "note", "message": message })
        );
    }

    /// A snapshot mismatch, as its own line kind with the parts split out.
    ///
    /// Snapshot comparison is a host decision, so this is not an `Event` and
    /// does not touch the protocol schema — but a consumer gating on failures
    /// still needs it, and folding it into `note` left the key and the spec
    /// buried in a prose blob only a human could read.
    pub fn snapshot_failure(&mut self, spec: &str, key: &str, detail: &str) {
        let mut value = json!({
            "kind": "snapshot_failure",
            "spec": spec,
            "key": key,
            "detail": detail,
        });
        if let (Some((suite, backend)), Some(object)) = (&self.suite, value.as_object_mut()) {
            object.insert("suite".to_string(), json!(suite));
            object.insert("backend".to_string(), json!(backend));
        }
        let _ = writeln!(self.out, "{value}");
    }

    /// A suite-level tool failure — the backend died mid-suite — as its own
    /// line kind, tagged like every other line. Not an `Event` for the same
    /// reason `snapshot_failure` is not: it is a host verdict, not a
    /// framework fact.
    pub fn suite_error(&mut self, message: &str) {
        let mut value = json!({ "kind": "suite_error", "message": message });
        if let (Some((suite, backend)), Some(object)) = (&self.suite, value.as_object_mut()) {
            object.insert("suite".to_string(), json!(suite));
            object.insert("backend".to_string(), json!(backend));
        }
        let _ = writeln!(self.out, "{value}");
    }

    pub fn on_event(&mut self, event: &Event) {
        let mut value = match serde_json::to_value(event) {
            Ok(value) => value,
            Err(err) => {
                json!({ "kind": "note", "message": format!("unserializable event: {err}") })
            }
        };
        if let (Some((suite, backend)), Some(object)) = (&self.suite, value.as_object_mut()) {
            object.insert("suite".to_string(), json!(suite));
            object.insert("backend".to_string(), json!(backend));
        }
        let _ = writeln!(self.out, "{value}");
    }

    pub fn finish(&mut self, totals: &Totals, elapsed: Duration) {
        let _ = writeln!(
            self.out,
            "{}",
            json!({
                "kind": "summary",
                "passed": totals.passed,
                "failed": totals.failed,
                "skipped": totals.skipped,
                "durationMs": elapsed.as_secs_f64() * 1000.0,
            })
        );
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_events_with_suite_and_backend() {
        let mut buf = Vec::new();
        {
            let mut reporter = Json::new(&mut buf);
            reporter.begin_suite("unit", "native");
            reporter.on_event(&Event::TestPass {
                path: vec!["math".into()],
                name: "adds".into(),
                duration_ms: 0.5,
            });
            reporter.finish(
                &Totals {
                    passed: 1,
                    failed: 0,
                    skipped: 0,
                },
                Duration::from_millis(10),
            );
        }
        let text = String::from_utf8(buf).unwrap();
        let mut lines = text.lines();
        let event: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(event["kind"], "test_pass");
        assert_eq!(event["suite"], "unit");
        assert_eq!(event["backend"], "native");
        let summary: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(summary["kind"], "summary");
        assert_eq!(summary["passed"], 1);
    }

    #[test]
    fn suite_error_is_structured_and_tagged() {
        let mut buf = Vec::new();
        {
            let mut reporter = Json::new(&mut buf);
            reporter.begin_suite("scripts", "lune");
            reporter.suite_error("the suite did not finish: lune exited");
        }
        let text = String::from_utf8(buf).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(line["kind"], "suite_error");
        assert_eq!(line["message"], "the suite did not finish: lune exited");
        assert_eq!(line["suite"], "scripts");
        assert_eq!(line["backend"], "lune");
    }

    #[test]
    fn snapshot_failure_is_structured() {
        let mut buf = Vec::new();
        {
            let mut reporter = Json::new(&mut buf);
            reporter.begin_suite("unit", "native");
            reporter.snapshot_failure("src/math.spec.luau", "adds > result", "- 3\n+ 4\n");
        }
        let text = String::from_utf8(buf).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(line["kind"], "snapshot_failure");
        assert_eq!(line["key"], "adds > result");
        assert_eq!(line["spec"], "src/math.spec.luau");
        assert_eq!(line["suite"], "unit");
    }
}
