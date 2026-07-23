use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::report::Event;

pub mod cloud;
pub mod native;
pub mod runtime;

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
    /// Absolute path to the rojo project file (`settings.rojo`), when set.
    /// Only the cloud backend consults it: string requires whose targets it
    /// maps into the place delegate to the engine's `require` instead of
    /// bundling a private copy.
    pub rojo_project: Option<PathBuf>,
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
