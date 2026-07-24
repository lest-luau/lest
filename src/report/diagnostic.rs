//! The CLI's diagnostic voice: the `Error:`, `Failure:`, and `Warning:` labels.
//!
//! Every diagnostic in the crate renders through here, so a bad flag, a bad
//! config, a missed coverage gate, and a dropped snapshot all read alike: a
//! bold colored label, then the message as a capitalized sentence with **no
//! trailing period**. Messages are written as lowercase fragments everywhere
//! else in the crate — [`sentence`] is the one place they become sentences, so
//! the same string can be a fatal error here and a running-log note elsewhere.

use super::pretty::{paint, BOLD_RED, BOLD_YELLOW, DIM};

/// Capitalizes a message fragment into the diagnostic voice: the first letter
/// is uppercased and a single trailing period is stripped — a rendered
/// diagnostic never ends with one. Interior punctuation is left untouched, as
/// is a trailing `:` (it introduces the lines below it), `!`, or `?`.
pub fn sentence(text: &str) -> String {
    let text = text.strip_suffix('.').unwrap_or(text);
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Which of the two non-zero exits a diagnostic belongs to. The labels differ
/// so the reader can tell the exit code from the message alone: lest broke, or
/// the code under test did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// lest itself could not do its job — exit 2.
    Error,
    /// The code under test did not meet the bar — exit 1.
    Failure,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "Error:",
            Severity::Failure => "Failure:",
        }
    }
}

/// Renders a diagnostic in the CLI's one error voice — the bold red label and
/// capitalized sentence `restyle_parse_error` (in `main`) gives clap's parse
/// failures, so every diagnostic reads alike regardless of who produced it.
/// The leading blank line separates it from whatever the reporter printed
/// last.
pub fn render_diagnostic(severity: Severity, message: &str, color: bool) -> String {
    render_labeled(severity.label(), BOLD_RED, message, color)
}

/// Renders a warning: the same shape as [`render_diagnostic`] but with a bold
/// yellow `Warning:` label. Warnings carry no exit code and always belong on
/// stderr — the caller `eprint!`s the result, deciding `color` from stderr's
/// terminal state.
pub fn render_warning(message: &str, color: bool) -> String {
    render_labeled("Warning:", BOLD_YELLOW, message, color)
}

/// The one shape every diagnostic wears; the label and its color are the only
/// degrees of freedom, so the voices cannot drift apart.
fn render_labeled(label: &str, code: &str, message: &str, color: bool) -> String {
    format!("\n{} {}\n", paint(color, code, label), sentence(message))
}

/// Whether stderr output should be colored, decided by the same rule `main`
/// uses — an argv `--no-color` (stopping at the `--` escape), `NO_COLOR`, and
/// stderr's own terminal state. Recomputed here because the callers of
/// [`warn_to_stderr`]/[`note_to_stderr`] have no reporter handle: config
/// loading runs before the CLI has computed its color flags, and the cloud
/// backend is plumbed for events, not diagnostics. The flags cannot change
/// mid-process, so this always agrees with `main`.
fn stderr_color() -> bool {
    use std::io::IsTerminal;
    let no_color = std::env::args()
        .take_while(|a| a != "--")
        .any(|a| a == "--no-color")
        || std::env::var_os("NO_COLOR").is_some();
    !no_color && std::io::stderr().is_terminal()
}

/// Renders and prints a warning to stderr.
pub fn warn_to_stderr(message: &str) {
    eprint!("{}", render_warning(message, stderr_color()));
}

/// Prints a note — the dim lowercase voice — to stderr, for progress from
/// places with no reporter handle. stderr rather than stdout because stdout
/// belongs to the reporters: a stray line there corrupts `--reporter json`
/// output and lcov streams.
pub fn note_to_stderr(message: &str) {
    eprintln!("{}", paint(stderr_color(), DIM, message));
}

/// Erases the most recent single-line stderr note, when stderr is a terminal
/// that honors ANSI (the same gate as coloring). For transient status lines
/// — "launching Roblox Studio…" — that should not outlive the moment they
/// describe. In non-terminal stderr (CI logs, redirects) the note stays,
/// which is exactly what a log wants; the caller need not care which
/// happened.
pub fn clear_stderr_note() {
    if stderr_color() {
        eprint!("\x1b[1A\x1b[2K");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_capitalizes_and_never_ends_with_a_period() {
        assert_eq!(sentence("cannot run git."), "Cannot run git");
        assert_eq!(sentence("cannot run git"), "Cannot run git");
        // A trailing colon introduces the lines below it and survives.
        assert_eq!(sentence("available suites:"), "Available suites:");
        // Interior periods are not touched.
        assert_eq!(
            sentence("see lest.toml for details"),
            "See lest.toml for details"
        );
        assert_eq!(sentence(""), "");
    }

    /// The label carries the exit class: exit 2 is lest's fault, exit 1 is the
    /// project's, and the reader can tell which from the first word.
    #[test]
    fn labels_carry_the_exit_class() {
        assert_eq!(Severity::Error.label(), "Error:");
        assert_eq!(Severity::Failure.label(), "Failure:");
    }

    #[test]
    fn diagnostics_wear_a_bold_red_label_and_no_trailing_period() {
        assert_eq!(
            render_diagnostic(Severity::Error, "cannot find the config file", true),
            "\n\x1b[1;31mError:\x1b[0m Cannot find the config file\n"
        );
        assert_eq!(
            render_diagnostic(
                Severity::Failure,
                "coverage 61.2% is below the minimum",
                false
            ),
            "\nFailure: Coverage 61.2% is below the minimum\n"
        );
    }

    #[test]
    fn warnings_wear_bold_yellow_in_the_same_shape() {
        assert_eq!(
            render_warning("snapshot \"k\" was not compared", true),
            "\n\x1b[1;33mWarning:\x1b[0m Snapshot \"k\" was not compared\n"
        );
        assert_eq!(render_warning("plain", false), "\nWarning: Plain\n");
    }
}
