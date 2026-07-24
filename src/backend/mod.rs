use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::report::{Event, Failure};

pub mod cloud;
pub mod native;
pub mod runtime;
pub mod studio;

/// The sink a backend streams protocol events into. The first argument is the
/// originating spec file when the backend can attribute it (always for native,
/// which runs each file in its own VM; for the runtime backend it becomes known
/// once a per-spec boundary marker is decoded). Snapshot attribution and any
/// per-file bookkeeping in the CLI keys off it.
pub type EventSink<'a> = dyn FnMut(Option<&Path>, &Event) + 'a;

/// Everything a backend needs to execute one suite. Reporters and totals
/// consume the resulting event stream without knowing which backend ran it.
pub struct SuitePlan {
    pub name: String,
    pub specs: Vec<PathBuf>,
    pub root: PathBuf,
    /// Normalized absolute path to lest/core's `init.luau`.
    pub core_entry: PathBuf,
    /// Per-test budget (native); scaled into a per-process budget for
    /// spawned runtimes, whose tests can only be timed out by killing the
    /// process.
    pub timeout: Duration,
    /// Native-backend worker threads; `0` means one per CPU.
    pub workers: usize,
    /// Plain-substring filter against full test names (`-t`).
    pub name_filter: Option<String>,
    /// Compile specs with Luau coverage instrumentation (native only).
    pub coverage: bool,
    /// Absolute path to the rojo project file (`[settings] rojo`), when set.
    /// Only the cloud backend consults it: string requires whose targets it
    /// maps into the place delegate to the engine's `require` instead of
    /// bundling a private copy.
    pub rojo_project: Option<PathBuf>,
    /// `[studio] executable` — the Roblox Studio binary, for non-standard
    /// installs. Only the studio backend consults it.
    pub studio_executable: Option<PathBuf>,
}

/// Root-relative display form of a spec path, with forward slashes.
///
/// The *comparison* is done on normalized paths so the strip succeeds even when
/// one side was case-folded (on case-insensitive hosts) and the other was not.
/// The *output* is sliced from the original `path`, because this string is not
/// merely cosmetic: it names snapshot files (`__snapshots__/<name>.snap`), JUnit
/// classnames, and coverage-exclude match targets. Emitting the folded form
/// would lowercase snapshot filenames on Windows/macOS — so a `.snap` committed
/// from one platform is invisible to another, and an absent snapshot silently
/// writes-and-passes rather than comparing.
pub fn display_rel(path: &Path, root: &Path) -> String {
    let norm_path = crate::resolve::normalize(path);
    let norm_root = crate::resolve::normalize(root);
    let depth = match norm_path.strip_prefix(&norm_root) {
        Ok(rel) => rel.components().count(),
        Err(_) => norm_path.components().count(),
    };
    // Take the same number of trailing components from the un-folded path.
    // `normalize` is component-preserving for the absolute, already-clean paths
    // that reach here, so the counts line up.
    let components: Vec<_> = path.components().collect();
    let start = components.len().saturating_sub(depth);
    components[start..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Builds the string for a `require()` in a file living in `from_dir`,
/// pointing at `target` on disk: relative, forward slashes, extension
/// stripped per the require-by-string convention, and init modules addressed
/// through their directory.
pub fn require_string(from_dir: &Path, target: &Path) -> Option<String> {
    let stem = match target.extension().and_then(|e| e.to_str()) {
        Some("luau") | Some("lua") => target.with_extension(""),
        _ => target.to_path_buf(),
    };
    // An init module cannot be required by the name "init"; requiring its
    // directory is the convention that reaches it.
    let stem = if stem.file_name().is_some_and(|n| n == "init") {
        stem.parent().map(Path::to_path_buf).unwrap_or(stem)
    } else {
        stem
    };
    relative_path(from_dir, &stem)
}

pub fn relative_path(from: &Path, to: &Path) -> Option<String> {
    use std::path::Component;
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    if common == 0 {
        return None;
    }
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    for component in &to[common..] {
        parts.push(component.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        return Some(".".to_string());
    }
    let joined = parts.join("/");
    Some(if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    })
}

/// Whether path text folds case when compared, mirroring the platform rule
/// `resolve::normalize` applies to real paths.
const CASE_INSENSITIVE_PATHS: bool = cfg!(any(target_os = "windows", target_os = "macos"));

fn fold_path_char(c: char) -> char {
    if c == '\\' {
        '/'
    } else if CASE_INSENSITIVE_PATHS {
        c.to_ascii_lowercase()
    } else {
        c
    }
}

/// The folded form of `path` plus a trailing separator, ready for
/// [`match_path_prefix`]. The trailing separator is what keeps a root of
/// `/proj/app` from matching inside `/proj/appendix`.
fn fold_needle(path: &Path) -> Vec<char> {
    format!("{}/", path.display())
        .chars()
        .map(fold_path_char)
        .collect()
}

/// Matches `needle` (a folded path) at `chars[start..]`, comparing case- and
/// separator-insensitively. A separator in the needle matches a *run* of
/// separators in the text, so a path quoted with escaped backslashes
/// (`c:\\users\\…`) still matches. Returns the index just past the match.
fn match_path_prefix(chars: &[char], start: usize, needle: &[char]) -> Option<usize> {
    let mut i = start;
    for &expected in needle {
        if expected == '/' {
            let mut separators = 0usize;
            while i < chars.len() && fold_path_char(chars[i]) == '/' {
                separators += 1;
                i += 1;
            }
            if separators == 0 {
                return None;
            }
        } else {
            if i >= chars.len() || fold_path_char(chars[i]) != expected {
                return None;
            }
            i += 1;
        }
    }
    Some(i)
}

/// Removes every occurrence of the absolute project root — any casing, either
/// separator, even doubled by string escaping — plus its trailing separator
/// from `text`, leaving root-relative paths behind. Failure messages and
/// traces quote paths in whatever spelling produced them (chunk names are
/// case-folded by `normalize`, spec paths keep discovery casing), so this
/// works on the text rather than parsing paths out of it; text without the
/// root passes through untouched.
pub fn strip_root(text: &str, root: &Path) -> String {
    let needle = fold_needle(root);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match match_path_prefix(&chars, i, &needle) {
            Some(next) => i = next,
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Whether `text` begins with the path `prefix` (same folding rules) followed
/// by a separator.
fn starts_with_path(text: &str, prefix: &Path) -> bool {
    let chars: Vec<char> = text.chars().collect();
    match_path_prefix(&chars, 0, &fold_needle(prefix)).is_some()
}

/// Cleans one failure for human eyes, in place. Applied by the CLI to every
/// `test_fail` from every backend, so the reporters and machine documents all
/// agree:
///
/// - Project paths in messages, traces, and assertion values become
///   root-relative (`src/ledger.luau:41: …` instead of an absolute path that
///   is longer than the message).
/// - Trace frames inside lest's own core drop out — core takes the traceback,
///   so `protectedCall`/`run` frames appeared in every single failure — as do
///   `[C]` frames.
/// - mlua's framing on native errors is normalized: the `runtime error: `
///   prefix goes, and a traceback embedded in the message (load failures)
///   moves into the trace field where every other failure carries it.
pub fn polish_failure(failure: &mut Failure, root: &Path, core_entry: &Path) {
    let core_dir = core_entry.parent();
    match failure {
        Failure::Assertion {
            message,
            expected,
            received,
        } => {
            *message = strip_root(message, root);
            if let Some(expected) = expected {
                *expected = strip_root(expected, root);
            }
            if let Some(received) = received {
                *received = strip_root(received, root);
            }
        }
        Failure::Error { message, trace } => {
            if let Some(rest) = message.strip_prefix("runtime error: ") {
                *message = rest.to_string();
            }
            if let Some(split) = message.find("\nstack traceback:") {
                let embedded = message[split + 1..]
                    .strip_prefix("stack traceback:")
                    .unwrap_or(&message[split + 1..])
                    .trim_matches('\n')
                    .to_string();
                message.truncate(split);
                message.truncate(message.trim_end().len());
                match trace {
                    Some(existing) if !existing.trim().is_empty() => {
                        existing.push('\n');
                        existing.push_str(&embedded);
                    }
                    _ => *trace = Some(embedded),
                }
            }
            *message = strip_root(message, root);
            if let Some(text) = trace {
                let filtered = filter_trace(text, root, core_dir);
                *trace = if filtered.is_empty() {
                    None
                } else {
                    Some(filtered)
                };
            }
        }
        // Host-synthesized after polishing, from values the store already
        // rendered — there are no paths or traces in a snapshot diff to clean.
        Failure::Snapshot { .. } => {}
    }
}

/// Root-strips every trace line and drops the frames a reader never wants:
/// lest's own plumbing under the core directory, and `[C]` frames. Core may
/// appear absolute (a core outside the root) or already root-relative (cloud
/// traces arrive source-mapped), so both spellings are tested. Frames are
/// also unindented — mlua tab-indents its traceback lines, core's
/// `debug.traceback` does not, and the reporter supplies its own padding.
fn filter_trace(trace: &str, root: &Path, core_dir: Option<&Path>) -> String {
    let core_rel: Option<PathBuf> =
        core_dir.map(|dir| PathBuf::from(strip_root(&dir.display().to_string(), root)));
    trace
        .lines()
        .filter_map(|line| {
            let stripped = strip_root(line, root);
            let frame = stripped.trim();
            if frame.is_empty() || frame.starts_with("[C]") {
                return None;
            }
            if let (Some(dir), Some(rel)) = (core_dir, &core_rel) {
                if starts_with_path(frame, dir) || starts_with_path(frame, rel) {
                    return None;
                }
            }
            Some(frame.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod polish_tests {
    use super::*;

    #[test]
    fn strip_root_removes_every_occurrence_and_keeps_the_rest() {
        let root = Path::new("/proj/app");
        let text = "error at /proj/app/src/mod.luau:41: boom (/proj/app/src/mod.luau:41)";
        assert_eq!(
            strip_root(text, root),
            "error at src/mod.luau:41: boom (src/mod.luau:41)"
        );
        assert_eq!(strip_root("no paths here", root), "no paths here");
        // The trailing separator keeps the root from matching inside a longer
        // sibling name.
        assert_eq!(strip_root("/proj/appendix/x", root), "/proj/appendix/x");
    }

    #[test]
    fn strip_root_matches_across_separator_styles_and_escaping() {
        let root = Path::new("c:\\proj\\app");
        // Forward slashes in the text, backslashes in the root.
        assert_eq!(strip_root("c:/proj/app/src/mod.luau", root), "src/mod.luau");
        // Escaped backslashes, as an error message quoting a Luau string
        // literal renders them.
        assert_eq!(
            strip_root(r"c:\\proj\\app\\src\\mod.luau", root),
            r"src\\mod.luau"
        );
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn strip_root_folds_case_on_case_insensitive_hosts() {
        // Chunk names arrive case-folded by `normalize`; spec paths keep
        // their discovery casing. Both spellings must strip.
        let root = Path::new("C:\\Proj\\App");
        assert_eq!(
            strip_root("c:\\proj\\app\\src\\mod.luau", root),
            "src\\mod.luau"
        );
        assert_eq!(
            strip_root("C:\\Proj\\App\\src\\mod.luau", root),
            "src\\mod.luau"
        );
    }

    #[test]
    fn polish_normalizes_a_native_load_failure() {
        let root = Path::new("/proj/app");
        let core = Path::new("/proj/app/.lest/core/init.luau");
        let mut failure = Failure::Error {
            message: "runtime error: cannot resolve require(\"../x\"): no file at\n  \
                      /proj/app/src/x.luau\n\
                      stack traceback:\n\
                      \t[C]: in function 'error'\n\
                      \t/proj/app/.lest/core/init.luau:104: in function 'describe'\n\
                      \t/proj/app/spec.luau:5: in function <spec.luau:1>"
                .to_string(),
            trace: None,
        };
        polish_failure(&mut failure, root, core);
        let Failure::Error { message, trace } = failure else {
            panic!("variant changed");
        };
        // Prefix gone, embedded traceback moved out, paths relative.
        assert_eq!(
            message,
            "cannot resolve require(\"../x\"): no file at\n  src/x.luau"
        );
        // `[C]` and core frames dropped; the spec frame survives, unindented.
        assert_eq!(
            trace.as_deref(),
            Some("spec.luau:5: in function <spec.luau:1>")
        );
    }

    #[test]
    fn polish_drops_core_frames_from_ordinary_traces() {
        let root = Path::new("/proj/app");
        let core = Path::new("/proj/app/.lest/core/init.luau");
        let mut failure = Failure::Error {
            message: "/proj/app/src/ledger.luau:41: unknown account 'ghost'".to_string(),
            trace: Some(
                "/proj/app/.lest/core/init.luau:307\n\
                 /proj/app/src/ledger.luau:41 function balance\n\
                 /proj/app/tests/ledger.spec.luau:28 function level3\n\
                 /proj/app/.lest/core/init.luau:306 function protectedCall\n\
                 /proj/app/.lest/core/init.luau:557 function run"
                    .to_string(),
            ),
        };
        polish_failure(&mut failure, root, core);
        let Failure::Error { message, trace } = failure else {
            panic!("variant changed");
        };
        assert_eq!(message, "src/ledger.luau:41: unknown account 'ghost'");
        assert_eq!(
            trace.as_deref(),
            Some(
                "src/ledger.luau:41 function balance\n\
                 tests/ledger.spec.luau:28 function level3"
            )
        );
    }

    /// Cloud traces arrive already source-mapped to root-relative paths; core
    /// frames must drop in that spelling too, and a dogfood-style core
    /// outside `.lest` (settings.core) works the same way.
    #[test]
    fn polish_drops_relative_core_frames_from_mapped_traces() {
        let root = Path::new("/proj/app");
        let core = Path::new("/proj/app/luau/core/init.luau");
        let mut failure = Failure::Error {
            message: "tests/engine/foo.spec.luau:12: boom".to_string(),
            trace: Some(
                "luau/core/init.luau:306 function protectedCall\n\
                 tests/engine/foo.spec.luau:12 function explode"
                    .to_string(),
            ),
        };
        polish_failure(&mut failure, root, core);
        let Failure::Error { trace, .. } = failure else {
            panic!("variant changed");
        };
        assert_eq!(
            trace.as_deref(),
            Some("tests/engine/foo.spec.luau:12 function explode")
        );
    }

    #[test]
    fn polish_empties_an_all_plumbing_trace_to_none() {
        let root = Path::new("/proj/app");
        let core = Path::new("/proj/app/.lest/core/init.luau");
        let mut failure = Failure::Error {
            message: "boom".to_string(),
            trace: Some("/proj/app/.lest/core/init.luau:307\n[C]: in ?".to_string()),
        };
        polish_failure(&mut failure, root, core);
        let Failure::Error { trace, .. } = failure else {
            panic!("variant changed");
        };
        assert_eq!(trace, None);
    }

    #[test]
    fn polish_relativizes_assertion_values() {
        let root = Path::new("/proj/app");
        let core = Path::new("/proj/app/.lest/core/init.luau");
        let mut failure = Failure::Assertion {
            message: "expected error message to contain \"other words\"".to_string(),
            expected: Some("\"other words\"".to_string()),
            received: Some("/proj/app/src/money.luau:28: allocate needs a ratio".to_string()),
        };
        polish_failure(&mut failure, root, core);
        let Failure::Assertion { received, .. } = failure else {
            panic!("variant changed");
        };
        assert_eq!(
            received.as_deref(),
            Some("src/money.luau:28: allocate needs a ratio")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A `C:\...` literal is a single relative component on Unix, so fixtures
    // must build an absolute path in the host's own syntax or every
    // assertion dies at the `common == 0` guard instead of testing anything.
    fn abs(tail: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\{}", tail.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/{tail}"))
        }
    }

    #[test]
    fn relative_path_walks_up_and_down() {
        let from = abs("proj/.lest");
        let to = abs("proj/luau/core/mod");
        assert_eq!(relative_path(&from, &to).unwrap(), "../luau/core/mod");
    }

    #[test]
    fn relative_path_prefixes_descents_with_dot() {
        let from = abs("proj");
        let to = abs("proj/src/mod");
        assert_eq!(relative_path(&from, &to).unwrap(), "./src/mod");
    }

    // Drive prefixes exist only on Windows; on Unix these literals are two
    // relative components and the assertion would pass vacuously.
    #[cfg(windows)]
    #[test]
    fn relative_path_rejects_different_drives() {
        let from = PathBuf::from("C:\\proj\\.lest");
        let to = PathBuf::from("D:\\other\\mod");
        assert_eq!(relative_path(&from, &to), None);
    }

    #[test]
    fn require_string_addresses_init_modules_by_directory() {
        let from = abs("proj/.lest");
        let to = abs("proj/luau/core/init.luau");
        assert_eq!(require_string(&from, &to).unwrap(), "../luau/core");
    }

    #[test]
    fn require_string_keeps_compound_extensions() {
        let from = abs("proj/.lest");
        let to = abs("proj/src/math.spec.luau");
        assert_eq!(require_string(&from, &to).unwrap(), "../src/math.spec");
    }
}
