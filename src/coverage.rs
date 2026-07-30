//! Assembles a [`CoverageData`] from the native backend's raw per-file line
//! hits, honoring the `[coverage] include`/`exclude` globs and excluding lest's
//! own framework.
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
    include: Option<&[String]>,
    exclude: &[String],
    instrumented: &CoverageMap,
    non_native_specs: &[PathBuf],
) -> Result<CoverageData, ToolError> {
    let filter = Filter::new(include, exclude, core_entry)?;
    let mut data = CoverageData::new();
    if !instrumented.is_empty() {
        // Recorded before filtering, so an empty table can still say whether it
        // is empty because nothing ran or because the globs took everything.
        data.mark_any_instrumented();
    }
    let mut seen: HashSet<String> = HashSet::new();

    // Deterministic order regardless of the hash map's iteration order.
    let mut files: Vec<(&PathBuf, &BTreeMap<u32, u64>)> = instrumented.iter().collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    for (path, lines) in files {
        let display = display_rel(path, root);
        if !filter.keeps(path, &display) {
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
        if !filter.keeps(&file, &display) {
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

/// Which files reach the coverage table. `include` (when configured) narrows
/// the candidates to what the project considers its own code; `exclude` then
/// removes from what is left, so `exclude` wins wherever the two overlap. Lest's
/// own framework is out either way, and is never re-admitted by an `include`
/// glob wide enough to cover it — a project's minimum is a statement about the
/// project's code, and lest's coverage of itself would silently pad it.
struct Filter {
    include: Option<GlobSet>,
    exclude: GlobSet,
    /// Already normalized: `keeps` runs once per candidate file, and folding a
    /// constant path on every call is an allocation per file for one answer.
    core_dir: Option<PathBuf>,
}

impl Filter {
    fn new(
        include: Option<&[String]>,
        exclude: &[String],
        core_entry: &Path,
    ) -> Result<Self, ToolError> {
        Ok(Self {
            include: include
                .map(|patterns| build_globset(patterns, "include"))
                .transpose()?,
            exclude: build_globset(exclude, "exclude")?,
            core_dir: core_entry.parent().map(crate::resolve::normalize),
        })
    }

    /// Whether `path` belongs in the table. `display` is its root-relative,
    /// forward-slashed spelling — the globs match that, never the absolute path,
    /// so one pattern works the same on every platform.
    fn keeps(&self, path: &Path, display: &str) -> bool {
        if let Some(core_dir) = &self.core_dir {
            // Compare folded *identities*, not spellings: coverage keys carry
            // the on-disk casing (that is what keeps the case-sensitive globs
            // and display paths honest) while `core_dir` is normalized — so a
            // literal `starts_with` would stop matching on Windows/macOS and
            // leak lest's own framework into the table.
            if crate::resolve::normalize(path).starts_with(core_dir) {
                return false;
            }
        }
        if let Some(include) = &self.include {
            if !include.is_match(display) {
                return false;
            }
        }
        !self.exclude.is_match(display)
    }
}

/// `kind` is the config key the patterns came from, so an invalid glob names
/// the key the user has to go fix.
fn build_globset(patterns: &[String], kind: &str) -> Result<GlobSet, ToolError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|e| ToolError(format!("invalid coverage {kind} glob \"{pattern}\": {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| ToolError(format!("invalid coverage {kind} globs: {e}")))
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
            None,
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
        let data = build(root, core, None, &[], &instrumented, &[]).unwrap();
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
        let data = build(
            root,
            core,
            None,
            &["Packages/**".to_string()],
            &instrumented,
            &[],
        )
        .unwrap();
        let paths: Vec<&str> = data.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/Math.luau"]);
    }

    #[test]
    fn include_narrows_the_table_to_matching_files() {
        let root = Path::new("/proj");
        let core = Path::new("/proj/luau/core/init.luau");
        let instrumented = map(&[
            ("/proj/src/math.luau", &[(1, 1)]),
            ("/proj/tests/helpers/fixture.luau", &[(1, 1)]),
            ("/proj/scripts/build.luau", &[(1, 0)]),
        ]);
        let data = build(
            root,
            core,
            Some(&["src/**".to_string()]),
            &[],
            &instrumented,
            &[],
        )
        .unwrap();
        let paths: Vec<&str> = data.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/math.luau"]);
    }

    /// `include` narrows, `exclude` then removes — so a file matching both is
    /// out. Anything else would make it impossible to include a tree and drop
    /// its generated files, which is the pairing the two keys exist for.
    #[test]
    fn exclude_wins_over_include() {
        let root = Path::new("/proj");
        let core = Path::new("/proj/luau/core/init.luau");
        let instrumented = map(&[
            ("/proj/src/math.luau", &[(1, 1)]),
            ("/proj/src/math.spec.luau", &[(1, 1)]),
            ("/proj/src/generated/api.luau", &[(1, 0)]),
        ]);
        let data = build(
            root,
            core,
            Some(&["src/**".to_string()]),
            &["**/*.spec.luau".to_string(), "src/generated/**".to_string()],
            &instrumented,
            &[],
        )
        .unwrap();
        let paths: Vec<&str> = data.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/math.luau"]);
    }

    /// The whole point of the key: no `include` means no narrowing at all, so
    /// adding the feature must not change what an existing config reports.
    #[test]
    fn no_include_reports_every_candidate() {
        let root = Path::new("/proj");
        let core = Path::new("/proj/luau/core/init.luau");
        let instrumented = map(&[
            ("/proj/src/math.luau", &[(1, 1)]),
            ("/proj/tests/helpers/fixture.luau", &[(1, 1)]),
        ]);
        let data = build(root, core, None, &[], &instrumented, &[]).unwrap();
        let paths: Vec<&str> = data.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/math.luau", "tests/helpers/fixture.luau"]);
    }

    /// An `include` wide enough to match lest's own framework must not re-admit
    /// it: the core exclusion is unconditional, not just another exclude glob.
    #[test]
    fn include_never_re_admits_lest_core() {
        let root = Path::new("/proj");
        let core = Path::new("/proj/luau/core/init.luau");
        let instrumented = map(&[
            ("/proj/src/math.luau", &[(1, 1)]),
            ("/proj/luau/core/expect.luau", &[(1, 5)]),
        ]);
        let data = build(
            root,
            core,
            Some(&["**".to_string()]),
            &[],
            &instrumented,
            &[],
        )
        .unwrap();
        let paths: Vec<&str> = data.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/math.luau"]);
    }

    /// `include` has to narrow the not-instrumented rows too, not just the
    /// instrumented ones. Those come from a different loop over a real require
    /// closure, so only a test with files on disk covers it: without the filter
    /// there, `include = ["src/**"]` still lists a lune suite's helpers as `—`.
    #[test]
    fn include_narrows_not_instrumented_files_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/math.luau"), "return {}\n").unwrap();
        std::fs::write(root.join("tests/helper.luau"), "return {}\n").unwrap();
        std::fs::write(
            root.join("tests/thing.spec.luau"),
            "local m = require('../src/math')\nlocal h = require('./helper')\nreturn nil\n",
        )
        .unwrap();

        let core = root.join("luau/core/init.luau");
        let spec = root.join("tests/thing.spec.luau");

        // Unfiltered, the whole closure is listed as not-instrumented — the
        // spec included, since the closure starts from it (a real run's
        // default `exclude` is what drops specs).
        let all = build(
            root,
            &core,
            None,
            &[],
            &CoverageMap::new(),
            std::slice::from_ref(&spec),
        )
        .unwrap();
        let mut paths: Vec<&str> = all.files().iter().map(|f| f.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec![
                "src/math.luau",
                "tests/helper.luau",
                "tests/thing.spec.luau"
            ]
        );
        assert!(all.files().iter().all(|f| !f.is_instrumented()));

        // With `include`, only the one under `src/` survives.
        let narrowed = build(
            root,
            &core,
            Some(&["src/**".to_string()]),
            &[],
            &CoverageMap::new(),
            &[spec],
        )
        .unwrap();
        let paths: Vec<&str> = narrowed.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/math.luau"]);
    }

    /// An empty table has two very different causes, and the flag is what lets
    /// the CLI tell a user which one they hit.
    #[test]
    fn any_instrumented_records_pre_filter_state() {
        let root = Path::new("/proj");
        let core = Path::new("/proj/luau/core/init.luau");
        let instrumented = map(&[("/proj/src/math.luau", &[(1, 1)])]);

        // Everything filtered out: the table is empty, but files were measured.
        let filtered = build(
            root,
            core,
            Some(&["nothing/**".to_string()]),
            &[],
            &instrumented,
            &[],
        )
        .unwrap();
        assert!(filtered.files().is_empty());
        assert!(filtered.any_instrumented());

        // Nothing measured at all.
        let nothing = build(root, core, None, &[], &CoverageMap::new(), &[]).unwrap();
        assert!(!nothing.any_instrumented());
    }

    /// A bad `include` glob has to name `include`, not `exclude` — the two
    /// share a builder and an error that names the wrong key sends the user to
    /// the wrong line of their config.
    #[test]
    fn invalid_include_glob_names_the_include_key() {
        let root = Path::new("/proj");
        let core = Path::new("/proj/luau/core/init.luau");
        let err = build(
            root,
            core,
            Some(&["src/[".to_string()]),
            &[],
            &CoverageMap::new(),
            &[],
        )
        .unwrap_err();
        assert!(err.0.contains("include"), "{}", err.0);
        assert!(!err.0.contains("exclude"), "{}", err.0);
    }
}
