//! The spawned-runtime backend: one abstraction, two runtimes (lune and
//! lute). The CLI writes a small harness into `.lest/`, spawns the runtime
//! on it, and decodes sentinel-marked JSON lines from stdout back into
//! protocol events. Tests get the *real* runtime APIs because they genuinely
//! run in that runtime — a second spawned runtime is config, not code.
//!
//! The sentinel exists so that ordinary output from test code cannot *corrupt*
//! the protocol stream: unmarked text passes through as output, and marked text
//! decodes as events, no matter what a test prints. It is emphatically **not** a
//! security boundary — an in-band marker can always be forged by code that
//! chooses to print it, and lest does not treat the tests it runs as hostile.
//!
//! Because the whole suite loads into one process, the merged stream cannot
//! say which spec file a snapshot came from on its own. The harness therefore
//! prints a spec-boundary marker (`@@LEST_SPEC@@<index>`) before each spec's
//! events, and this decoder attributes every subsequent event to that spec so
//! snapshot storage keys off the right `.snap` file — the same per-spec
//! attribution the native backend gets for free.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::report::{check_protocol_version, Event, Failure};
use crate::resolve::Runtime;

use crate::backend::{display_rel, require_string, EventSink, SuitePlan};
use crate::error::ToolError;

// `pub(crate)`: the studio backend frames its LogService relay with the same
// markers, so the decode path is shared rather than duplicated.
pub(crate) const SENTINEL: &str = "@@LEST@@";
pub(crate) const SPEC_SENTINEL: &str = "@@LEST_SPEC@@";
/// The studio run's terminal marker: printed once after the last spec so the
/// plugin knows the suite finished without waiting for a process exit (the
/// signal the spawned runtimes get for free). `classify` never sees it — the
/// studio path drops done-framed lines before classifying — and the studio
/// side applies its own first-marker-wins rule against the event marker, so
/// a payload containing this text cannot forge completion.
pub(crate) const DONE_SENTINEL: &str = "@@LEST_STUDIO_DONE@@";
const HARNESS_TEMPLATE: &str = include_str!("../../luau/runtime/harness.luau");

/// How many trailing stderr lines are retained for diagnosing a process that
/// dies before speaking the protocol. A bound, not a log — the lines still
/// stream to the terminal as they arrive.
const STDERR_TAIL_LINES: usize = 20;

fn install_hint(runtime: Runtime) -> &'static str {
    match runtime {
        Runtime::Lune => {
            "install it with rokit (`rokit add lune-org/lune`) or from \
             https://github.com/lune-org/lune/releases"
        }
        Runtime::Lute => {
            "install it with rokit (`rokit add luau-lang/lute`) or from \
             https://github.com/luau-lang/lute/releases"
        }
    }
}

/// What one line of runtime stdout turned out to be, with any text preceding
/// the marker kept so it can still be shown.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decoded<'a> {
    /// A spec-boundary marker carrying its (untrimmed) index text.
    SpecBoundary { leading: &'a str, index: &'a str },
    /// A protocol event carrying its JSON payload.
    Event { leading: &'a str, json: &'a str },
    /// Ordinary test output.
    Output,
}

/// Finds a protocol marker *anywhere* in the line, not just at position 0.
///
/// Test code that writes without a trailing newline (`stdio.write` under
/// `@lune/stdio`) leaves its output glued to the front of the next protocol
/// line, so the runtime emits `partial@@LEST@@{...}`. Requiring the marker to be
/// a prefix would send that line down the passthrough branch and silently drop
/// the event — the suite would report fewer tests than it ran, with nothing
/// failing. Scanning instead recovers both halves: the leading text is ordinary
/// output, the remainder is the event.
///
/// The marker that occurs *first* in the line is the one framing it. A
/// legitimate event line can carry the spec marker inside its JSON payload — a
/// snapshot's received text, a failure message, a test name; the marker
/// characters survive JSON string escaping untouched — and classifying by
/// "does the spec marker appear anywhere" turned such an event into a bogus
/// spec boundary: an exit-2 abort, or silently misattributed snapshots when
/// the tail happened to parse as an index. On a tie at the same position the
/// longer marker wins (the two differ within their first eight bytes, so a tie
/// cannot actually occur — the rule is stated for whoever adds a third marker).
pub(crate) fn classify(line: &str) -> Decoded<'_> {
    let spec_at = line.find(SPEC_SENTINEL);
    let event_at = line.find(SENTINEL);
    match (spec_at, event_at) {
        (Some(spec), Some(event)) if event < spec => Decoded::Event {
            leading: &line[..event],
            json: &line[event + SENTINEL.len()..],
        },
        (Some(spec), _) => Decoded::SpecBoundary {
            leading: &line[..spec],
            index: &line[spec + SPEC_SENTINEL.len()..],
        },
        (None, Some(event)) => Decoded::Event {
            leading: &line[..event],
            json: &line[event + SENTINEL.len()..],
        },
        (None, None) => Decoded::Output,
    }
}

/// Text preceding a marker is test output that arrived without its own newline;
/// printing it keeps the passthrough contract intact.
pub(crate) fn passthrough(leading: &str) {
    if !leading.is_empty() {
        println!("{leading}");
    }
}

/// Reports an exhausted process budget as a *test* failure (exit 1), not a
/// tool error (exit 2) — the same call native makes when its interrupt fires.
/// Shared by the mid-stream deadline check and the post-EOF wait, so both
/// expiries report identically. The process is the only granularity a spawned
/// runtime offers, so the failure is attributed to the spec that was executing
/// when the budget ran out (or the suite itself when none had started).
fn report_budget_timeout(
    plan: &SuitePlan,
    command: &str,
    budget: Duration,
    current_spec: Option<usize>,
    on_event: &mut EventSink,
) {
    let spec = current_spec.map(|i| plan.specs[i].as_path());
    let path = spec
        .map(|p| display_rel(p, &plan.root))
        .unwrap_or_else(|| plan.name.clone());
    let event = Event::TestFail {
        path: vec![path],
        name: "(timeout)".to_string(),
        duration_ms: budget.as_millis() as f64,
        failure: Failure::Error {
            message: format!(
                "{command} suite \"{}\" exceeded its time budget ({}s) and was killed",
                plan.name,
                budget.as_secs()
            ),
            trace: None,
        },
        origin: None,
    };
    on_event(spec, &event);
}

pub fn run(runtime: Runtime, plan: &SuitePlan, on_event: &mut EventSink) -> Result<(), ToolError> {
    let harness = write_harness(runtime, plan)?;
    let command = runtime.to_string();

    let mut child = Command::new(&command)
        .arg("run")
        .arg(&harness)
        .current_dir(&plan.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError(format!(
                    "cannot find `{command}` on PATH — {}",
                    install_hint(runtime)
                ))
            } else {
                ToolError(format!("cannot start {command}: {e}"))
            }
        })?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel::<std::io::Result<String>>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // stderr is piped only so its tail can be inspected when the process dies
    // before speaking the protocol — every line still streams to our stderr
    // the moment it arrives, so runtime and test diagnostics reach the
    // terminal as they did when it was inherited. The motivating case: a
    // rokit shim whose tool is missing from the project manifest exits
    // nonzero with one clear stderr line, which used to scroll past while
    // lest reported only a bare exit code.
    let stderr = child.stderr.take().expect("stderr was piped");
    let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_reader = {
        let tail = Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                eprintln!("{line}");
                let mut tail = tail.lock().unwrap();
                if tail.len() >= STDERR_TAIL_LINES {
                    tail.remove(0);
                }
                tail.push(line);
            }
        })
    };

    // The whole suite runs in one process, so the per-test budget is scaled
    // into a per-process budget; a stuck runtime can only be timed out by
    // killing it.
    // Saturating, not panicking: `timeout_ms` comes from user config, and
    // `Duration`'s `Mul`/`Add` panic on overflow. A panic here would exit 101
    // and bypass the exit-code policy entirely, so an absurd budget clamps to
    // "effectively never" instead.
    let budget = plan
        .timeout
        .saturating_mul(plan.specs.len().max(1) as u32)
        .saturating_add(Duration::from_secs(10));
    let deadline = Instant::now()
        .checked_add(budget)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));

    // The spec file the harness is currently emitting events for, as an index
    // into `plan.specs`, set by each `@@LEST_SPEC@@<index>` boundary marker.
    let mut current_spec: Option<usize> = None;
    // A run that produced spec files but never a single test outcome means the
    // harness broke before running anything — a false green we refuse.
    let mut outcomes = 0usize;

    type Readers = (std::thread::JoinHandle<()>, std::thread::JoinHandle<()>);
    let abort = |mut child: Child,
                 rx: mpsc::Receiver<std::io::Result<String>>,
                 readers: Readers,
                 err: ToolError| {
        let _ = child.kill();
        let _ = child.wait();
        drop(rx);
        let _ = readers.0.join();
        let _ = readers.1.join();
        err
    };

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // Something under test hung. Returning a ToolError here would
            // additionally make the CLI discard every outcome this suite
            // already streamed, so the expiry is reported as a synthetic
            // test failure instead (see `report_budget_timeout`).
            let _ = child.kill();
            let _ = child.wait();
            drop(rx);
            let _ = reader.join();
            let _ = stderr_reader.join();

            report_budget_timeout(plan, &command, budget, current_spec, on_event);
            return Ok(());
        }
        match rx.recv_timeout(remaining) {
            Ok(Ok(line)) => match classify(&line) {
                Decoded::SpecBoundary { leading, index } => {
                    passthrough(leading);
                    let raw = index.trim();
                    // A boundary index the CLI cannot map to a spec means the
                    // harness and the CLI disagree about the spec list. Silently
                    // clearing attribution would drop every snapshot for the
                    // rest of the suite with no diagnostic — exactly the quiet
                    // skew the false-green guard exists to make loud.
                    let resolved = raw
                        .parse::<usize>()
                        .ok()
                        .and_then(|one_based| one_based.checked_sub(1))
                        .filter(|&i| i < plan.specs.len());
                    match resolved {
                        Some(index) => current_spec = Some(index),
                        None => {
                            return Err(abort(
                                child,
                                rx,
                                (reader, stderr_reader),
                                ToolError(format!(
                                    "{command} suite \"{}\" sent the spec-boundary marker \
                                     \"{raw}\", which is not a 1-based index into its {} spec \
                                     file(s) — the harness and the CLI disagree about the spec \
                                     list",
                                    plan.name,
                                    plan.specs.len()
                                )),
                            ));
                        }
                    }
                }
                Decoded::Event { leading, json } => {
                    passthrough(leading);
                    let event = match serde_json::from_str::<Event>(json) {
                        Ok(event) => event,
                        Err(err) => {
                            // Native aborts on an undecodable protocol line;
                            // the runtime backend now matches it rather than
                            // warning and marching on to a false success.
                            return Err(abort(
                                child,
                                rx,
                                (reader, stderr_reader),
                                ToolError(format!(
                                    "undecodable protocol line from {command} while running \
                                     suite \"{}\": {err}",
                                    plan.name
                                )),
                            ));
                        }
                    };
                    if let Event::RunStart {
                        protocol_version, ..
                    } = event
                    {
                        if let Err(mismatch) = check_protocol_version(protocol_version) {
                            return Err(abort(
                                child,
                                rx,
                                (reader, stderr_reader),
                                ToolError(format!(
                                    "framework/CLI protocol mismatch from {command}: {mismatch}"
                                )),
                            ));
                        }
                    }
                    if matches!(
                        event,
                        Event::TestPass { .. } | Event::TestFail { .. } | Event::TestSkip { .. }
                    ) {
                        outcomes += 1;
                    }
                    let spec = current_spec.map(|i| plan.specs[i].as_path());
                    on_event(spec, &event);
                }
                Decoded::Output => {
                    // Blank lines included: a test that prints one gets it
                    // echoed under native, and spawned output must match.
                    println!("{line}");
                }
            },
            Ok(Err(err)) => {
                return Err(abort(
                    child,
                    rx,
                    (reader, stderr_reader),
                    ToolError(format!("cannot read {command} output: {err}")),
                ));
            }
            // recv_timeout returned early; the loop re-checks the deadline.
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = reader.join();
    // Stdout EOF only proves the pipe closed, not that the child exited: a
    // runtime hung in shutdown (or holding the process open after tests) would
    // leave an unbounded `wait()` blocked forever with no deadline governing
    // it. Poll instead, bounded by the same overall budget, and report expiry
    // through the same test-failure path the mid-stream deadline uses.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                report_budget_timeout(plan, &command, budget, current_spec, on_event);
                return Ok(());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(ToolError(format!("cannot wait for {command}: {e}"))),
        }
    };
    // Joined only after the child exited: its stderr is at EOF, so this never
    // blocks — joining before the wait loop could deadlock on a hung child
    // still holding the pipe.
    let _ = stderr_reader.join();
    if !status.success() {
        let mut message = format!(
            "{command} exited with {status} while running suite \"{}\"",
            plan.name
        );
        // A rokit shim resolves its tool per project manifest, so a `lune` on
        // PATH can still fail to *be* lune here — and the bare exit code
        // reads as the runtime crashing. The shim says what happened in one
        // stderr line; the retained tail is what lets it be named.
        if stderr_tail
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("Failed to find tool"))
        {
            message = format!(
                "{message}\n`{command}` on PATH is a rokit shim, and this project's rokit.toml \
                 does not list the tool — {}",
                install_hint(runtime)
            );
        }
        return Err(ToolError(message));
    }
    // A name filter legitimately produces zero outcomes, so it disarms this
    // guard — the CLI reports the no-match case itself, with a message that
    // names the real cause instead of blaming the harness.
    if !plan.specs.is_empty() && outcomes == 0 && plan.name_filter.is_none() {
        return Err(ToolError(format!(
            "{command} suite \"{}\" ran {} spec file(s) but produced no test outcomes — the \
             harness likely failed to load lest/core or the specs",
            plan.name,
            plan.specs.len()
        )));
    }
    Ok(())
}

fn write_harness(runtime: Runtime, plan: &SuitePlan) -> Result<PathBuf, ToolError> {
    let dir = plan.root.join(".lest");
    // Ignoring `.lest/` is the project's `.gitignore` to decide — `lest init`
    // offers it — so nothing is written here but the harness itself.
    std::fs::create_dir_all(&dir)
        .map_err(|e| ToolError(format!("cannot create {}: {e}", dir.display())))?;

    // Both sides are normalized before the paths are compared. `core_entry`
    // arrives normalized (and case-folded on Windows) while `plan.root` does
    // not, and `relative_path` compares components literally — so `Users` vs
    // `users` would diverge right after the drive letter and produce a require
    // that climbs to the root and walks all the way back down. That path still
    // *reads* the right file, but the runtime keys its module cache on it, so a
    // spec reaching lest/core by any other spelling (a `.luaurc` alias, say)
    // would load a second, separate copy of the framework: its `describe`/`it`
    // calls would register into a registry the harness never runs, and the
    // suite would report zero tests without failing anything.
    let dir_key = crate::resolve::normalize(&dir);
    let rel_require = |target: &std::path::Path| -> Result<String, ToolError> {
        require_string(&dir_key, &crate::resolve::normalize(target)).ok_or_else(|| {
            ToolError(format!(
                "cannot express {} relative to {} for a {runtime} require (different drives?)",
                target.display(),
                dir.display()
            ))
        })
    };

    let core_require = rel_require(&plan.core_entry)?;
    let mut spec_entries = String::from("{\n");
    for spec in &plan.specs {
        let name = display_rel(spec, &plan.root);
        let require = rel_require(spec)?;
        spec_entries.push_str(&format!(
            "\t{{ name = '{}', load = function ()\n\t\treturn require('{}')\n\tend }},\n",
            escape_luau(&name),
            escape_luau(&require),
        ));
    }
    spec_entries.push('}');

    let name_filter = match &plan.name_filter {
        Some(filter) => format!("'{}'", escape_luau(filter)),
        None => "nil".to_string(),
    };

    let harness = substitute(
        HARNESS_TEMPLATE,
        &[
            (
                "__LEST_CORE_REQUIRE__",
                format!("local Lest = require('{}')", escape_luau(&core_require)),
            ),
            (
                "__LEST_SPECS__",
                format!("local specs: {{ Spec }} = {spec_entries}"),
            ),
            (
                "__LEST_NAME_FILTER__",
                format!("local nameFilter: string? = {name_filter}"),
            ),
        ],
    )?;

    let path = dir.join(harness_file_name(&plan.name));
    std::fs::write(&path, harness)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", path.display())))?;
    Ok(path)
}

/// The harness filename for a suite. `sanitize` folds every punctuation mark
/// to `_`, so distinct suite names (`a b`, `a_b`) can share a stem — the
/// digest of the *original* name keeps their files distinct, or two suites
/// running back to back would silently overwrite each other's harness. FNV is
/// deterministic, so the name is stable across runs.
fn harness_file_name(suite_name: &str) -> String {
    let digest = crate::resolve::hash_bytes(suite_name.as_bytes());
    format!(
        "harness-{}-{:08x}.luau",
        sanitize(suite_name),
        (digest & 0xffff_ffff) as u32
    )
}

/// Replaces each marked line of the harness template with its generated
/// counterpart. A binding's marker is a trailing `-- __LEST_*__` comment, and
/// the *whole line* carrying it is swapped out.
///
/// Substituting lines rather than tokens is what lets the template be ordinary
/// Luau: each placeholder line holds a working default (`local specs: { Spec }
/// = {}`) that parses, type-checks against the real lest/core, and formats,
/// instead of a bare token that would leave the file uncheckable and — because
/// stylua would happily reformat a token sitting inside a table constructor —
/// one `stylua .` away from generating a syntactically broken harness.
///
/// A marker that no line carries is a hard error. The template and this
/// function are edited in different languages by different tools; the failure
/// mode of a silently dropped marker is a harness that keeps its default (no
/// specs at all) and reports an empty green run.
fn substitute(template: &str, bindings: &[(&str, String)]) -> Result<String, ToolError> {
    let mut out = String::new();
    let mut applied = vec![false; bindings.len()];
    let mut lines = template.lines().peekable();

    // The template's header addresses whoever edits it; the copy on disk needs
    // the opposite advice, since every run overwrites it. It goes *below* any
    // `--!` directive: a mode directive only binds while it is still in the
    // file's leading comment block, so banner-first would silently un-type the
    // generated harness.
    while let Some(directive) = lines.next_if(|line| line.starts_with("--!")) {
        out.push_str(directive);
        out.push('\n');
    }
    out.push_str(
        "-- Generated by lest from luau/runtime/harness.luau — do not edit; \
         rewritten on every run.\n",
    );

    for line in lines {
        match bindings
            .iter()
            .position(|(marker, _)| line.contains(marker))
        {
            Some(index) => {
                applied[index] = true;
                out.push_str(&bindings[index].1);
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }

    let missing: Vec<&str> = bindings
        .iter()
        .zip(&applied)
        .filter(|(_, &done)| !done)
        .map(|((marker, _), _)| *marker)
        .collect();
    if !missing.is_empty() {
        return Err(ToolError(format!(
            "the harness template is missing the marker comment(s) {} — every `-- __LEST_*__` \
             line in luau/runtime/harness.luau must survive editing",
            missing.join(", ")
        )));
    }
    Ok(out)
}

/// Escapes a string for a single-quoted Luau literal. Backslash and the quote
/// need escaping to stay inside the literal; the control characters `\n`, `\r`,
/// and `\t` must be escaped too, or a filter like `-t $'foo\nbar'` would inject
/// a raw newline and produce an unterminated string literal.
fn escape_luau(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_reads_a_marker_at_the_start_of_the_line() {
        assert_eq!(
            classify("@@LEST@@{\"kind\":\"run_end\"}"),
            Decoded::Event {
                leading: "",
                json: "{\"kind\":\"run_end\"}"
            }
        );
        assert_eq!(
            classify("@@LEST_SPEC@@2"),
            Decoded::SpecBoundary {
                leading: "",
                index: "2"
            }
        );
    }

    #[test]
    fn classify_recovers_an_event_glued_to_unterminated_test_output() {
        // `stdio.write('busy...')` with no newline puts the next protocol line
        // behind it. Requiring a prefix match would drop the event silently.
        let line = "busy...@@LEST@@{\"kind\":\"test_pass\"}";
        assert_eq!(
            classify(line),
            Decoded::Event {
                leading: "busy...",
                json: "{\"kind\":\"test_pass\"}"
            }
        );
    }

    #[test]
    fn classify_prefers_the_spec_boundary_marker() {
        // The two markers share a prefix; the longer one must win so a boundary
        // is never decoded as an (undecodable) event.
        assert_eq!(
            classify("x@@LEST_SPEC@@1"),
            Decoded::SpecBoundary {
                leading: "x",
                index: "1"
            }
        );
    }

    #[test]
    fn classify_is_not_fooled_by_a_spec_marker_inside_an_event_payload() {
        // The spec marker survives JSON string escaping, so a snapshot's
        // received text (or a failure message, or a test name) can carry it.
        // The earlier marker frames the line: this is an event, and treating
        // it as a boundary was an exit-2 abort — or, with an integer-looking
        // tail, silent snapshot misattribution.
        let line = r#"@@LEST@@{"kind":"snapshot","received":"...@@LEST_SPEC@@1..."}"#;
        assert_eq!(
            classify(line),
            Decoded::Event {
                leading: "",
                json: r#"{"kind":"snapshot","received":"...@@LEST_SPEC@@1..."}"#
            }
        );
        // And the converse: an event marker inside a boundary line's tail does
        // not steal the line from the earlier spec marker.
        assert_eq!(
            classify("@@LEST_SPEC@@2 then @@LEST@@x"),
            Decoded::SpecBoundary {
                leading: "",
                index: "2 then @@LEST@@x"
            }
        );
    }

    #[test]
    fn distinct_suite_names_never_share_a_harness_file() {
        // `sanitize` folds "a b" and "a_b" to one stem; the appended digest
        // must keep the files apart.
        assert_ne!(harness_file_name("a b"), harness_file_name("a_b"));
        // Deterministic: the same suite keeps the same file across runs.
        assert_eq!(harness_file_name("unit"), harness_file_name("unit"));
    }

    #[test]
    fn classify_passes_unmarked_lines_through() {
        assert_eq!(classify("just some print output"), Decoded::Output);
        assert_eq!(classify(""), Decoded::Output);
    }

    #[test]
    fn escape_luau_escapes_control_characters() {
        assert_eq!(escape_luau("foo\nbar"), "foo\\nbar");
        assert_eq!(escape_luau("a\tb\r"), "a\\tb\\r");
        assert_eq!(escape_luau("it's\\ok"), "it\\'s\\\\ok");
    }

    #[test]
    fn substitute_replaces_the_whole_marked_line() {
        let template = "local specs = {} -- __LEST_SPECS__\nprint(specs)";
        let out = substitute(
            template,
            &[("__LEST_SPECS__", "local specs = { 1 }".to_string())],
        )
        .unwrap();
        assert!(out.contains("local specs = { 1 }\n"), "{out}");
        assert!(!out.contains("__LEST_SPECS__"), "{out}");
        assert!(out.contains("print(specs)"), "{out}");
    }

    #[test]
    fn substitute_keeps_the_banner_below_mode_directives() {
        let out = substitute(
            "--!strict\n-- x -- __LEST_X__",
            &[("__LEST_X__", "y".into())],
        )
        .unwrap();
        assert!(
            out.starts_with("--!strict\n"),
            "a banner above --!strict would silently un-type the harness: {out}"
        );
        assert!(
            out.lines().nth(1).unwrap().contains("Generated by lest"),
            "{out}"
        );
    }

    #[test]
    fn substitute_rejects_a_marker_the_template_dropped() {
        // The template keeps a working default on every marked line, so a lost
        // marker would otherwise generate a harness with no specs at all — a
        // green run over nothing.
        let err = substitute("local specs = {}", &[("__LEST_SPECS__", "x".into())])
            .expect_err("a missing marker must not pass silently");
        assert!(err.to_string().contains("__LEST_SPECS__"), "{err}");
    }
}
