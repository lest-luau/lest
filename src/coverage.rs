//! Assembles a [`CoverageData`] from the native backend's raw per-file line
//! hits, honoring `[coverage] exclude` globs and excluding lest's own framework.
//!
//! Coverage is native-only by design. Files that ran under a spawned runtime
//! cannot be instrumented, so rather than counting them as `0%` they are listed
//! as [`FileCoverage::not_instrumented`] — enumerated from each non-native
//! suite's require closure — so the numbers stay honest.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::report::{CoverageData, FileCoverage};
use crate::resolve::dependency_closure_all;
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::backend::display_rel;
use crate::backend::native::CoverageMap;
use crate::error::ToolError;

/// Builds the run's coverage table. `instrumented` is the native backend's
/// aggregated line hits; `non_native_specs` are the spec files of any suites
/// that ran under a spawned runtime, whose source files are listed as
/// not-instrumented.
pub fn build(
    root: &Path,
    core_entry: &Path,
    exclude: &[String],
    instrumented: &CoverageMap,
    non_native_specs: &[PathBuf],
) -> Result<CoverageData, ToolError> {
    let set = build_globset(exclude)?;
    let core_dir = core_entry.parent().map(Path::to_path_buf);
    let mut data = CoverageData::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Deterministic order regardless of the hash map's iteration order.
    let mut files: Vec<(&PathBuf, &BTreeMap<u32, u64>)> = instrumented.iter().collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    for (path, lines) in files {
        let display = display_rel(path, root);
        if is_excluded(path, &display, &set, core_dir.as_deref()) {
            continue;
        }
        if seen.insert(display.clone()) {
            data.add(FileCoverage::instrumented(display, lines.clone()));
        }
    }

    // Honest labelling for code a spawned runtime executed.
    // One walk over every non-native spec, not one closure per spec: shared
    // dependencies (a suite's helpers, a common module) would otherwise be read
    // and scanned once per spec that requires them.
    let mut not_instrumented: Vec<String> = Vec::new();
    for file in dependency_closure_all(non_native_specs) {
        let display = display_rel(&file, root);
        if is_excluded(&file, &display, &set, core_dir.as_deref()) {
            continue;
        }
        // `insert` already reports whether the key was new — no prior lookup.
        if seen.insert(display.clone()) {
            not_instrumented.push(display);
        }
    }
    not_instrumented.sort();
    for display in not_instrumented {
        data.add(FileCoverage::not_instrumented(display));
    }

    Ok(data)
}

fn is_excluded(path: &Path, display: &str, set: &GlobSet, core_dir: Option<&Path>) -> bool {
    if let Some(core_dir) = core_dir {
        // Compare folded *identities*, not spellings: coverage keys carry the
        // on-disk casing (that is what keeps the case-sensitive exclude globs
        // and display paths honest) while `core_entry` arrives normalized —
        // so a literal `starts_with` would stop matching on Windows/macOS and
        // leak lest's own framework into the table.
        if crate::resolve::normalize(path).starts_with(crate::resolve::normalize(core_dir)) {
            return true;
        }
    }
    set.is_match(display)
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, ToolError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|e| ToolError(format!("invalid coverage exclude glob \"{pattern}\": {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| ToolError(format!("invalid coverage exclude globs: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &[(u32, u64)])]) -> CoverageMap {
        entries
            .iter()
            .map(|(path, lines)| {
                (
                    PathBuf::from(path),
                    lines.iter().copied().collect::<BTreeMap<u32, u64>>(),
                )
            })
            .collect()
    }

    #[test]
    fn excludes_spec_files_and_core() {
        let root = Path::new("/proj");
        let core = Path::new("/proj/luau/core/init.luau");
        let instrumented = map(&[
            ("/proj/src/math.luau", &[(1, 3), (2, 0)]),
            ("/proj/src/math.spec.luau", &[(1, 1)]),
            ("/proj/luau/core/expect.luau", &[(1, 5)]),
        ]);
        let data = build(
            root,
            core,
            &["**/*.spec.luau".to_string()],
            &instrumented,
            &[],
        )
        .unwrap();
        let paths: Vec<&str> = data.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/math.luau"]);
        assert_eq!(data.overall_percent(), Some(50.0));
    }

    /// Coverage map keys now carry the on-disk spelling while `core_entry` is
    /// normalized (folded on case-insensitive hosts); the core exclusion must
    /// compare folded identities or core's own files leak into the table.
    /// Windows-only because the fixture uses drive-letter paths, which are a
    /// single opaque component on Unix.
    #[cfg(windows)]
    #[test]
    fn core_exclusion_survives_case_differences() {
        let root = Path::new("C:\\Proj");
        // Folded, as `plan.core_entry` genuinely arrives.
        let core = Path::new("c:\\proj\\luau\\core\\init.luau");
        let instrumented = map(&[
            ("C:\\Proj\\src\\Math.luau", &[(1, 1)]),
            // On-disk spelling, as the native loader now attributes it.
            ("C:\\Proj\\Luau\\Core\\expect.luau", &[(1, 5)]),
        ]);
        let data = build(root, core, &[], &instrumented, &[]).unwrap();
        let paths: Vec<&str> = data.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/Math.luau"]);
    }

    #[test]
    fn case_sensitive_exclude_globs_match_the_display_path() {
        // The shipped default excludes `Packages/**`. Globs are case-sensitive,
        // so this only works while `display_rel` emits the *original* casing —
        // a case-folded display path silently vendored every dependency into
        // the coverage table. Lowercase-only fixtures cannot catch that.
        let root = Path::new("/proj");
        let core = Path::new("/proj/luau/core/init.luau");
        let instrumented = map(&[
            ("/proj/src/Math.luau", &[(1, 3), (2, 1)]),
            ("/proj/Packages/dep/mod.luau", &[(1, 0)]),
        ]);
        let data = build(root, core, &["Packages/**".to_string()], &instrumented, &[]).unwrap();
        let paths: Vec<&str> = data.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/Math.luau"]);
    }
}
