use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSetBuilder};
use walkdir::{DirEntry, WalkDir};

use crate::error::ToolError;

/// Directories that never contain the user's specs. Dot-directories
/// (including `.git` and lest's own `.lest`) are skipped wholesale.
const SKIPPED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "Packages",
    "luau_packages",
    "lune_packages",
];

/// Finds every file under `root` matching the suite's include globs.
/// Patterns match the path relative to the project root, with forward
/// slashes. Results are sorted for deterministic runs.
///
/// `literal_separator` makes `*` stop at a directory boundary, so patterns mean
/// what they look like: `*.spec.luau` is the root's specs, `**/*.spec.luau` is
/// every spec at any depth. Without it globset lets a single `*` cross `/`, and
/// a suite scoped to one directory silently swallows the whole project — which
/// is how a spec needing a runtime backend ends up in a native suite.
pub fn discover(root: &Path, include: &[String]) -> Result<Vec<PathBuf>, ToolError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in include {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| ToolError(format!("invalid include glob \"{pattern}\": {e}")))?;
        builder.add(glob);
    }
    let set = builder
        .build()
        .map_err(|e| ToolError(format!("invalid include globs: {e}")))?;

    let mut specs = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(keep);
    for entry in walker {
        let entry = entry.map_err(|e| ToolError(format!("cannot scan {}: {e}", root.display())))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let rel_slash = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if set.is_match(&rel_slash) {
            // The path is kept exactly as walked, never `normalize`d. `normalize`
            // is a *cache-key* transform that case-folds on Windows/macOS, and
            // these paths are also used as file names — `snapshot_path` builds
            // `__snapshots__/<file_name>.snap` from them. Folding here renamed
            // committed snapshots per-platform. Identity comparisons take their
            // own `cache_key_path` of this value where they need one.
            specs.push(entry.path().to_path_buf());
        }
    }
    specs.sort();
    Ok(specs)
}

/// Skips hidden entries wholesale — dot-*directories* (`.git`, `.lest`) and
/// dot-*files* alike. The watcher ignores every dot component, so a hidden
/// spec that discovery accepted would run once and then never re-run on save;
/// hidden files not running at all is the consistent reading of "hidden".
fn keep(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if name.starts_with('.') {
        return false;
    }
    !entry.file_type().is_dir() || !SKIPPED_DIRS.contains(&name.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    #[test]
    fn finds_specs_and_skips_vendored_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "src/math.spec.luau");
        touch(root, "src/math.luau");
        touch(root, "Packages/dep/dep.spec.luau");
        touch(root, ".lest/harness.luau");

        let specs = discover(root, &["**/*.spec.luau".to_string()]).unwrap();
        assert_eq!(specs.len(), 1);
        assert!(specs[0].ends_with("math.spec.luau"));
    }

    /// Hidden means hidden: a dot-file spec must not be discovered, matching
    /// the watcher, which would never re-run it on save.
    #[test]
    fn dot_files_are_skipped_like_dot_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "src/math.spec.luau");
        touch(root, "src/.hidden.spec.luau");
        touch(root, ".rooted.spec.luau");

        let specs = discover(root, &["**/*.spec.luau".to_string()]).unwrap();
        assert_eq!(specs.len(), 1);
        assert!(specs[0].ends_with("math.spec.luau"));
    }

    #[test]
    fn scoped_globs_stay_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "tests/a.spec.luau");
        touch(root, "src/b.spec.luau");

        let specs = discover(root, &["tests/**/*.spec.luau".to_string()]).unwrap();
        assert_eq!(specs.len(), 1);
        assert!(specs[0].ends_with("a.spec.luau"));
    }

    /// Root-relative, forward-slashed names of what `include` matched, so the
    /// assertions below read like the globs they are checking.
    fn matched(root: &Path, include: &[&str]) -> Vec<String> {
        let include: Vec<String> = include.iter().map(|s| s.to_string()).collect();
        discover(root, &include)
            .unwrap()
            .iter()
            .map(|path| crate::backend::display_rel(path, &crate::resolve::normalize(root)))
            .collect()
    }

    #[test]
    fn a_single_star_does_not_cross_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "top.spec.luau");
        touch(root, "rt/nested.spec.luau");

        // The bug this pins: `*.spec.luau` used to match `rt/nested.spec.luau`
        // too, so a suite meant for the root swallowed every other suite's
        // specs — including ones that need a different backend.
        assert_eq!(matched(root, &["*.spec.luau"]), ["top.spec.luau"]);
    }

    #[test]
    fn double_star_spans_any_depth_including_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "top.spec.luau");
        touch(root, "src/mid.spec.luau");
        touch(root, "src/deep/low.spec.luau");

        // The default include, so `**/` has to match zero directories as well
        // as many — a root-level spec must not fall through it.
        assert_eq!(
            matched(root, &["**/*.spec.luau"]),
            [
                "src/deep/low.spec.luau",
                "src/mid.spec.luau",
                "top.spec.luau"
            ]
        );
    }

    #[test]
    fn scoped_double_star_stays_in_its_directory_at_every_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "tests/a.spec.luau");
        touch(root, "tests/deep/b.spec.luau");
        touch(root, "src/c.spec.luau");

        assert_eq!(
            matched(root, &["tests/**/*.spec.luau"]),
            ["tests/a.spec.luau", "tests/deep/b.spec.luau"]
        );
    }

    #[test]
    fn bad_glob_is_a_tool_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover(dir.path(), &["[".to_string()]).is_err());
    }
}
