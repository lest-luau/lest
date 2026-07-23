use serde::{Deserialize, Serialize};

/// Bumped when the event schema changes in a way out-of-process backends can
/// observe. Shipped in `run_start` from day one so version skew between the
/// CLI and an older framework copy is detectable.
pub const PROTOCOL_VERSION: u32 = 1;

/// A `protocolVersion` on the wire that this build does not speak. The CLI owns
/// what to do about it (warn, refuse, exit code); this type only carries the
/// comparison so that policy lives in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolVersionMismatch {
    /// The version this build of `lest-report` implements ([`PROTOCOL_VERSION`]).
    pub expected: u32,
    /// The version reported by the framework in its `run_start` event.
    pub incoming: u32,
}

impl std::fmt::Display for ProtocolVersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "protocol version mismatch: this build speaks {}, framework emitted {}",
            self.expected, self.incoming
        )
    }
}

impl std::error::Error for ProtocolVersionMismatch {}

/// Compares an incoming `protocolVersion` (from a `run_start` event) against the
/// version this build implements. Returns `Ok(())` when they match. The CLI
/// decides how to react to a mismatch — this helper only performs the check so
/// the comparison isn't duplicated across backends.
pub fn check_protocol_version(incoming: u32) -> Result<(), ProtocolVersionMismatch> {
    if incoming == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolVersionMismatch {
            expected: PROTOCOL_VERSION,
            incoming,
        })
    }
}

/// One event in the result protocol. The framework reports facts; the host
/// makes decisions. In-process these arrive as Luau tables through an mlua
/// callback; out-of-process backends ship the same schema as JSON lines, so
/// the serde names here are the wire format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    RunStart {
        spec_count: u32,
        protocol_version: u32,
    },
    SuiteStart {
        path: Vec<String>,
    },
    TestStart {
        path: Vec<String>,
        name: String,
    },
    TestPass {
        path: Vec<String>,
        name: String,
        duration_ms: f64,
    },
    TestFail {
        path: Vec<String>,
        name: String,
        duration_ms: f64,
        failure: Failure,
        /// The hook the failure happened in (`beforeAll`, `beforeEach`,
        /// `afterEach`) when it was not the test body itself. Without it a
        /// reader has to notice that the failing line sits outside the test
        /// to realize setup broke, not the test. Additive: absent from older
        /// frameworks and from failures in the body, and kept off the wire
        /// when unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    TestSkip {
        path: Vec<String>,
        name: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Snapshot {
        path: Vec<String>,
        name: String,
        key: String,
        received: String,
    },
    RunEnd {
        passed: u32,
        failed: u32,
        skipped: u32,
    },
}

/// Why a test failed. A `property` variant is reserved for the post-1.0
/// property engine; adding it is an additive protocol change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Failure {
    Assertion {
        message: String,
        #[serde(default)]
        expected: Option<String>,
        #[serde(default)]
        received: Option<String>,
    },
    Error {
        message: String,
        /// Optional because an absent traceback is a real state, not a decode
        /// hole: a host that catches an error after the stack has unwound has
        /// no trace to report. Keeping it `String` with `#[serde(default)]`
        /// silently turned "no trace" into "empty trace" and made the field
        /// look non-optional while behaving otherwise.
        #[serde(default)]
        trace: Option<String>,
    },
    /// Host-synthesized: the framework only reports the *fact* of a snapshot
    /// and cannot produce this variant — comparison lives CLI-side, and when
    /// it fails, the host folds its verdict back into the test's result by
    /// rewriting the streamed `test_pass` into a `test_fail` carrying this.
    /// It therefore appears only in the merged output stream (JSON consumers
    /// included), never on the framework wire, so the protocol stays v1.
    Snapshot { mismatches: Vec<SnapshotMismatch> },
}

/// One mismatched `toMatchSnapshot` call inside a [`Failure::Snapshot`] — a
/// test can hold several, so each carries its storage key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMismatch {
    /// The snapshot's storage key (`<describe path> > <test> <hint-or-counter>`).
    pub key: String,
    /// Already-rendered failure body: the stored-vs-received line diff, or
    /// prose for a non-diff failure (a duplicate key's explanation).
    pub detail: String,
}

/// Pass/fail/skip counts aggregated across every backend in a run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

impl Totals {
    /// Counts test outcomes; all other events (including per-run `run_end`
    /// summaries from individual backends) are ignored so the CLI's own
    /// aggregation is the single source of truth.
    pub fn record(&mut self, event: &Event) {
        match event {
            Event::TestPass { .. } => self.passed += 1,
            Event::TestFail { .. } => self.failed += 1,
            Event::TestSkip { .. } => self.skipped += 1,
            _ => {}
        }
    }

    pub fn total(&self) -> u32 {
        self.passed + self.failed + self.skipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_test_fail_json_line() {
        let line = r#"{"kind":"test_fail","path":["math","add"],"name":"adds","durationMs":1.5,"failure":{"type":"assertion","message":"expected 1 to equal 2","expected":"2","received":"1"}}"#;
        let event: Event = serde_json::from_str(line).unwrap();
        assert_eq!(
            event,
            Event::TestFail {
                path: vec!["math".into(), "add".into()],
                name: "adds".into(),
                duration_ms: 1.5,
                failure: Failure::Assertion {
                    message: "expected 1 to equal 2".into(),
                    expected: Some("2".into()),
                    received: Some("1".into()),
                },
                origin: None,
            }
        );
    }

    /// `origin` is additive: absent decodes to `None`, present survives, and
    /// an unset one stays off the wire when re-encoded.
    #[test]
    fn origin_field_is_additive() {
        let line = r#"{"kind":"test_fail","path":[],"name":"t","durationMs":0,"origin":"beforeAll","failure":{"type":"error","message":"boom"}}"#;
        let event: Event = serde_json::from_str(line).unwrap();
        match &event {
            Event::TestFail { origin, .. } => assert_eq!(origin.as_deref(), Some("beforeAll")),
            other => panic!("unexpected event: {other:?}"),
        }
        let without = Event::TestFail {
            path: vec![],
            name: "t".into(),
            duration_ms: 0.0,
            failure: Failure::Error {
                message: "boom".into(),
                trace: None,
            },
            origin: None,
        };
        let encoded = serde_json::to_string(&without).unwrap();
        assert!(!encoded.contains("origin"), "{encoded}");
    }

    #[test]
    fn decodes_run_start_with_camel_case_fields() {
        let line = r#"{"kind":"run_start","specCount":3,"protocolVersion":1}"#;
        let event: Event = serde_json::from_str(line).unwrap();
        assert_eq!(
            event,
            Event::RunStart {
                spec_count: 3,
                protocol_version: 1
            }
        );
    }

    #[test]
    fn decodes_test_skip_without_reason() {
        let line = r#"{"kind":"test_skip","path":[],"name":"later"}"#;
        let event: Event = serde_json::from_str(line).unwrap();
        assert_eq!(
            event,
            Event::TestSkip {
                path: vec![],
                name: "later".into(),
                reason: None
            }
        );
    }

    #[test]
    fn decodes_error_failure_with_missing_trace() {
        let line = r#"{"kind":"test_fail","path":["s"],"name":"(load)","durationMs":0,"failure":{"type":"error","message":"boom"}}"#;
        let event: Event = serde_json::from_str(line).unwrap();
        match event {
            Event::TestFail {
                failure: Failure::Error { message, trace },
                ..
            } => {
                assert_eq!(message, "boom");
                assert_eq!(trace, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn protocol_version_check_matches_current() {
        assert!(check_protocol_version(PROTOCOL_VERSION).is_ok());
        let err = check_protocol_version(PROTOCOL_VERSION + 1).unwrap_err();
        assert_eq!(err.expected, PROTOCOL_VERSION);
        assert_eq!(err.incoming, PROTOCOL_VERSION + 1);
    }

    #[test]
    fn totals_count_only_test_outcomes() {
        let mut totals = Totals::default();
        totals.record(&Event::RunStart {
            spec_count: 2,
            protocol_version: PROTOCOL_VERSION,
        });
        totals.record(&Event::TestPass {
            path: vec![],
            name: "a".into(),
            duration_ms: 0.1,
        });
        totals.record(&Event::TestSkip {
            path: vec![],
            name: "b".into(),
            reason: None,
        });
        totals.record(&Event::RunEnd {
            passed: 1,
            failed: 0,
            skipped: 1,
        });
        assert_eq!(
            totals,
            Totals {
                passed: 1,
                failed: 0,
                skipped: 1
            }
        );
    }
}
