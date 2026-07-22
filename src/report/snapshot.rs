//! Canonical snapshot format, summary counts, and failure diffs.
//!
//! This is the "stable text format from the canonical pretty-printer" the
//! architecture calls for — deliberately *not* the terminal [`Pretty`] view,
//! which emits ANSI and glyphs. The CLI owns the file IO and the pass/fail
//! comparison; this module owns the on-disk format and the diff rendering so
//! both live in exactly one place.
//!
//! [`Pretty`]: super::Pretty
//!
//! # `.snap` file format
//!
//! ```text
//! lest snapshot v1
//!
//! <escaped-key> <line-count>
//! <value line 1>
//! …
//! <value line n>
//! ```
//!
//! * The first line is always the literal header `lest snapshot v1`.
//! * Each entry is a header line `"<escaped-key> <line-count>"` followed by
//!   exactly `<line-count>` lines of **literal** value content. Values are
//!   never escaped, so a stored snapshot reads as itself and line-oriented
//!   diffs (git, editors) stay meaningful.
//! * `<line-count>` is `value.split('\n').count()`, so it is always `>= 1`
//!   (the empty string is one empty line). The value round-trips as
//!   `lines.join("\n")`. Splitting is on `\n` alone in **both** directions: a
//!   `\r` is ordinary value content, never a line terminator.
//! * Only the **key** is escaped, and only for the two characters that would
//!   otherwise break the header line: `\` becomes `\\` and a newline becomes
//!   `\n`. Spaces in keys are fine because the line count is split off from the
//!   right.
//! * Entries are sorted by key and separated by a single blank line, making the
//!   file deterministic regardless of insertion order.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::pretty::{paint, GREEN, RED};

/// The header stamped at the top of every `.snap` file.
const HEADER: &str = "lest snapshot v1";

/// A parsed snapshot file: stable map of `key -> received string`. This is the
/// shared type the CLI constructs when writing snapshots and compares
/// against when reading them.
pub type SnapshotFile = BTreeMap<String, String>;

/// Tally of what happened to snapshots during a run, built by the CLI from
/// its own comparisons (the host decides pass/fail per the protocol) and handed
/// to [`Pretty::finish`](super::Pretty::finish) for the `Snapshots:` line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotSummary {
    /// Keys whose received value equalled the stored value.
    pub matched: u32,
    /// Keys written for the first time (absent → write and pass).
    pub written: u32,
    /// Keys overwritten under `lest -u`.
    pub updated: u32,
    /// Keys whose received value differed (and `-u` was not set).
    pub failed: u32,
    /// Stored keys that no test produced this run.
    pub obsolete: u32,
}

impl SnapshotSummary {
    /// Snapshots that were exercised this run: matched, written, updated, or
    /// failed. Obsolete keys are reported but not part of the total, matching
    /// how the summary line reads.
    pub fn total(&self) -> u32 {
        self.matched + self.written + self.updated + self.failed
    }

    /// True when nothing happened to any snapshot — no keys touched and none
    /// left obsolete.
    pub fn is_empty(&self) -> bool {
        self.total() == 0 && self.obsolete == 0
    }
}

/// Serializes a snapshot map into the canonical `.snap` text. Deterministic:
/// keys are emitted in sorted order (`BTreeMap` guarantees it) so the file only
/// changes when a snapshot changes.
pub fn serialize_snapshots(snapshots: &SnapshotFile) -> String {
    let mut out = String::new();
    out.push_str(HEADER);
    out.push('\n');
    for (key, value) in snapshots {
        out.push('\n');
        let lines: Vec<&str> = value.split('\n').collect();
        let _ = writeln!(out, "{} {}", escape_key(key), lines.len());
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// An error encountered while parsing a `.snap` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotParseError {
    /// The file did not start with the expected `lest snapshot v1` header.
    MissingHeader,
    /// An entry header line lacked a trailing integer line count.
    MissingLineCount { line: usize },
    /// An entry header's line count was not a valid non-negative integer.
    InvalidLineCount { line: usize, found: String },
    /// The file ended before an entry's declared line count was satisfied.
    UnexpectedEof {
        key: String,
        expected: usize,
        found: usize,
    },
}

impl std::fmt::Display for SnapshotParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHeader => {
                write!(f, "not a lest snapshot file (missing `{HEADER}` header)")
            }
            Self::MissingLineCount { line } => {
                write!(f, "line {line}: snapshot entry header has no line count")
            }
            Self::InvalidLineCount { line, found } => {
                write!(f, "line {line}: invalid snapshot line count {found:?}")
            }
            Self::UnexpectedEof {
                key,
                expected,
                found,
            } => write!(
                f,
                "snapshot {key:?} declared {expected} lines but file ended after {found}"
            ),
        }
    }
}

impl std::error::Error for SnapshotParseError {}

/// Parses canonical `.snap` text back into a snapshot map. The inverse of
/// [`serialize_snapshots`]; a value round-trips exactly.
pub fn parse_snapshots(text: &str) -> Result<SnapshotFile, SnapshotParseError> {
    // `split('\n')`, not `lines()`. `lines()` strips a trailing `\r` while
    // `serialize_snapshots` writes it out verbatim, so any value containing
    // `\r\n` parsed back one byte short and compared unequal against itself
    // forever — and `-u` could not repair it, because rewriting produced the
    // same bytes that re-parse the same wrong way, rendering a diff of two
    // apparently identical texts.
    //
    // A trailing `\r` is tolerated only on the *structural* lines (the file
    // header and an entry header's line count), so a `.snap` that some tool
    // converted to CRLF still opens; value lines are taken byte for byte.
    let mut lines = text.split('\n').enumerate();

    match lines.next() {
        Some((_, first)) if strip_cr(first) == HEADER => {}
        _ => return Err(SnapshotParseError::MissingHeader),
    }

    let mut snapshots = SnapshotFile::new();
    let mut pending = lines.next();
    while let Some((idx, header)) = pending {
        // Skip blank separator lines between entries (and the empty trailing
        // element `split` yields for the file's final newline).
        if strip_cr(header).is_empty() {
            pending = lines.next();
            continue;
        }

        let (raw_key, raw_count) = header
            .rsplit_once(' ')
            .ok_or(SnapshotParseError::MissingLineCount { line: idx + 1 })?;
        let raw_count = strip_cr(raw_count);
        let count: usize = raw_count
            .parse()
            .map_err(|_| SnapshotParseError::InvalidLineCount {
                line: idx + 1,
                found: raw_count.to_string(),
            })?;
        let key = unescape_key(raw_key);

        let mut collected = Vec::with_capacity(count);
        for _ in 0..count {
            match lines.next() {
                Some((_, line)) => collected.push(line),
                None => {
                    return Err(SnapshotParseError::UnexpectedEof {
                        key,
                        expected: count,
                        found: collected.len(),
                    })
                }
            }
        }
        snapshots.insert(key, collected.join("\n"));
        pending = lines.next();
    }

    Ok(snapshots)
}

/// Drops one trailing `\r`, for the structural lines that tolerate CRLF.
fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

fn escape_key(key: &str) -> String {
    key.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut chars = key.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Renders a line-oriented diff between the stored (`expected`) and produced
/// (`received`) snapshot values, for the failure output. Colored consistently
/// with the rest of `pretty` when `color` is on: expected lines green, received
/// lines red (matching the assertion palette), context lines plain. A leading
/// `- ` marks expected-only lines, `+ ` received-only, and `  ` shared context.
pub fn diff(expected: &str, received: &str, color: bool) -> String {
    let old: Vec<&str> = expected.split('\n').collect();
    let new: Vec<&str> = received.split('\n').collect();

    // The LCS table is quadratic. Two 5 000-line snapshots would want ~200 MB
    // of `usize`s — on a run that is already failing, which is the worst moment
    // to start allocating. Past the cap, render both blocks whole: less
    // pleasant, still complete, and bounded.
    if old.len() > MAX_LCS_LINES || new.len() > MAX_LCS_LINES {
        let mut out = String::new();
        for line in &old {
            let _ = writeln!(out, "{}", paint(color, GREEN, &format!("- {line}")));
        }
        for line in &new {
            let _ = writeln!(out, "{}", paint(color, RED, &format!("+ {line}")));
        }
        return out;
    }

    let lcs = lcs_table(&old, &new);
    // One flat allocation with stride indexing rather than a `Vec<Vec<_>>`:
    // `old.len() + 1` separate allocations was the other half of the cost.
    let stride = new.len() + 1;
    let lcs = |i: usize, j: usize| lcs[i * stride + j];

    let mut out = String::new();
    let mut i = 0;
    let mut j = 0;
    // Walk the LCS table, emitting removed/added/context lines in order.
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            let _ = writeln!(out, "  {}", old[i]);
            i += 1;
            j += 1;
        } else if lcs(i + 1, j) >= lcs(i, j + 1) {
            let _ = writeln!(out, "{}", paint(color, GREEN, &format!("- {}", old[i])));
            i += 1;
        } else {
            let _ = writeln!(out, "{}", paint(color, RED, &format!("+ {}", new[j])));
            j += 1;
        }
    }
    while i < old.len() {
        let _ = writeln!(out, "{}", paint(color, GREEN, &format!("- {}", old[i])));
        i += 1;
    }
    while j < new.len() {
        let _ = writeln!(out, "{}", paint(color, RED, &format!("+ {}", new[j])));
        j += 1;
    }

    out
}

/// The per-side line count above which [`diff`] stops building an LCS table
/// and renders both blocks whole instead. Chosen so the table stays in the tens
/// of megabytes at worst.
const MAX_LCS_LINES: usize = 2_000;

/// Standard LCS length DP, flattened to one allocation: cell `(i, j)` lives at
/// `i * (new.len() + 1) + j` and holds the longest common subsequence of
/// `old[i..]` and `new[j..]`, used to reconstruct a minimal line diff.
fn lcs_table(old: &[&str], new: &[&str]) -> Vec<usize> {
    let stride = new.len() + 1;
    let mut table = vec![0usize; stride * (old.len() + 1)];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            table[i * stride + j] = if old[i] == new[j] {
                table[(i + 1) * stride + j + 1] + 1
            } else {
                table[(i + 1) * stride + j].max(table[i * stride + j + 1])
            };
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_multiline_and_special_values() {
        let mut snaps = SnapshotFile::new();
        snaps.insert("simple".into(), "hello".into());
        snaps.insert("multi line key".into(), "a\nb\nc".into());
        snaps.insert("empty".into(), String::new());
        snaps.insert("trailing newline".into(), "x\n".into());
        snaps.insert("blank in middle".into(), "top\n\nbottom".into());
        // A `\r` is content, not a terminator: `lines()` used to eat these on
        // the way back in, so the value could never match itself again.
        snaps.insert("crlf".into(), "a\r\nb\r\nc".into());
        snaps.insert("trailing cr".into(), "ends with cr\r".into());
        snaps.insert("lone cr".into(), "one\rline".into());

        let text = serialize_snapshots(&snaps);
        assert!(text.starts_with("lest snapshot v1\n"));
        let parsed = parse_snapshots(&text).unwrap();
        assert_eq!(parsed, snaps);
    }

    #[test]
    fn crlf_value_compares_equal_after_a_round_trip() {
        // The failure this guards: store, reload, compare — a mismatch here is
        // a permanently red snapshot that `-u` cannot fix.
        let mut snaps = SnapshotFile::new();
        let value = "line one\r\nline two\r\n";
        snaps.insert("windows output".into(), value.into());
        let reparsed = parse_snapshots(&serialize_snapshots(&snaps)).unwrap();
        assert_eq!(
            reparsed.get("windows output").map(String::as_str),
            Some(value)
        );
    }

    #[test]
    fn serialization_is_sorted_and_stable() {
        let mut a = SnapshotFile::new();
        a.insert("zebra".into(), "z".into());
        a.insert("alpha".into(), "a".into());
        let text = serialize_snapshots(&a);
        let alpha = text.find("alpha").unwrap();
        let zebra = text.find("zebra").unwrap();
        assert!(alpha < zebra);
    }

    #[test]
    fn key_with_backslash_and_newline_round_trips() {
        let mut snaps = SnapshotFile::new();
        snaps.insert("weird\\key\nwrapped".into(), "value".into());
        let text = serialize_snapshots(&snaps);
        let parsed = parse_snapshots(&text).unwrap();
        assert_eq!(parsed, snaps);
    }

    #[test]
    fn rejects_missing_header() {
        assert_eq!(
            parse_snapshots("nope\n"),
            Err(SnapshotParseError::MissingHeader)
        );
    }

    #[test]
    fn rejects_truncated_entry() {
        let text = "lest snapshot v1\n\nkey 3\nonly one line\n";
        match parse_snapshots(text) {
            Err(SnapshotParseError::UnexpectedEof { expected, .. }) => assert_eq!(expected, 3),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn diff_marks_changed_lines() {
        let out = diff("a\nb\nc", "a\nB\nc", false);
        assert!(out.contains("  a"));
        assert!(out.contains("- b"));
        assert!(out.contains("+ B"));
        assert!(out.contains("  c"));
    }

    #[test]
    fn diff_falls_back_to_whole_blocks_past_the_lcs_cap() {
        // Beyond the cap there is no table at all, so nothing quadratic is
        // allocated on a run that is already failing.
        let old: String = (0..=MAX_LCS_LINES)
            .map(|i| format!("{i}\n"))
            .collect::<String>();
        let new: String = (0..=MAX_LCS_LINES)
            .map(|i| format!("x{i}\n"))
            .collect::<String>();
        let out = diff(&old, &new, false);
        assert!(out.contains("- 0\n"));
        assert!(out.contains("+ x0\n"));
        // Every line is marked; the fallback emits no shared context.
        assert!(out
            .lines()
            .all(|line| line.starts_with("- ") || line.starts_with("+ ")));
    }

    #[test]
    fn diff_colors_when_enabled() {
        let out = diff("old", "new", true);
        assert!(out.contains("\x1b[32m- old"));
        assert!(out.contains("\x1b[31m+ new"));
    }

    #[test]
    fn summary_total_excludes_obsolete() {
        let summary = SnapshotSummary {
            matched: 2,
            written: 1,
            updated: 1,
            failed: 1,
            obsolete: 3,
        };
        assert_eq!(summary.total(), 5);
        assert!(!summary.is_empty());
        assert!(SnapshotSummary::default().is_empty());
    }
}
