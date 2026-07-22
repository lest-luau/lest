//! The Open Cloud Luau-execution client.
//!
//! Verified contract (Open Cloud v2, probed live against a real place):
//!   * Auth header `x-api-key: <key>`; base `https://apis.roblox.com/cloud/v2`.
//!   * Submit: `POST /universes/{u}/places/{p}/luau-execution-session-tasks`
//!     with body `{"script": "<luau>"}` → a task JSON carrying `path` and
//!     `state` (`QUEUED|PROCESSING|COMPLETE|FAILED|CANCELLED`).
//!   * Poll: `GET /{task.path}` until the state leaves QUEUED/PROCESSING.
//!   * On COMPLETE the task JSON carries `output.results`, a JSON array of the
//!     script's return values; our entrypoint returns the events array, so
//!     `output.results[0]` is the protocol event list.
//!   * On FAILED/CANCELLED we also fetch `GET /{task.path}/logs` and fold the
//!     log text into the error so a broken script is diagnosable.
//!
//! HTTP lives behind the [`Transport`] trait so the submit/poll/decode state
//! machine is unit-testable without a network.

use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::ToolError;

pub const DEFAULT_BASE: &str = "https://apis.roblox.com/cloud/v2";

/// One HTTP exchange. `body` is `Some` for POST, `None` for GET.
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// A completed HTTP exchange: the numeric status and the response body text,
/// captured for both success and error responses (an error body from Open
/// Cloud carries a diagnostic JSON we surface verbatim).
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    /// The server's `Retry-After`, when it sent one, so a rate-limited poll
    /// waits as long as it was told to rather than guessing. Only the
    /// delta-seconds form is understood; the HTTP-date form is vanishingly rare
    /// for rate limiting and does not justify a date parser here.
    pub retry_after: Option<Duration>,
}

impl HttpResponse {
    /// The real transport fills `retry_after` from the response headers; only
    /// the test fakes build a response without one.
    #[cfg(test)]
    pub fn new(status: u16, body: String) -> Self {
        HttpResponse {
            status,
            body,
            retry_after: None,
        }
    }
}

/// The seam between the cloud state machine and the wire. The real
/// implementation ([`UreqTransport`]) uses blocking `ureq`; tests substitute a
/// scripted transport so the polling logic runs without a network.
pub trait Transport {
    fn send(&self, request: HttpRequest, api_key: &str) -> Result<HttpResponse, ToolError>;
}

/// The blocking `ureq`-backed transport. No async runtime — a request is one
/// synchronous call, matching lest's no-async ethos.
///
/// The agent is built once and held: `ureq` pools connections per agent, and a
/// cloud suite is a long sequence of small requests to one host (a submit and
/// several polls per spec file). A fresh agent per request would hand every one
/// of them a fresh TLS handshake.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new() -> Self {
        UreqTransport {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .build(),
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        UreqTransport::new()
    }
}

/// Reads `Retry-After`'s delta-seconds form off a response, ignoring the
/// HTTP-date form and anything unparseable.
fn retry_after_of(response: &ureq::Response) -> Option<Duration> {
    response
        .header("Retry-After")
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

impl Transport for UreqTransport {
    fn send(&self, request: HttpRequest, api_key: &str) -> Result<HttpResponse, ToolError> {
        let req = match request.method {
            Method::Get => self.agent.get(&request.url),
            Method::Post => self.agent.post(&request.url),
        }
        .set("x-api-key", api_key);

        let result = match request.body {
            Some(body) => req
                .set("Content-Type", "application/json")
                .send_string(&body),
            None => req.call(),
        };

        match result {
            Ok(response) => {
                let status = response.status();
                // Read headers before `into_string`, which consumes the response.
                let retry_after = retry_after_of(&response);
                let body = response.into_string().map_err(|e| {
                    ToolError(format!("cannot read the Open Cloud response body: {e}"))
                })?;
                Ok(HttpResponse {
                    status,
                    body,
                    retry_after,
                })
            }
            // A non-2xx status is a normal outcome we want to inspect (the body
            // carries Open Cloud's error JSON), not a transport failure.
            Err(ureq::Error::Status(status, response)) => {
                let retry_after = retry_after_of(&response);
                let body = response.into_string().unwrap_or_default();
                Ok(HttpResponse {
                    status,
                    body,
                    retry_after,
                })
            }
            Err(ureq::Error::Transport(t)) => {
                Err(ToolError(format!("cannot reach Open Cloud: {t}")))
            }
        }
    }
}

/// A submitted or polled Luau-execution task.
#[derive(Debug, Clone)]
pub struct Task {
    /// Resource path, e.g. `universes/{u}/places/{p}/.../tasks/{id}`. Poll and
    /// logs URLs are built from it.
    pub path: String,
    pub state: TaskState,
    /// The full task JSON as returned, so `output.results` can be read on
    /// completion without a second request.
    pub raw: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Processing,
    Complete,
    Failed,
    Cancelled,
    /// A state string the API introduced that this build does not know; treated
    /// as terminal so polling never spins forever on it.
    Unknown,
}

impl TaskState {
    fn parse(s: &str) -> Self {
        match s {
            "QUEUED" | "STATE_QUEUED" => TaskState::Queued,
            "PROCESSING" | "STATE_PROCESSING" => TaskState::Processing,
            "COMPLETE" | "STATE_COMPLETE" | "COMPLETED" => TaskState::Complete,
            "FAILED" | "STATE_FAILED" => TaskState::Failed,
            "CANCELLED" | "STATE_CANCELLED" | "CANCELED" => TaskState::Cancelled,
            _ => TaskState::Unknown,
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, TaskState::Queued | TaskState::Processing)
    }
}

/// One Open Cloud session bound to a place. Owns the transport, credentials,
/// and the identifiers the URLs are built from.
///
/// There is deliberately no configurable base URL: the seam tests need is
/// [`Transport`], which intercepts the request before it reaches a host, so a
/// per-session base would be a field that is only ever the constant.
pub struct Session<'a, T: Transport> {
    transport: &'a T,
    api_key: &'a str,
    universe_id: &'a str,
    place_id: &'a str,
}

impl<'a, T: Transport> Session<'a, T> {
    pub fn new(
        transport: &'a T,
        api_key: &'a str,
        universe_id: &'a str,
        place_id: &'a str,
    ) -> Self {
        Session {
            transport,
            api_key,
            universe_id,
            place_id,
        }
    }

    /// Submits a script as a new Luau-execution session task, retrying
    /// transient failures against `deadline`.
    ///
    /// The retry rationale on [`poll_to_completion`](Self::poll_to_completion)
    /// applies verbatim here: Open Cloud rate-limits, CI auto-enables this
    /// backend, and cloud submits once per spec file sequentially — so one 429
    /// or 503 on submit would otherwise hard-fail the whole suite. A retried
    /// POST can at worst leave an orphaned duplicate task behind, which the
    /// deadline bounds and Open Cloud expires; a hard-failed CI run cannot be
    /// bounded by anything.
    pub fn submit(&self, script: &str, deadline: Instant) -> Result<Task, ToolError> {
        let mut delay = Duration::from_millis(600);
        let cap = Duration::from_secs(5);
        loop {
            match self.submit_attempt(script) {
                Ok(task) => return Ok(task),
                Err(RequestFailure::Fatal(err)) => return Err(err),
                Err(RequestFailure::Transient { error, retry_after }) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        // Retrying past the deadline only delays the verdict;
                        // the last transient error is the most useful report.
                        return Err(error);
                    }
                    // One sleep per attempt — the backoff delay, or the
                    // server's Retry-After when it asked for longer, never
                    // both stacked — and never past the deadline.
                    let wait = retry_after.map_or(delay, |requested| requested.max(delay));
                    sleep(wait.min(remaining));
                    delay = (delay * 2).min(cap);
                }
            }
        }
    }

    /// One submit attempt, classifying its failure so [`submit`](Self::submit)
    /// can decide whether to try again.
    fn submit_attempt(&self, script: &str) -> Result<Task, RequestFailure> {
        let url = format!(
            "{DEFAULT_BASE}/universes/{}/places/{}/luau-execution-session-tasks",
            self.universe_id, self.place_id
        );
        let body = serde_json::json!({ "script": script }).to_string();
        let response = self
            .transport
            .send(
                HttpRequest {
                    method: Method::Post,
                    url,
                    body: Some(body),
                },
                self.api_key,
            )
            // A transport-level blip is worth another attempt, same as a poll.
            .map_err(|error| RequestFailure::Transient {
                error,
                retry_after: None,
            })?;
        if !(200..300).contains(&response.status) {
            let error = self.status_error("submitting the execution task", &response);
            return Err(if is_retryable(response.status) {
                RequestFailure::Transient {
                    error,
                    retry_after: response.retry_after,
                }
            } else {
                RequestFailure::Fatal(error)
            });
        }
        parse_task(&response.body).map_err(RequestFailure::Fatal)
    }

    /// One poll, classifying its failure so the caller can decide whether to
    /// try again.
    fn poll_attempt(&self, task_path: &str) -> Result<Task, RequestFailure> {
        let url = format!("{DEFAULT_BASE}/{task_path}");
        let response = self
            .transport
            .send(
                HttpRequest {
                    method: Method::Get,
                    url,
                    body: None,
                },
                self.api_key,
            )
            // A transport-level failure (DNS, reset connection, read timeout) is
            // exactly the kind of blip another poll fixes.
            .map_err(|err| RequestFailure::Transient {
                error: err,
                retry_after: None,
            })?;
        if !(200..300).contains(&response.status) {
            let error = self.status_error("polling the execution task", &response);
            return Err(if is_retryable(response.status) {
                RequestFailure::Transient {
                    error,
                    retry_after: response.retry_after,
                }
            } else {
                RequestFailure::Fatal(error)
            });
        }
        parse_task(&response.body).map_err(RequestFailure::Fatal)
    }

    /// Fetches the task's logs (stdout/print output). Best-effort: a failure to
    /// retrieve logs returns an explanatory placeholder rather than masking the
    /// original error being diagnosed.
    pub fn logs(&self, task_path: &str) -> String {
        let url = format!("{DEFAULT_BASE}/{task_path}/logs");
        match self.transport.send(
            HttpRequest {
                method: Method::Get,
                url,
                body: None,
            },
            self.api_key,
        ) {
            Ok(response) if (200..300).contains(&response.status) => {
                extract_log_text(&response.body)
            }
            Ok(response) => format!(
                "(could not fetch logs: HTTP {} {})",
                response.status,
                response.body.trim()
            ),
            Err(err) => format!("(could not fetch logs: {err})"),
        }
    }

    /// Submits `script`, polls to completion (bounded delay/backoff and an
    /// overall deadline), and returns the decoded `output.results[0]` array —
    /// the protocol events our entrypoint returned. A FAILED/CANCELLED task, or
    /// a timeout, is a tool error with the task's logs folded in.
    pub fn run_script(&self, script: &str, overall: Duration) -> Result<Vec<Value>, ToolError> {
        // Guarded, not bare `+`: `overall` derives from unvalidated user
        // config, and `Instant + Duration` panics on overflow — exit 101 would
        // bypass the exit-code policy entirely (the same guard its siblings in
        // `cloud::run_with_transport` and `backend::runtime` carry).
        let deadline = Instant::now()
            .checked_add(overall)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        let task = self.submit(script, deadline)?;
        let task = self.poll_to_completion(task, deadline)?;

        match task.state {
            TaskState::Complete => extract_results(&task.raw).ok_or_else(|| {
                ToolError(format!(
                    "the Open Cloud task completed but returned no output.results — logs:\n{}",
                    self.logs(&task.path)
                ))
            }),
            TaskState::Failed | TaskState::Cancelled | TaskState::Unknown => {
                let reason = task_error_message(&task.raw);
                Err(ToolError(format!(
                    "Open Cloud Luau execution {} — {reason}\n--- task logs ---\n{}",
                    state_word(task.state),
                    self.logs(&task.path)
                )))
            }
            // Non-terminal states cannot reach here: poll_to_completion loops.
            TaskState::Queued | TaskState::Processing => unreachable!(),
        }
    }

    /// Polls `task` until it reaches a terminal state or `deadline` passes,
    /// starting at a short delay and backing off up to a cap.
    ///
    /// A transient poll failure does **not** end the run. Open Cloud rate-limits
    /// and cloud polls once per spec file sequentially, so one 429 or 503 would
    /// otherwise abort every remaining suite *and* orphan an engine task that is
    /// still running. Transient failures are retried against the same overall
    /// deadline (the run's real bound), honouring `Retry-After` when the server
    /// sent one; a non-retryable 4xx — a bad key, a place that does not exist —
    /// stays a hard error, because retrying it only wastes the deadline.
    fn poll_to_completion(&self, mut task: Task, deadline: Instant) -> Result<Task, ToolError> {
        let mut delay = Duration::from_millis(600);
        let cap = Duration::from_secs(5);
        let mut last_transient: Option<ToolError> = None;
        // The server's Retry-After from the previous attempt, if it sent one.
        let mut server_wait: Option<Duration> = None;
        loop {
            if task.state.is_terminal() {
                return Ok(task);
            }
            if Instant::now() >= deadline {
                let reason = match last_transient {
                    Some(err) => format!(" (last poll error: {err})"),
                    None => String::new(),
                };
                return Err(ToolError(format!(
                    "the Open Cloud execution task did not finish within the deadline (last \
                     state: {}){reason} — logs:\n{}",
                    state_word(task.state),
                    self.logs(&task.path)
                )));
            }
            // One sleep per attempt: the backoff delay, or the server's
            // Retry-After when it asked for longer — never both stacked, which
            // would wait `delay + retry_after` for a single retry. And never
            // past the deadline: the loop must get back to the top to report
            // the timeout.
            let wait = server_wait
                .take()
                .map_or(delay, |requested| requested.max(delay));
            let remaining = deadline.saturating_duration_since(Instant::now());
            sleep(wait.min(remaining));
            delay = (delay * 2).min(cap);
            match self.poll_attempt(&task.path) {
                Ok(next) => {
                    task = next;
                    last_transient = None;
                }
                Err(RequestFailure::Fatal(err)) => return Err(err),
                Err(RequestFailure::Transient { error, retry_after }) => {
                    // Keep the state we already know and try again; the deadline
                    // check at the top of the loop is what ends this.
                    last_transient = Some(error);
                    server_wait = retry_after;
                }
            }
        }
    }

    fn status_error(&self, action: &str, response: &HttpResponse) -> ToolError {
        ToolError(format!(
            "Open Cloud returned HTTP {} while {action}: {}",
            response.status,
            response.body.trim()
        ))
    }
}

/// Why one request attempt (a submit or a poll) failed, and whether trying
/// again could plausibly help.
enum RequestFailure {
    /// Rate limiting, a gateway hiccup, a request timeout, or a transport-level
    /// blip — worth another attempt inside the run's deadline.
    Transient {
        error: ToolError,
        retry_after: Option<Duration>,
    },
    /// A definite failure: a rejected key, a place that does not exist, a
    /// response that will not parse. Retrying only burns the deadline.
    Fatal(ToolError),
}

/// Statuses worth another attempt: request timeout, rate limit, and the 5xx
/// family. Every other 4xx describes something a retry cannot change.
fn is_retryable(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

fn state_word(state: TaskState) -> &'static str {
    match state {
        TaskState::Queued => "QUEUED",
        TaskState::Processing => "PROCESSING",
        TaskState::Complete => "COMPLETE",
        TaskState::Failed => "FAILED",
        TaskState::Cancelled => "CANCELLED",
        TaskState::Unknown => "an unrecognized state",
    }
}

/// Parses a task resource JSON into a [`Task`], tolerating the two shapes Open
/// Cloud uses for the resource identifier (`path`, or the newer `name`).
fn parse_task(body: &str) -> Result<Task, ToolError> {
    let raw: Value = serde_json::from_str(body).map_err(|e| {
        ToolError(format!(
            "the Open Cloud task response was not JSON: {e}\nbody: {body}"
        ))
    })?;
    let path = raw
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| raw.get("name").and_then(Value::as_str))
        .ok_or_else(|| {
            ToolError(format!(
                "the Open Cloud task response is missing a resource path\nbody: {body}"
            ))
        })?
        .to_string();
    let state = raw
        .get("state")
        .and_then(Value::as_str)
        .map(TaskState::parse)
        .unwrap_or(TaskState::Unknown);
    Ok(Task { path, state, raw })
}

/// Extracts `output.results` (an array) from a completed task JSON.
fn extract_results(raw: &Value) -> Option<Vec<Value>> {
    raw.get("output")
        .and_then(|o| o.get("results"))
        .and_then(Value::as_array)
        .cloned()
}

/// Pulls a human-readable failure message out of a task's `error` field,
/// whatever nested shape the API used.
fn task_error_message(raw: &Value) -> String {
    if let Some(error) = raw.get("error") {
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            let code = error.get("code").and_then(Value::as_str).unwrap_or("");
            if code.is_empty() {
                return message.to_string();
            }
            return format!("{code}: {message}");
        }
        return error.to_string();
    }
    "no error detail provided".to_string()
}

/// Extracts printable log text from a logs response, tolerating the
/// `luauExecutionSessionTaskLogs` list shape (each entry carrying `messages`)
/// as well as a plain string or array fallback.
fn extract_log_text(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.trim().to_string();
    };
    // The documented shape: { luauExecutionSessionTaskLogs: [ { messages: [..] } ] }
    if let Some(list) = value
        .get("luauExecutionSessionTaskLogs")
        .and_then(Value::as_array)
    {
        let mut lines = Vec::new();
        for entry in list {
            if let Some(messages) = entry.get("messages").and_then(Value::as_array) {
                for message in messages {
                    if let Some(text) = message.as_str() {
                        lines.push(text.to_string());
                    } else {
                        lines.push(message.to_string());
                    }
                }
            }
        }
        if lines.is_empty() {
            return "(no log messages)".to_string();
        }
        return lines.join("\n");
    }
    // Fallbacks: a bare messages array, or the raw body.
    if let Some(messages) = value.get("messages").and_then(Value::as_array) {
        return messages
            .iter()
            .map(|m| {
                m.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| m.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    body.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A scripted transport: each call pops the next canned response, letting a
    /// test drive the submit→poll→results state machine deterministically.
    struct ScriptedTransport {
        responses: RefCell<Vec<HttpResponse>>,
        seen: RefCell<Vec<(Method, String)>>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<HttpResponse>) -> Self {
            ScriptedTransport {
                responses: RefCell::new(responses),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for ScriptedTransport {
        fn send(&self, request: HttpRequest, _api_key: &str) -> Result<HttpResponse, ToolError> {
            self.seen.borrow_mut().push((request.method, request.url));
            Ok(self.responses.borrow_mut().remove(0))
        }
    }

    fn ok(body: &str) -> HttpResponse {
        HttpResponse::new(200, body.to_string())
    }

    fn status(code: u16, body: &str) -> HttpResponse {
        HttpResponse::new(code, body.to_string())
    }

    #[test]
    fn parses_task_path_and_state() {
        let task = parse_task(
            r#"{"path":"universes/1/places/2/luau-execution-session-tasks/abc","state":"QUEUED"}"#,
        )
        .unwrap();
        assert_eq!(
            task.path,
            "universes/1/places/2/luau-execution-session-tasks/abc"
        );
        assert_eq!(task.state, TaskState::Queued);
    }

    #[test]
    fn run_script_polls_until_complete_and_returns_results() {
        let submit = ok(r#"{"path":"universes/1/places/2/tasks/abc","state":"QUEUED"}"#);
        let processing = ok(r#"{"path":"universes/1/places/2/tasks/abc","state":"PROCESSING"}"#);
        let complete = ok(
            r#"{"path":"universes/1/places/2/tasks/abc","state":"COMPLETE",
                "output":{"results":[[{"kind":"run_end","passed":1,"failed":0,"skipped":0}]]}}"#,
        );
        let transport = ScriptedTransport::new(vec![submit, processing, complete]);
        let session = Session::new(&transport, "key", "1", "2");
        // A tiny overall deadline is fine: the scripted transport advances state
        // on every poll, so completion is reached within two polls.
        let results = session
            .run_script("return {}", Duration::from_secs(30))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_array(), "results[0] is the events array");
        // Submit + two polls.
        assert_eq!(transport.seen.borrow().len(), 3);
    }

    #[test]
    fn failed_task_folds_in_logs() {
        let submit = ok(r#"{"path":"t/abc","state":"QUEUED"}"#);
        let failed = ok(
            r#"{"path":"t/abc","state":"FAILED","error":{"code":"LuauError","message":"boom"}}"#,
        );
        let logs = ok(
            r#"{"luauExecutionSessionTaskLogs":[{"messages":["stack traceback: boom at line 3"]}]}"#,
        );
        let transport = ScriptedTransport::new(vec![submit, failed, logs]);
        let session = Session::new(&transport, "key", "1", "2");
        let err = session
            .run_script("error('boom')", Duration::from_secs(30))
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("FAILED"), "{message}");
        assert!(message.contains("boom"), "{message}");
        assert!(
            message.contains("stack traceback"),
            "logs folded in: {message}"
        );
    }

    #[test]
    fn a_rate_limited_poll_is_retried_not_fatal() {
        // Open Cloud rate-limits, and cloud polls once per spec sequentially.
        // One 429 must not abort the suite and orphan the running engine task.
        let submit = ok(r#"{"path":"t/abc","state":"QUEUED"}"#);
        let throttled = status(429, r#"{"message":"Too many requests"}"#);
        let unavailable = status(503, "");
        let complete = ok(r#"{"path":"t/abc","state":"COMPLETE",
                "output":{"results":[[{"kind":"run_end","passed":1,"failed":0,"skipped":0}]]}}"#);
        let transport = ScriptedTransport::new(vec![submit, throttled, unavailable, complete]);
        let session = Session::new(&transport, "key", "1", "2");
        let results = session
            .run_script("return {}", Duration::from_secs(30))
            .unwrap();
        assert_eq!(results.len(), 1);
        // Submit plus three polls: both transient failures were retried.
        assert_eq!(transport.seen.borrow().len(), 4);
    }

    #[test]
    fn a_rate_limited_submit_is_retried_not_fatal() {
        // CI auto-enables the cloud backend, so one 429 on submit must not
        // hard-fail the suite; it gets the same transient treatment polls do.
        let throttled = status(429, r#"{"message":"Too many requests"}"#);
        let submit = ok(r#"{"path":"t/abc","state":"QUEUED"}"#);
        let complete = ok(r#"{"path":"t/abc","state":"COMPLETE",
                "output":{"results":[[{"kind":"run_end","passed":1,"failed":0,"skipped":0}]]}}"#);
        let transport = ScriptedTransport::new(vec![throttled, submit, complete]);
        let session = Session::new(&transport, "key", "1", "2");
        let results = session
            .run_script("return {}", Duration::from_secs(30))
            .unwrap();
        assert_eq!(results.len(), 1);
        // Two submits (one throttled, one accepted) plus one poll.
        assert_eq!(transport.seen.borrow().len(), 3);
        assert_eq!(transport.seen.borrow()[1].0, Method::Post);
    }

    #[test]
    fn a_rejected_submit_is_not_retried() {
        let forbidden = status(403, r#"{"message":"Invalid API key"}"#);
        let transport = ScriptedTransport::new(vec![forbidden]);
        let session = Session::new(&transport, "key", "1", "2");
        let err = session
            .run_script("return {}", Duration::from_secs(30))
            .unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        // Exactly one submit — no wasted retries against a 4xx.
        assert_eq!(transport.seen.borrow().len(), 1);
    }

    #[test]
    fn a_rejected_key_is_not_retried() {
        let submit = ok(r#"{"path":"t/abc","state":"QUEUED"}"#);
        let forbidden = status(403, r#"{"message":"Invalid API key"}"#);
        let transport = ScriptedTransport::new(vec![submit, forbidden]);
        let session = Session::new(&transport, "key", "1", "2");
        let err = session
            .run_script("return {}", Duration::from_secs(30))
            .unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        // Submit plus exactly one poll — no wasted retries against a 4xx.
        assert_eq!(transport.seen.borrow().len(), 2);
    }

    #[test]
    fn retryable_statuses_are_the_transient_ones() {
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(is_retryable(code), "{code} should be retryable");
        }
        for code in [400, 401, 403, 404, 409] {
            assert!(!is_retryable(code), "{code} should be fatal");
        }
    }

    #[test]
    fn extract_log_text_handles_documented_shape() {
        let text = extract_log_text(
            r#"{"luauExecutionSessionTaskLogs":[{"messages":["a","b"]},{"messages":["c"]}]}"#,
        );
        assert_eq!(text, "a\nb\nc");
    }
}
