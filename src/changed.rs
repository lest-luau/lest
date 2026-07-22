//! `--changed <git-ref>` selection: run only the specs affected by files that
//! changed since a git ref.
//!
//! `git diff --name-only <ref>` plus `git ls-files --others` gives the changed
//! paths (relative to the repo top-level); those are canonicalized and fed
//! through the require [`DependencyGraph`] so every spec transitively depending
//! on a changed file re-runs. The graph work is shared with watch mode.
//!
//! Nothing guarantees the project lest is pointed at is a git repository — or
//! that git is installed at all — so every invocation is guarded into a clear
//! tool error naming the flag; the selection logic itself is pure and
//! unit-tested.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::resolve::{cache_key_path, DependencyGraph};

use crate::error::ToolError;

/// The set of changed files since `git_ref`, as canonical cache-key paths.
pub fn changed_paths(root: &Path, git_ref: &str) -> Result<HashSet<PathBuf>, ToolError> {
    let toplevel = git_toplevel(root)?;
    let tracked = run_git(root, &["diff", "--name-only", git_ref])?;
    // `git diff` reports *tracked* modifications only, so a brand-new spec file
    // is invisible to it — and a spec that never ran cannot fail, which is how
    // `--changed` in CI goes green over the very test the change added.
    // `ls-files --others` fills that gap; `--exclude-standard` keeps gitignored
    // build output out, and `--full-name` reports relative to the repo
    // top-level like `diff` does rather than to the working directory.
    let untracked = run_git(
        root,
        &["ls-files", "--others", "--exclude-standard", "--full-name"],
    )?;
    let mut changed = parse_changed(&toplevel, &tracked);
    changed.extend(parse_changed(&toplevel, &untracked));
    Ok(changed)
}

/// Every spec (from `specs`) transitively affected by `changed`, as canonical
/// cache-key paths — the same paths [`DependencyGraph`] produces, so a caller
/// filtering discovered specs must match via [`cache_key_path`].
pub fn affected_specs<I, P>(root: &Path, specs: I, changed: &HashSet<PathBuf>) -> HashSet<PathBuf>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let graph = DependencyGraph::build(root, specs);
    graph.affected_specs(changed)
}

/// Joins each non-empty diff line to the repo top-level and canonicalizes it.
/// Pure so the path-identity handling is testable without a live repo.
fn parse_changed(toplevel: &Path, output: &str) -> HashSet<PathBuf> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| cache_key_path(&toplevel.join(line)))
        .collect()
}

fn git_toplevel(root: &Path) -> Result<PathBuf, ToolError> {
    let out = run_git(root, &["rev-parse", "--show-toplevel"])?;
    let line = out.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return Err(ToolError(
            "`--changed` needs a git repository, but git reported no top-level directory"
                .to_string(),
        ));
    }
    Ok(PathBuf::from(line))
}

fn run_git(root: &Path, args: &[&str]) -> Result<String, ToolError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        // git's default `core.quotePath` C-quotes any path byte outside ASCII,
        // so `src/café.luau` comes back as the literal `"src/caf\303\251.luau"`
        // — quotes, backslashes and all — and joining that produces a path that
        // exists nowhere. Turned off per-invocation rather than relying on the
        // user's config, which lest does not own.
        .arg("-c")
        .arg("core.quotePath=false")
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError(
                    "`--changed` needs git on PATH, but git was not found — install git or drop \
                     the flag"
                        .to_string(),
                )
            } else {
                ToolError(format!("cannot run git: {e}"))
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(ToolError(format!(
            "`git {}` failed{} — `--changed` needs a git repository with a valid ref",
            args.join(" "),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_changed_joins_and_canonicalizes() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        fs::create_dir_all(top.join("src")).unwrap();
        let file = top.join("src/math.luau");
        fs::write(&file, "return 1").unwrap();

        let changed = parse_changed(top, "src/math.luau\n\n  \n");
        assert_eq!(changed.len(), 1);
        // Matches the identity a DependencyGraph node would carry.
        assert!(changed.contains(&cache_key_path(&file)));
    }

    /// With `core.quotePath=false` git emits raw UTF-8, so a non-ASCII path
    /// arrives as itself. The bug this pins is the other spelling: the C-quoted
    /// `"src/caf\303\251.luau"` was joined literally, quotes included, and
    /// matched nothing.
    #[test]
    fn parse_changed_keeps_non_ascii_paths_intact() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        fs::create_dir_all(top.join("src")).unwrap();
        let file = top.join("src/café.luau");
        fs::write(&file, "return 1").unwrap();

        let changed = parse_changed(top, "src/café.luau\n");
        assert_eq!(changed.len(), 1);
        assert!(changed.contains(&cache_key_path(&file)));
    }

    #[test]
    fn affected_specs_follows_require_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let module = root.join("src/math.luau");
        let spec = root.join("src/math.spec.luau");
        fs::write(&module, "return { add = function(a, b) return a + b end }").unwrap();
        fs::write(&spec, "local m = require('./math')\nreturn nil").unwrap();

        let mut changed = HashSet::new();
        changed.insert(cache_key_path(&module));
        let affected = affected_specs(root, [spec.clone()], &changed);
        assert!(affected.contains(&cache_key_path(&spec)));

        // A change to an unrelated file affects nothing.
        let unrelated = root.join("src/other.luau");
        fs::write(&unrelated, "return 0").unwrap();
        let mut changed = HashSet::new();
        changed.insert(cache_key_path(&unrelated));
        assert!(affected_specs(root, [spec], &changed).is_empty());
    }
}
