//! Line-coverage data and its two renderings: a terminal table and lcov.
//!
//! Coverage is a native-backend-only feature by design — only the embedded VM
//! exposes the hooks. The CLI's native backend *produces* a [`CoverageData`]
//! by walking the Luau VM coverage API after a run; this module *renders* it.
//! Non-native suites are labelled "not instrumented" ([`FileCoverage::not_instrumented`])
//! and are never counted as `0%`, so the numbers stay honest.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::pretty::{paint, BOLD, DIM, GREEN, RED, YELLOW};

/// Per-file line coverage. Files may be instrumented (with a line-hit map) or
/// explicitly [`not_instrumented`](FileCoverage::not_instrumented) — the latter
/// renders as `—`, never `0%`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCoverage {
    /// The file this record describes, as the CLI wishes to display it.
    pub path: String,
    /// Line number → hit count for every instrumentable line, or `None` when
    /// the file was not run under an instrumented (native) backend. A hit count
    /// of `0` means the line was instrumented but never executed.
    pub lines: Option<BTreeMap<u32, u64>>,
}

impl FileCoverage {
    /// A file whose lines were tracked. `lines` maps each instrumentable line
    /// number to how many times it ran (`0` = missed).
    pub fn instrumented(path: impl Into<String>, lines: BTreeMap<u32, u64>) -> Self {
        Self {
            path: path.into(),
            lines: Some(lines),
        }
    }

    /// A file that ran under a non-native backend, so no coverage exists. It is
    /// reported as "not instrumented" rather than counted as zero.
    pub fn not_instrumented(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            lines: None,
        }
    }

    /// Whether this file carries coverage data.
    pub fn is_instrumented(&self) -> bool {
        self.lines.is_some()
    }

    /// Number of instrumentable lines, or `0` when not instrumented.
    pub fn total_lines(&self) -> u32 {
        self.lines.as_ref().map_or(0, |l| l.len() as u32)
    }

    /// Number of lines that ran at least once, or `0` when not instrumented.
    pub fn covered_lines(&self) -> u32 {
        self.lines
            .as_ref()
            .map_or(0, |l| l.values().filter(|&&h| h > 0).count() as u32)
    }

    /// Percentage of instrumentable lines covered, or `None` when not
    /// instrumented (a file with zero instrumentable lines is `100%`).
    pub fn percent(&self) -> Option<f64> {
        let lines = self.lines.as_ref()?;
        if lines.is_empty() {
            return Some(100.0);
        }
        Some(self.covered_lines() as f64 / lines.len() as f64 * 100.0)
    }
}

/// A whole run's line coverage: one [`FileCoverage`] per file, in insertion
/// order. Built by the native backend and handed to a renderer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageData {
    files: Vec<FileCoverage>,
}

impl CoverageData {
    /// An empty coverage set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a file's coverage. Chainable.
    pub fn add(&mut self, file: FileCoverage) -> &mut Self {
        self.files.push(file);
        self
    }

    /// The recorded files, in insertion order.
    pub fn files(&self) -> &[FileCoverage] {
        &self.files
    }

    /// `(covered, total)` instrumentable lines across every instrumented file.
    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .filter(|f| f.is_instrumented())
            .fold((0, 0), |(c, t), f| {
                (c + f.covered_lines(), t + f.total_lines())
            })
    }

    /// Overall covered percentage across instrumented files, or `None` when
    /// nothing was instrumented. Used to gate CI against `--min`.
    pub fn overall_percent(&self) -> Option<f64> {
        let (covered, total) = self.totals();
        if self.files.iter().all(|f| !f.is_instrumented()) {
            return None;
        }
        if total == 0 {
            return Some(100.0);
        }
        Some(covered as f64 / total as f64 * 100.0)
    }

    /// Renders the terminal coverage table: a box-drawn grid with one row per
    /// file giving covered/total lines and a percentage (`not instrumented`
    /// files show `—`), ruled off from a totals row. Percentages are colored by
    /// threshold when `color` is on.
    pub fn table(&self, color: bool) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{}", paint(color, BOLD, "Coverage:"));

        if self.files.is_empty() {
            // A note, so it wears the note voice: dim, lowercase, no period.
            let note = paint(color, DIM, "  no files were instrumented");
            let _ = writeln!(out, "{note}");
            return out;
        }

        let (covered, total) = self.totals();
        let overall = self.overall_percent();
        let ratio = |c: u32, t: u32| format!("{c}/{t}");
        let lines_of = |f: &FileCoverage| {
            if f.is_instrumented() {
                ratio(f.covered_lines(), f.total_lines())
            } else {
                "—".to_string()
            }
        };

        // Widths are measured in chars so the em dash and any non-ASCII path
        // align the way `{:width$}` (which counts chars) pads them.
        let name_width = self
            .files
            .iter()
            .map(|f| f.path.chars().count())
            .chain(["File".len(), "All files".len()])
            .max()
            .unwrap_or(9);
        let lines_width = self
            .files
            .iter()
            .map(|f| lines_of(f).chars().count())
            .chain(["Lines".len(), ratio(covered, total).chars().count()])
            .max()
            .unwrap_or(5);
        let pct_width = self
            .files
            .iter()
            .map(|f| format_percent(f.percent()).chars().count())
            .chain(["Covered".len(), format_percent(overall).chars().count()])
            .max()
            .unwrap_or(7);

        // Every cell is padded by one space on each side, so a rule segment is
        // the column width plus two.
        let rule = |left: &str, mid: &str, right: &str| {
            let seg = |w: usize| "─".repeat(w + 2);
            format!(
                "{left}{}{mid}{}{mid}{}{right}",
                seg(name_width),
                seg(lines_width),
                seg(pct_width)
            )
        };
        let row = |name: &str, lines: &str, pct: &str| format!("│ {name} │ {lines} │ {pct} │");

        let _ = writeln!(out, "{}", rule("┌", "┬", "┐"));
        let _ = writeln!(
            out,
            "{}",
            // Padded first, then bolded — the escape must not count toward the
            // cell width or the column rules stop lining up.
            row(
                &paint(color, BOLD, &format!("{:<name_width$}", "File")),
                &paint(color, BOLD, &format!("{:<lines_width$}", "Lines")),
                &paint(color, BOLD, &format!("{:<pct_width$}", "Covered")),
            )
        );
        let _ = writeln!(out, "{}", rule("├", "┼", "┤"));

        for file in &self.files {
            let name = format!("{:<name_width$}", file.path);
            let lines_cell = format!("{:>lines_width$}", lines_of(file));
            let pct = file.percent();
            // Pad the plain cell first, then color it — coloring a padded cell
            // keeps columns aligned (ANSI codes would otherwise count as width).
            let pct_cell = format!("{:>pct_width$}", format_percent(pct));
            if file.is_instrumented() {
                let painted = colorize_percent(color, pct, &pct_cell);
                let _ = writeln!(out, "{}", row(&name, &lines_cell, &painted));
            } else {
                // No data → the row's contents are dim, but its borders stay the
                // same weight as the rest of the grid.
                let _ = writeln!(
                    out,
                    "{}",
                    row(
                        &paint(color, DIM, &name),
                        &paint(color, DIM, &lines_cell),
                        &paint(color, DIM, &pct_cell),
                    )
                );
            }
        }

        let _ = writeln!(out, "{}", rule("├", "┼", "┤"));
        let name = format!("{:<name_width$}", "All files");
        let lines_cell = format!("{:>lines_width$}", ratio(covered, total));
        let pct_cell = format!("{:>pct_width$}", format_percent(overall));
        let painted = colorize_percent(color, overall, &pct_cell);
        let _ = writeln!(out, "{}", row(&name, &lines_cell, &painted));
        let _ = writeln!(out, "{}", rule("└", "┴", "┘"));
        out
    }

    /// Serializes to [lcov tracefile] format for Codecov and editor gutters.
    /// Only instrumented files emit records; non-instrumented files are absent
    /// (an lcov consumer treats "absent" and "not instrumented" the same way).
    ///
    /// [lcov tracefile]: https://ltp.sourceforge.net/coverage/lcov/geninfo.1.php
    pub fn to_lcov(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            let Some(lines) = &file.lines else { continue };
            out.push_str("TN:\n");
            let _ = writeln!(out, "SF:{}", file.path);
            for (line, hits) in lines {
                let _ = writeln!(out, "DA:{line},{hits}");
            }
            let _ = writeln!(out, "LF:{}", file.total_lines());
            let _ = writeln!(out, "LH:{}", file.covered_lines());
            out.push_str("end_of_record\n");
        }
        out
    }
}

/// `100%` / `62.5%` / `—` for the not-instrumented case.
fn format_percent(pct: Option<f64>) -> String {
    match pct {
        Some(p) => format!("{p:.1}%"),
        None => "—".to_string(),
    }
}

/// Green ≥ 80%, yellow ≥ 50%, red below; not-instrumented cells stay dim.
fn colorize_percent(color: bool, pct: Option<f64>, cell: &str) -> String {
    match pct {
        None => paint(color, DIM, cell),
        Some(p) if p >= 80.0 => paint(color, GREEN, cell),
        Some(p) if p >= 50.0 => paint(color, YELLOW, cell),
        Some(_) => paint(color, RED, cell),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(pairs: &[(u32, u64)]) -> BTreeMap<u32, u64> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn percent_and_totals_ignore_not_instrumented() {
        let mut cov = CoverageData::new();
        cov.add(FileCoverage::instrumented(
            "a.luau",
            lines(&[(1, 2), (2, 0), (3, 1), (4, 0)]),
        ));
        cov.add(FileCoverage::not_instrumented("b.luau"));

        assert_eq!(cov.totals(), (2, 4));
        assert_eq!(cov.overall_percent(), Some(50.0));

        let a = &cov.files()[0];
        assert_eq!(a.covered_lines(), 2);
        assert_eq!(a.total_lines(), 4);
        assert_eq!(a.percent(), Some(50.0));

        let b = &cov.files()[1];
        assert!(!b.is_instrumented());
        assert_eq!(b.percent(), None);
    }

    #[test]
    fn overall_percent_is_none_when_nothing_instrumented() {
        let mut cov = CoverageData::new();
        cov.add(FileCoverage::not_instrumented("only.luau"));
        assert_eq!(cov.overall_percent(), None);
    }

    #[test]
    fn table_labels_not_instrumented_and_totals() {
        let mut cov = CoverageData::new();
        cov.add(FileCoverage::instrumented(
            "a.luau",
            lines(&[(1, 1), (2, 0)]),
        ));
        cov.add(FileCoverage::not_instrumented("b.luau"));
        let table = cov.table(false);

        assert!(table.contains("File"));
        assert!(table.contains("a.luau"));
        assert!(table.contains("50.0%"));
        assert!(table.contains("b.luau"));
        assert!(table.contains("—")); // not instrumented cell
        assert!(table.contains("All files"));
    }

    #[test]
    fn lcov_emits_only_instrumented_files() {
        let mut cov = CoverageData::new();
        cov.add(FileCoverage::instrumented(
            "src/a.luau",
            lines(&[(1, 3), (2, 0)]),
        ));
        cov.add(FileCoverage::not_instrumented("src/b.luau"));
        let lcov = cov.to_lcov();

        assert!(lcov.contains("SF:src/a.luau"));
        assert!(lcov.contains("DA:1,3"));
        assert!(lcov.contains("DA:2,0"));
        assert!(lcov.contains("LF:2"));
        assert!(lcov.contains("LH:1"));
        assert!(lcov.contains("end_of_record"));
        assert!(!lcov.contains("src/b.luau"));
    }

    #[test]
    fn empty_instrumented_file_is_full_coverage() {
        let file = FileCoverage::instrumented("empty.luau", BTreeMap::new());
        assert_eq!(file.percent(), Some(100.0));
    }
}
