//! The loopback bridge server: the CLI's half of the wire contract the
//! companion plugin speaks (the contract itself is documented in
//! `luau/studio/bridge.luau`'s header, which ships inside the plugin).
//!
//! S2 scope: enough server to prove the loop end to end. The CLI binds
//! `127.0.0.1:<port>`, a polling plugin picks up a pre-queued `ping` job on
//! `GET /job`, answers on `POST /result`, and `lest studio status` reports
//! the live session; run sessions stream a suite through the same socket.
//! Requests are still answered immediately — the held long-poll (waiting
//! for work to arrive) is deferred to the persistent-session work, where it
//! becomes a budget requirement: an immediate-204 server against the
//! plugin's 0.1s idle re-poll burns ~600 requests/minute of Studio's shared
//! HTTP budget. Today that burn exists in exactly one shape — a *second*
//! open Studio polling through another instance's run — and is accepted
//! for this slice; single-session traffic stays far under budget.
//!
//! Hand-rolled HTTP/1.1 over `TcpListener` on purpose: the surface is two
//! routes on a loopback socket that lest owns both ends of. Each request is
//! one connection (`Connection: close`), which matches how the plugin's
//! `RequestAsync` behaves and keeps the parser to request-line + headers +
//! `Content-Length` body.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crate::error::ToolError;

/// What a live plugin reported back to a `ping` job. `ok = false` means the
/// plugin answered but refused the job (its `error` says why) — still a live
/// session, but not a healthy one.
#[derive(Debug, Clone, PartialEq)]
pub struct PingReport {
    pub ok: bool,
    pub error: Option<String>,
    pub place_name: String,
    pub place_id: Option<u64>,
    pub plugin_version: String,
}

/// How a probe window ended. `Silent` and `RefusedSecret` are both "no
/// session report", but they call for opposite advice — permission checks
/// versus a Studio restart — so the caller must be able to tell them apart.
#[derive(Debug, Clone, PartialEq)]
pub enum PingOutcome {
    /// A plugin answered the ping.
    Session(PingReport),
    /// Something polled with the wrong secret and nothing answered: a plugin
    /// from an older install is still loaded in Studio.
    RefusedSecret,
    /// Nothing polled at all.
    Silent,
}

/// A bound bridge session. Dropping it closes the port.
pub struct Bridge {
    listener: TcpListener,
    secret: String,
}

/// How long each individual read may block before it is abandoned.
const STREAM_TIMEOUT: Duration = Duration::from_millis(500);

/// Wall-clock budget for one whole connection (head and body together), so a
/// peer dribbling one byte per read cannot pin the single-threaded loop —
/// each accepted connection costs at most this much of the probe window.
const CONNECTION_BUDGET: Duration = Duration::from_secs(1);

/// Extra time granted once the job has been handed to a plugin, so a fetch
/// in the window's last moments still gets to deliver its answer.
const DISPATCH_GRACE: Duration = Duration::from_secs(1);

/// Upper bound on a request body. The plugin's results are small JSON; a
/// body beyond this is not the plugin.
const MAX_BODY: usize = 1024 * 1024;

/// A parsed request: method, path, lowercased headers, body.
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    /// Header lookup by lowercased name. The plugin sends lowercase names
    /// today, but HTTP header names are case-insensitive and Studio owns
    /// the transport, so the server must not care.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

impl Bridge {
    /// Binds the bridge to `127.0.0.1:port`. Loopback only, by construction:
    /// the bridge must never be reachable from another machine.
    pub fn bind(port: u16, secret: &str) -> Result<Bridge, ToolError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|e| {
            ToolError(format!(
                "cannot open the bridge port {port}: {e} (is another lest session running?)"
            ))
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| ToolError(format!("cannot configure the bridge socket: {e}")))?;
        Ok(Bridge {
            listener,
            secret: secret.to_string(),
        })
    }

    /// The port actually bound, for tests binding port 0.
    #[cfg(test)]
    fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Queues one `ping` job and serves the socket until a plugin answers it
    /// or `wait` elapses. A non-`Session` outcome is an answer too, not an
    /// error; `Err` is reserved for the socket itself failing.
    pub fn ping(&self, wait: Duration) -> Result<PingOutcome, ToolError> {
        let job_id = format!("ping-{}", super::generate_secret());
        let mut deadline = Instant::now() + wait;
        let mut dispatched = false;
        let mut grace_granted = false;
        let mut refused = false;

        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(report) = self.serve(stream, &job_id, &mut dispatched, &mut refused)
                    {
                        return Ok(PingOutcome::Session(report));
                    }
                    // A fetch at the window's edge still deserves its answer:
                    // extend once when the job goes out.
                    if dispatched && !grace_granted {
                        grace_granted = true;
                        let extended = Instant::now() + DISPATCH_GRACE;
                        deadline = deadline.max(extended);
                    }
                    // Serving connections must not postpone expiry forever:
                    // back-to-back peers are no reason to outlive the window.
                    if Instant::now() >= deadline {
                        return Ok(expired(refused));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Ok(expired(refused));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                // Routine accept-loop noise on both shipping platforms: a
                // queued connection that reset before accept (Windows), an
                // aborted handshake (macOS), or a signal. Retry, not fatal.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    return Err(ToolError(format!("the bridge socket failed: {e}")));
                }
            }
        }
    }

    /// Handles one connection. Malformed requests and wrong secrets are
    /// answered on the wire and otherwise ignored (`refused` records that a
    /// wrong secret was seen); only a valid `/result` for the outstanding
    /// job produces a report.
    fn serve(
        &self,
        mut stream: TcpStream,
        job_id: &str,
        dispatched: &mut bool,
        refused: &mut bool,
    ) -> Option<PingReport> {
        // The accepted stream inherits the listener's nonblocking mode on
        // Windows and macOS — exactly the platforms Studio exists on. Left
        // that way, every read returns WouldBlock instantly, the timeouts
        // below never apply, and a request whose bytes are still in flight
        // is answered 400. Blocking mode is load-bearing, so a failure to
        // set it drops the connection rather than mis-serving it.
        if stream.set_nonblocking(false).is_err() {
            return None;
        }
        if stream.set_read_timeout(Some(STREAM_TIMEOUT)).is_err()
            || stream.set_write_timeout(Some(STREAM_TIMEOUT)).is_err()
        {
            // Without timeouts a dribbling peer could hold the loop past
            // every budget; an unconfigurable stream is not worth serving.
            return None;
        }

        let request = match read_request(&mut stream, Instant::now() + CONNECTION_BUDGET) {
            Some(request) => request,
            None => {
                respond(&mut stream, "400 Bad Request", None);
                return None;
            }
        };

        if request.header("x-lest-secret") != Some(self.secret.as_str()) {
            *refused = true;
            respond(&mut stream, "403 Forbidden", None);
            return None;
        }

        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/job") => {
                if *dispatched {
                    // The one job this slice knows is out; nothing for you.
                    respond(&mut stream, "204 No Content", None);
                } else {
                    *dispatched = true;
                    let body = format!(r#"{{"id":"{job_id}","kind":"ping"}}"#);
                    respond(&mut stream, "200 OK", Some(&body));
                }
                None
            }
            ("POST", "/result") => {
                let value: serde_json::Value = match serde_json::from_slice(&request.body) {
                    Ok(value) => value,
                    Err(_) => {
                        respond(&mut stream, "400 Bad Request", None);
                        return None;
                    }
                };
                if value.get("id").and_then(|v| v.as_str()) != Some(job_id) {
                    // Not the job we are waiting on (a stale answer from an
                    // earlier session, say). Refuse it on the wire — visible
                    // to a human watching traffic, if not to the plugin,
                    // which ignores /result responses by design — and keep
                    // waiting for the real one.
                    respond(&mut stream, "400 Bad Request", None);
                    return None;
                }
                respond(&mut stream, "200 OK", None);
                Some(ping_report(&value))
            }
            _ => {
                respond(&mut stream, "404 Not Found", None);
                None
            }
        }
    }
}

/// The outcome when the window closes without a session report.
fn expired(refused: bool) -> PingOutcome {
    if refused {
        PingOutcome::RefusedSecret
    } else {
        PingOutcome::Silent
    }
}

/// One-shot probe used by `lest studio status`: bind, ping, report.
pub fn probe(port: u16, secret: &str, wait: Duration) -> Result<PingOutcome, ToolError> {
    Bridge::bind(port, secret)?.ping(wait)
}

/// How a run job ended, from the bridge's side of the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    /// The plugin posted `/done`; `stopped` is whether it also managed to
    /// stop the playtest (false means the user must press Stop).
    Finished { stopped: bool },
    /// Something polled with the wrong secret and nothing took the job.
    RefusedSecret,
    /// No plugin fetched the job in the fetch window.
    NeverFetched,
    /// The plugin fetched the job and refused it, with its reason.
    Refused(String),
    /// The plugin took the job but neither finished nor kept the budget.
    Died,
}

/// Mutable state one run session threads through its connections.
struct RunState {
    fetched: bool,
    /// The ack said `started = false`: the user must press Run.
    needs_armed_notice: bool,
    armed_reported: bool,
    refused: Option<String>,
    done: Option<bool>,
}

impl Bridge {
    /// Serves one run job to completion: a plugin fetches the bundle on
    /// `GET /job`, acks on `POST /result` (its `started` flag drives the
    /// press-Run notice via `on_armed`), streams sentinel lines in
    /// `POST /events` batches to `on_line`, and finishes with `POST /done`.
    ///
    /// `fetch_wait` bounds the wait for a session to take the job at all;
    /// `run_budget` bounds everything after the fetch (arming, the user's
    /// Run press, and the suite itself — the caller sizes it accordingly).
    /// An `on_line` error aborts the run as a tool error.
    pub fn run_suite(
        &self,
        bundle: &str,
        fetch_wait: Duration,
        run_budget: Duration,
        on_line: &mut dyn FnMut(&str) -> Result<(), ToolError>,
        on_armed: &mut dyn FnMut(),
    ) -> Result<RunOutcome, ToolError> {
        let job_id = format!("run-{}", super::generate_secret());
        let job_body = serde_json::json!({
            "id": job_id,
            "kind": "run",
            "bundle": bundle,
            "markers": {
                "event": crate::backend::runtime::SENTINEL,
                "spec": crate::backend::runtime::SPEC_SENTINEL,
                "done": crate::backend::runtime::DONE_SENTINEL,
            },
        })
        .to_string();

        let mut deadline = deadline_after(fetch_wait);
        let mut refused_secret = false;
        let mut state = RunState {
            fetched: false,
            needs_armed_notice: false,
            armed_reported: false,
            refused: None,
            done: None,
        };

        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let was_fetched = state.fetched;
                    self.serve_run(
                        stream,
                        &job_id,
                        &job_body,
                        &mut state,
                        &mut refused_secret,
                        on_line,
                    )?;
                    if state.fetched && !was_fetched {
                        // The job is out; the budget now covers arming, the
                        // user's Run press, and the suite.
                        deadline = deadline_after(run_budget);
                    }
                    if state.needs_armed_notice && !state.armed_reported {
                        // Exactly once, and only after an ack that said the
                        // world is waiting on the user's Run press.
                        state.armed_reported = true;
                        on_armed();
                    }
                    if let Some(stopped) = state.done {
                        return Ok(RunOutcome::Finished { stopped });
                    }
                    if let Some(reason) = state.refused.take() {
                        return Ok(RunOutcome::Refused(reason));
                    }
                    if Instant::now() >= deadline {
                        return Ok(run_expired(&state, refused_secret));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Ok(run_expired(&state, refused_secret));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    return Err(ToolError(format!("the bridge socket failed: {e}")));
                }
            }
        }
    }

    /// Handles one connection of a run session.
    fn serve_run(
        &self,
        mut stream: TcpStream,
        job_id: &str,
        job_body: &str,
        state: &mut RunState,
        refused_secret: &mut bool,
        on_line: &mut dyn FnMut(&str) -> Result<(), ToolError>,
    ) -> Result<(), ToolError> {
        if stream.set_nonblocking(false).is_err() {
            return Ok(());
        }
        if stream.set_read_timeout(Some(STREAM_TIMEOUT)).is_err()
            || stream.set_write_timeout(Some(STREAM_TIMEOUT)).is_err()
        {
            return Ok(());
        }

        let request = match read_request(&mut stream, Instant::now() + CONNECTION_BUDGET) {
            Some(request) => request,
            None => {
                respond(&mut stream, "400 Bad Request", None);
                return Ok(());
            }
        };

        if request.header("x-lest-secret") != Some(self.secret.as_str()) {
            *refused_secret = true;
            respond(&mut stream, "403 Forbidden", None);
            return Ok(());
        }

        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/job") => {
                if state.fetched {
                    respond(&mut stream, "204 No Content", None);
                } else {
                    state.fetched = true;
                    respond(&mut stream, "200 OK", Some(job_body));
                }
                Ok(())
            }
            ("POST", "/result") => {
                let Some(value) = decode_for(&mut stream, &request, job_id) else {
                    return Ok(());
                };
                if value.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                    // started=false (or absent) means the world waits on the
                    // user's Run press; the caller prints that notice once.
                    if value.get("started").and_then(|v| v.as_bool()) != Some(true) {
                        state.needs_armed_notice = true;
                    }
                } else {
                    state.refused = Some(
                        value
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(no reason given)")
                            .to_string(),
                    );
                    // A refusal is terminal; suppress the armed notice.
                    state.armed_reported = true;
                }
                respond(&mut stream, "200 OK", None);
                Ok(())
            }
            ("POST", "/events") => {
                let Some(value) = decode_for(&mut stream, &request, job_id) else {
                    return Ok(());
                };
                respond(&mut stream, "200 OK", None);
                if let Some(lines) = value.get("lines").and_then(|v| v.as_array()) {
                    for line in lines {
                        if let Some(text) = line.as_str() {
                            on_line(text)?;
                        }
                    }
                }
                Ok(())
            }
            ("POST", "/done") => {
                let Some(value) = decode_for(&mut stream, &request, job_id) else {
                    return Ok(());
                };
                state.done = Some(
                    value
                        .get("stopped")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                );
                respond(&mut stream, "200 OK", None);
                Ok(())
            }
            _ => {
                respond(&mut stream, "404 Not Found", None);
                Ok(())
            }
        }
    }
}

/// Decodes a JSON body and checks its `id` addresses this job; answers the
/// wire (400) and yields `None` otherwise.
fn decode_for(
    stream: &mut TcpStream,
    request: &Request,
    job_id: &str,
) -> Option<serde_json::Value> {
    let value: serde_json::Value = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(_) => {
            respond(stream, "400 Bad Request", None);
            return None;
        }
    };
    if value.get("id").and_then(|v| v.as_str()) != Some(job_id) {
        respond(stream, "400 Bad Request", None);
        return None;
    }
    Some(value)
}

/// `Instant + Duration` guarded the way `runtime.rs` guards it: the budget
/// derives from unvalidated `timeout_ms` config, and the unchecked `Add`
/// panics on overflow — exit 101, bypassing the exit-code policy.
fn deadline_after(wait: Duration) -> Instant {
    Instant::now()
        .checked_add(wait)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400))
}

/// The outcome when a run window closes without `/done`.
fn run_expired(state: &RunState, refused_secret: bool) -> RunOutcome {
    if state.fetched {
        RunOutcome::Died
    } else if refused_secret {
        RunOutcome::RefusedSecret
    } else {
        RunOutcome::NeverFetched
    }
}

/// Shapes a `/result` value into a report, tolerating missing fields: a
/// plugin that answered at all is a live session even if a field is absent.
fn ping_report(value: &serde_json::Value) -> PingReport {
    PingReport {
        ok: value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
        error: value
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        place_name: value
            .pointer("/place/name")
            .and_then(|v| v.as_str())
            .unwrap_or("(unnamed place)")
            .to_string(),
        // JSONEncode writes Luau numbers as JSON numbers; place ids are
        // integral, but tolerate a float spelling.
        place_id: value
            .pointer("/place/placeId")
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))),
        plugin_version: value
            .pointer("/plugin/version")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)")
            .to_string(),
    }
}

/// Reads and parses one HTTP request, giving up at `deadline`. `None` on
/// anything malformed; the caller answers 400. Reads byte-wise up to the
/// header terminator, then `Content-Length` bytes of body in chunks.
fn read_request(stream: &mut TcpStream, deadline: Instant) -> Option<Request> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // Byte-at-a-time until CRLFCRLF: loopback and tiny, so simplicity beats
    // buffering cleverness that could over-read into the body.
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 || Instant::now() >= deadline {
            return None;
        }
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return None,
        }
    }
    let head = String::from_utf8(head).ok()?;
    let mut lines = head.split("\r\n");

    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }

    // Absent Content-Length means no body (GET /job); a Content-Length that
    // does not parse is a malformed request, not an empty one.
    let length: usize = match headers.iter().find(|(name, _)| name == "content-length") {
        None => 0,
        Some((_, value)) => value.parse().ok()?,
    };
    if length > MAX_BODY {
        return None;
    }
    let mut body = Vec::with_capacity(length.min(64 * 1024));
    let mut chunk = [0u8; 4096];
    while body.len() < length {
        if Instant::now() >= deadline {
            return None;
        }
        let want = (length - body.len()).min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) => return None,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }

    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

/// Writes a minimal HTTP/1.1 response and closes (via drop at the caller).
/// 204 carries no body headers — Content-Length on No Content is an RFC
/// violation some clients trip over.
fn respond(stream: &mut TcpStream, status: &str, body: Option<&str>) {
    let response = if status.starts_with("204") {
        format!("HTTP/1.1 {status}\r\nconnection: close\r\n\r\n")
    } else {
        let body = body.unwrap_or("");
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    // A peer that vanished mid-response is its problem; nothing to do.
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted stand-in for the plugin: speaks raw HTTP over a socket the
    /// way `luau/studio/bridge.luau` does through RequestAsync.
    fn request(port: u16, raw: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        stream.write_all(raw.as_bytes()).expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        response
    }

    fn get_job(port: u16, secret: &str) -> String {
        request(
            port,
            &format!(
                "GET /job HTTP/1.1\r\nx-lest-secret: {secret}\r\nx-lest-version: 9.9.9\r\n\r\n"
            ),
        )
    }

    fn post_result(port: u16, secret: &str, body: &str) -> String {
        request(
            port,
            &format!(
                "POST /result HTTP/1.1\r\nx-lest-secret: {secret}\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    /// Extracts the dispatched job id from a 200 `GET /job` response body.
    fn job_id(response: &str) -> String {
        let body = response.split("\r\n\r\n").nth(1).expect("body");
        let value: serde_json::Value = serde_json::from_str(body).expect("json");
        value["id"].as_str().expect("id").to_string()
    }

    #[test]
    fn ping_round_trip_reports_the_session() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let plugin = std::thread::spawn(move || {
            // Poll until the job appears, answer it, like the real plugin.
            loop {
                let response = get_job(port, "s3cret");
                if response.starts_with("HTTP/1.1 200") {
                    let id = job_id(&response);
                    let body = format!(
                        r#"{{"id":"{id}","ok":true,"plugin":{{"version":"9.9.9"}},"place":{{"name":"My Place","placeId":12345}}}}"#
                    );
                    let posted = post_result(port, "s3cret", &body);
                    assert!(posted.starts_with("HTTP/1.1 200"), "{posted}");
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let outcome = bridge.ping(Duration::from_secs(5)).expect("ping");
        plugin.join().expect("plugin thread");

        let PingOutcome::Session(report) = outcome else {
            panic!("expected a session, got {outcome:?}");
        };
        assert!(report.ok);
        assert_eq!(report.place_name, "My Place");
        assert_eq!(report.place_id, Some(12345));
        assert_eq!(report.plugin_version, "9.9.9");
    }

    #[test]
    fn a_wrong_secret_is_403_and_reported_as_refused() {
        let bridge = Bridge::bind(0, "right").expect("bind");
        let port = bridge.port();

        let prober = std::thread::spawn(move || {
            let response = get_job(port, "wrong");
            assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        });

        let outcome = bridge.ping(Duration::from_millis(1500)).expect("ping");
        prober.join().expect("prober");
        // The distinction status needs: someone is there, with a stale
        // install, which calls for a Studio restart rather than a
        // permissions check.
        assert_eq!(outcome, PingOutcome::RefusedSecret);
    }

    #[test]
    fn a_request_with_no_headers_is_refused_not_served() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let prober = std::thread::spawn(move || {
            let response = request(port, "GET /job HTTP/1.1\r\n\r\n");
            assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        });

        let outcome = bridge.ping(Duration::from_millis(1500)).expect("ping");
        prober.join().expect("prober");
        assert_eq!(outcome, PingOutcome::RefusedSecret);
    }

    #[test]
    fn no_poller_means_silent_not_an_error() {
        let bridge = Bridge::bind(0, "s").expect("bind");
        let outcome = bridge.ping(Duration::from_millis(100)).expect("ping");
        assert_eq!(outcome, PingOutcome::Silent);
    }

    #[test]
    fn header_names_match_case_insensitively() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let plugin = std::thread::spawn(move || {
            let response = request(port, "GET /job HTTP/1.1\r\nX-LEST-SECRET: s3cret\r\n\r\n");
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let id = job_id(&response);
            let body = format!(r#"{{"id":"{id}","ok":true}}"#);
            post_result(port, "s3cret", &body);
        });

        let outcome = bridge.ping(Duration::from_secs(5)).expect("ping");
        plugin.join().expect("plugin");
        let PingOutcome::Session(report) = outcome else {
            panic!("expected a session, got {outcome:?}");
        };
        // Optional fields tolerated: an answering plugin is a live session.
        assert_eq!(report.place_name, "(unnamed place)");
        assert_eq!(report.place_id, None);
    }

    #[test]
    fn a_refusing_plugin_answer_is_still_a_session_with_its_error() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let plugin = std::thread::spawn(move || {
            let response = get_job(port, "s3cret");
            let id = job_id(&response);
            let body = format!(
                r#"{{"id":"{id}","ok":false,"error":"unsupported job kind 'ping' — plugin 0.1.0 predates it"}}"#
            );
            post_result(port, "s3cret", &body);
        });

        let outcome = bridge.ping(Duration::from_secs(5)).expect("ping");
        plugin.join().expect("plugin");
        let PingOutcome::Session(report) = outcome else {
            panic!("expected a session, got {outcome:?}");
        };
        assert!(!report.ok);
        assert!(report
            .error
            .expect("error")
            .contains("unsupported job kind"));
    }

    #[test]
    fn the_second_poller_gets_204_after_dispatch() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let plugin = std::thread::spawn(move || {
            let first = get_job(port, "s3cret");
            assert!(first.starts_with("HTTP/1.1 200"), "{first}");
            let second = get_job(port, "s3cret");
            assert!(second.starts_with("HTTP/1.1 204"), "{second}");
            // 204 must carry no body headers (RFC: no Content-Length on
            // No Content).
            assert!(!second.to_ascii_lowercase().contains("content-length"));
            // Leave the job unanswered; ping times out below.
        });

        let outcome = bridge.ping(Duration::from_millis(600)).expect("ping");
        plugin.join().expect("plugin");
        assert_eq!(outcome, PingOutcome::Silent);
    }

    #[test]
    fn a_result_for_the_wrong_job_is_refused_and_waiting_continues() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let plugin = std::thread::spawn(move || {
            let response = get_job(port, "s3cret");
            let real_id = job_id(&response);
            let stale = post_result(port, "s3cret", r#"{"id":"stale","ok":true}"#);
            assert!(stale.starts_with("HTTP/1.1 400"), "{stale}");
            let body = format!(r#"{{"id":"{real_id}","ok":true,"plugin":{{"version":"9.9.9"}}}}"#);
            let real = post_result(port, "s3cret", &body);
            assert!(real.starts_with("HTTP/1.1 200"), "{real}");
        });

        let outcome = bridge.ping(Duration::from_secs(5)).expect("ping");
        plugin.join().expect("plugin");
        let PingOutcome::Session(report) = outcome else {
            panic!("expected a session, got {outcome:?}");
        };
        assert_eq!(report.plugin_version, "9.9.9");
    }

    #[test]
    fn malformed_json_in_a_result_is_400() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let plugin = std::thread::spawn(move || {
            get_job(port, "s3cret");
            let response = post_result(port, "s3cret", "not json");
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        });

        let outcome = bridge.ping(Duration::from_millis(1500)).expect("ping");
        plugin.join().expect("plugin");
        assert_eq!(outcome, PingOutcome::Silent);
    }

    #[test]
    fn a_non_numeric_content_length_is_400() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let prober = std::thread::spawn(move || {
            // No body follows the bogus header: the server refuses on the
            // head alone, and unread bytes at close would RST the socket on
            // Windows before the 400 could be read back.
            let response = request(
                port,
                "POST /result HTTP/1.1\r\nx-lest-secret: s3cret\r\ncontent-length: nope\r\n\r\n",
            );
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        });

        let outcome = bridge.ping(Duration::from_millis(1500)).expect("ping");
        prober.join().expect("prober");
        assert_eq!(outcome, PingOutcome::Silent);
    }

    #[test]
    fn an_oversized_body_is_400() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let prober = std::thread::spawn(move || {
            let response = request(
                port,
                &format!(
                    "POST /result HTTP/1.1\r\nx-lest-secret: s3cret\r\ncontent-length: {}\r\n\r\n",
                    MAX_BODY + 1
                ),
            );
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        });

        let outcome = bridge.ping(Duration::from_millis(1500)).expect("ping");
        prober.join().expect("prober");
        assert_eq!(outcome, PingOutcome::Silent);
    }

    #[test]
    fn a_split_request_head_and_body_are_both_read() {
        // The regression C1 guards against: RequestAsync may deliver the
        // head and body in separate segments. The server must block for the
        // rest, not answer 400 on the first packet.
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let plugin = std::thread::spawn(move || {
            let response = get_job(port, "s3cret");
            let id = job_id(&response);
            let body = format!(r#"{{"id":"{id}","ok":true,"plugin":{{"version":"9.9.9"}}}}"#);
            let head = format!(
                "POST /result HTTP/1.1\r\nx-lest-secret: s3cret\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
            stream.write_all(head.as_bytes()).expect("head");
            stream.flush().expect("flush");
            // A pause long enough that the server has certainly accepted and
            // begun reading before the body exists to be read.
            std::thread::sleep(Duration::from_millis(150));
            stream.write_all(body.as_bytes()).expect("body");
            let mut response = String::new();
            stream.read_to_string(&mut response).expect("read");
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        });

        let outcome = bridge.ping(Duration::from_secs(5)).expect("ping");
        plugin.join().expect("plugin");
        assert!(matches!(outcome, PingOutcome::Session(_)));
    }

    #[test]
    fn unknown_routes_are_404() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();

        let prober = std::thread::spawn(move || {
            let response = request(port, "GET /nope HTTP/1.1\r\nx-lest-secret: s3cret\r\n\r\n");
            assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        });

        let outcome = bridge.ping(Duration::from_millis(1500)).expect("ping");
        prober.join().expect("prober");
        assert_eq!(outcome, PingOutcome::Silent);
    }

    fn post_json(port: u16, secret: &str, path: &str, body: &str) -> String {
        request(
            port,
            &format!(
                "POST {path} HTTP/1.1\r\nx-lest-secret: {secret}\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    /// Runs `run_suite` on a thread with collected lines/armed notices, so
    /// the test body plays the plugin synchronously.
    fn spawn_run(
        bridge: Bridge,
        fetch_wait: Duration,
        run_budget: Duration,
    ) -> std::thread::JoinHandle<(Result<RunOutcome, ToolError>, Vec<String>, usize)> {
        std::thread::spawn(move || {
            let mut lines = Vec::new();
            let mut armed = 0usize;
            let outcome = bridge.run_suite(
                "THE BUNDLE",
                fetch_wait,
                run_budget,
                &mut |line| {
                    lines.push(line.to_string());
                    Ok(())
                },
                &mut || armed += 1,
            );
            (outcome, lines, armed)
        })
    }

    #[test]
    fn run_round_trip_streams_lines_and_finishes() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();
        let server = spawn_run(bridge, Duration::from_secs(5), Duration::from_secs(5));

        // Poll like the plugin until the job appears.
        let job: serde_json::Value = loop {
            let response = get_job(port, "s3cret");
            if response.starts_with("HTTP/1.1 200") {
                let body = response.split("\r\n\r\n").nth(1).expect("body");
                break serde_json::from_str(body).expect("job json");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(job["kind"], "run");
        assert_eq!(job["bundle"], "THE BUNDLE");
        assert_eq!(job["markers"]["event"], crate::backend::runtime::SENTINEL);
        let id = job["id"].as_str().expect("id");

        post_json(
            port,
            "s3cret",
            "/result",
            &format!(r#"{{"id":"{id}","ok":true,"started":false}}"#),
        );
        post_json(
            port,
            "s3cret",
            "/events",
            &format!(r#"{{"id":"{id}","lines":["a-line","b-line"]}}"#),
        );
        post_json(
            port,
            "s3cret",
            "/done",
            &format!(r#"{{"id":"{id}","ok":true,"stopped":true}}"#),
        );

        let (outcome, lines, armed) = server.join().expect("server");
        assert_eq!(
            outcome.expect("run"),
            RunOutcome::Finished { stopped: true }
        );
        assert_eq!(lines, vec!["a-line".to_string(), "b-line".to_string()]);
        // started=false earns exactly one press-Run notice.
        assert_eq!(armed, 1);
    }

    #[test]
    fn a_started_ack_skips_the_armed_notice() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();
        let server = spawn_run(bridge, Duration::from_secs(5), Duration::from_secs(5));

        let job: serde_json::Value = loop {
            let response = get_job(port, "s3cret");
            if response.starts_with("HTTP/1.1 200") {
                let body = response.split("\r\n\r\n").nth(1).expect("body");
                break serde_json::from_str(body).expect("job json");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let id = job["id"].as_str().expect("id");
        post_json(
            port,
            "s3cret",
            "/result",
            &format!(r#"{{"id":"{id}","ok":true,"started":true}}"#),
        );
        post_json(
            port,
            "s3cret",
            "/done",
            &format!(r#"{{"id":"{id}","ok":true,"stopped":false}}"#),
        );

        let (outcome, _, armed) = server.join().expect("server");
        assert_eq!(
            outcome.expect("run"),
            RunOutcome::Finished { stopped: false }
        );
        assert_eq!(armed, 0);
    }

    #[test]
    fn a_plugin_refusal_is_reported_with_its_reason() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();
        let server = spawn_run(bridge, Duration::from_secs(5), Duration::from_secs(5));

        let job: serde_json::Value = loop {
            let response = get_job(port, "s3cret");
            if response.starts_with("HTTP/1.1 200") {
                let body = response.split("\r\n\r\n").nth(1).expect("body");
                break serde_json::from_str(body).expect("job json");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let id = job["id"].as_str().expect("id");
        post_json(
            port,
            "s3cret",
            "/result",
            &format!(r#"{{"id":"{id}","ok":false,"error":"a run is already in progress"}}"#),
        );

        let (outcome, _, armed) = server.join().expect("server");
        assert_eq!(
            outcome.expect("run"),
            RunOutcome::Refused("a run is already in progress".to_string())
        );
        assert_eq!(armed, 0);
    }

    #[test]
    fn an_unfetched_job_expires_as_never_fetched() {
        let bridge = Bridge::bind(0, "s").expect("bind");
        let server = spawn_run(bridge, Duration::from_millis(100), Duration::from_secs(5));
        let (outcome, _, _) = server.join().expect("server");
        assert_eq!(outcome.expect("run"), RunOutcome::NeverFetched);
    }

    #[test]
    fn a_fetched_job_that_goes_quiet_expires_as_died() {
        let bridge = Bridge::bind(0, "s3cret").expect("bind");
        let port = bridge.port();
        let server = spawn_run(bridge, Duration::from_secs(5), Duration::from_millis(400));

        loop {
            let response = get_job(port, "s3cret");
            if response.starts_with("HTTP/1.1 200") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (outcome, _, _) = server.join().expect("server");
        assert_eq!(outcome.expect("run"), RunOutcome::Died);
    }

    #[test]
    fn a_wrong_secret_run_window_reports_refused_secret() {
        let bridge = Bridge::bind(0, "right").expect("bind");
        let port = bridge.port();
        let server = spawn_run(bridge, Duration::from_millis(800), Duration::from_secs(5));

        let response = get_job(port, "wrong");
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");

        let (outcome, _, _) = server.join().expect("server");
        assert_eq!(outcome.expect("run"), RunOutcome::RefusedSecret);
    }

    #[test]
    fn binding_is_loopback_only() {
        let bridge = Bridge::bind(0, "s").expect("bind");
        let addr = bridge.listener.local_addr().expect("addr");
        assert!(addr.ip().is_loopback());
    }
}
