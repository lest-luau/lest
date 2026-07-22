//! CLI-side snapshot storage and comparison.
//!
//! The framework only reports the *fact* of a snapshot (`Event::Snapshot` with
//! a key and the received value); the host decides pass/fail. That decision
//! lives here: for each snapshot the store loads the spec file's
//! `__snapshots__/<file>.snap`, compares against the stored value, and records
//! the outcome into a [`SnapshotSummary`]. Absent key → write and pass; equal →
//! pass; different → fail (with a rendered diff) unless `--update` overwrites.
//! Keys stored but never produced this run are reported obsolete at the end —
//! but only on an unfiltered run, since under `-t` or a watch-mode spec
//! restriction "not produced" means "did not run", not "no longer wanted".
//!
//! The on-disk format, the summary type, and the diff renderer all come from
//! `lest-report` so the format lives in exactly one place; this module owns
//! only the file IO and the per-run bookkeeping.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::report::{diff, parse_snapshots, serialize_snapshots, SnapshotFile, SnapshotSummary};

use crate::error::ToolError;

/// One snapshot failure, kept in parts rather than pre-rendered: the JUnit
/// reporter needs a real `<testcase>` with a `<failure>` element (a prose note
/// is invisible to CI annotation), so it needs the spec, the key, and the body
/// separately. [`Display`](fmt::Display) reproduces the terminal block.
pub struct SnapshotFailure {
    /// The spec file the snapshot belongs to, as displayed.
    pub spec: String,
    /// The snapshot's storage key.
    pub key: String,
    /// The rendered diff, or an explanation for a non-diff failure.
    pub detail: String,
}

impl fmt::Display for SnapshotFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "snapshot \"{}\" in {}:\n{}",
            self.key, self.spec, self.detail
        )
    }
}

/// Accumulates snapshot outcomes across a run and defers file writes to the
/// end, so a spec file's `.snap` is rewritten at most once.
pub struct SnapshotStore {
    update: bool,
    color: bool,
    /// Whether this run executed only a subset of the tests (`-t`, or a
    /// watch/`--changed` spec restriction). Obsolete detection compares "keys
    /// on disk" against "keys produced this run", which is only sound over a
    /// complete run — see [`SnapshotStore::new`].
    filtered: bool,
    files: HashMap<PathBuf, SnapState>,
    pub summary: SnapshotSummary,
    /// Failures for the reporter to surface.
    pub failures: Vec<SnapshotFailure>,
}

struct SnapState {
    /// The spec file these snapshots belong to (for failure headers).
    spec_display: String,
    /// Keys as they exist on disk at run start.
    stored: SnapshotFile,
    /// Working copy to be written back (starts as a clone of `stored`).
    working: SnapshotFile,
    /// Keys produced this run, for obsolete detection.
    seen: BTreeSet<String>,
    /// Whether `working` diverged from disk and must be rewritten.
    dirty: bool,
}

impl SnapshotStore {
    /// `filtered` must be true whenever the run does **not** execute every test
    /// in every discovered spec — a `-t` name filter, or a watch / `--changed`
    /// spec restriction. A key on disk that no test produced is only "obsolete"
    /// if every test had a chance to produce it; under a filter the untouched
    /// keys are simply the tests that did not run. Without this flag,
    /// `lest -u -t "adds"` on a five-snapshot spec would write one key and
    /// **delete the other four** — permanently, since `write` also removes a
    /// file whose keys were all pruned.
    pub fn new(update: bool, color: bool, filtered: bool) -> Self {
        Self {
            update,
            color,
            filtered,
            files: HashMap::new(),
            summary: SnapshotSummary::default(),
            failures: Vec::new(),
        }
    }

    /// Records one snapshot: the spec file it came from, its storage key, and
    /// the received value. Loads the spec's `.snap` on first sight.
    pub fn record(
        &mut self,
        spec: &Path,
        spec_display: &str,
        key: &str,
        received: &str,
    ) -> Result<(), ToolError> {
        let snap_path = snapshot_path(spec);
        if !self.files.contains_key(&snap_path) {
            let stored = load(&snap_path)?;
            self.files.insert(
                snap_path.clone(),
                SnapState {
                    spec_display: spec_display.to_string(),
                    working: stored.clone(),
                    stored,
                    seen: BTreeSet::new(),
                    dirty: false,
                },
            );
        }
        let state = self.files.get_mut(&snap_path).unwrap();

        // Keys are `<describe path> > <test name> <hint-or-counter>`, so two
        // same-named tests in one `describe` collide. Comparing only against
        // `stored` would let both "pass" while the second silently overwrote
        // the first — neither test actually pinned to anything. `insert`
        // returning false is the only signal that happened.
        if !state.seen.insert(key.to_string()) {
            self.summary.failed += 1;
            self.failures.push(SnapshotFailure {
                spec: state.spec_display.clone(),
                key: key.to_string(),
                detail: "duplicate snapshot key: two tests produced it, so neither is pinned — \
                         rename one of the tests or give it a distinct snapshot hint"
                    .to_string(),
            });
            return Ok(());
        }

        match state.stored.get(key) {
            None => {
                // First time we have seen this key: write and pass.
                state.working.insert(key.to_string(), received.to_string());
                state.dirty = true;
                self.summary.written += 1;
            }
            Some(existing) if existing == received => {
                self.summary.matched += 1;
            }
            Some(existing) => {
                if self.update {
                    state.working.insert(key.to_string(), received.to_string());
                    state.dirty = true;
                    self.summary.updated += 1;
                } else {
                    self.summary.failed += 1;
                    self.failures.push(SnapshotFailure {
                        spec: state.spec_display.clone(),
                        key: key.to_string(),
                        detail: diff(existing, received, self.color),
                    });
                }
            }
        }
        Ok(())
    }

    /// Finalizes the run: counts obsolete keys, prunes them under `--update`,
    /// and writes back every spec file whose snapshots changed. Returns the
    /// completed summary.
    ///
    /// Obsolete detection is skipped entirely on a filtered run — neither
    /// counted nor pruned. "Stored but not produced" only means "obsolete" when
    /// every test ran; under `-t` or a watch-mode spec restriction it means
    /// "that test did not run this time", and pruning it under `-u` would
    /// delete live snapshots irrecoverably.
    pub fn finish(&mut self) -> Result<SnapshotSummary, ToolError> {
        for (path, state) in self.files.iter_mut() {
            if !self.filtered {
                let obsolete: Vec<String> = state
                    .stored
                    .keys()
                    .filter(|k| !state.seen.contains(*k))
                    .cloned()
                    .collect();
                self.summary.obsolete += obsolete.len() as u32;
                if self.update {
                    for key in &obsolete {
                        state.working.remove(key);
                        state.dirty = true;
                    }
                }
            }
            if state.dirty {
                write(path, &state.working)?;
            }
        }
        Ok(self.summary)
    }
}

/// `<spec-dir>/__snapshots__/<spec-file-name>.snap`.
fn snapshot_path(spec: &Path) -> PathBuf {
    let dir = spec.parent().unwrap_or_else(|| Path::new("."));
    let name = spec
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    dir.join("__snapshots__").join(format!("{name}.snap"))
}

fn load(path: &Path) -> Result<SnapshotFile, ToolError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_snapshots(&text).map_err(|e| {
            ToolError(format!(
                "cannot read snapshots from {}: {e}",
                path.display()
            ))
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(SnapshotFile::new()),
        Err(err) => Err(ToolError(format!("cannot read {}: {err}", path.display()))),
    }
}

fn write(path: &Path, snapshots: &SnapshotFile) -> Result<(), ToolError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| ToolError(format!("cannot create {}: {e}", dir.display())))?;
    }
    // An emptied file (all keys pruned) is removed rather than left as a stub.
    if snapshots.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(ToolError(format!(
                    "cannot remove {}: {err}",
                    path.display()
                )))
            }
        }
        return Ok(());
    }
    std::fs::write(path, serialize_snapshots(snapshots))
        .map_err(|e| ToolError(format!("cannot write {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn spec_in(dir: &Path) -> PathBuf {
        let path = dir.join("math.spec.luau");
        fs::write(&path, "return nil").unwrap();
        path
    }

    #[test]
    fn absent_key_is_written_and_passes() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path());
        let mut store = SnapshotStore::new(false, false, false);
        store.record(&spec, "math.spec.luau", "adds", "3").unwrap();
        let summary = store.finish().unwrap();
        assert_eq!(summary.written, 1);
        assert_eq!(summary.failed, 0);
        let snap = snapshot_path(&spec);
        assert!(snap.is_file());
        let parsed = parse_snapshots(&fs::read_to_string(&snap).unwrap()).unwrap();
        assert_eq!(parsed.get("adds").map(String::as_str), Some("3"));
    }

    #[test]
    fn equal_matches_and_leaves_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path());
        let snap = snapshot_path(&spec);
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        let mut initial = SnapshotFile::new();
        initial.insert("adds".into(), "3".into());
        fs::write(&snap, serialize_snapshots(&initial)).unwrap();
        let before = fs::metadata(&snap).unwrap().modified().unwrap();

        let mut store = SnapshotStore::new(false, false, false);
        store.record(&spec, "math.spec.luau", "adds", "3").unwrap();
        let summary = store.finish().unwrap();
        assert_eq!(summary.matched, 1);
        // Untouched because nothing changed.
        assert_eq!(fs::metadata(&snap).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn different_fails_without_update() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path());
        let snap = snapshot_path(&spec);
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        let mut initial = SnapshotFile::new();
        initial.insert("adds".into(), "3".into());
        fs::write(&snap, serialize_snapshots(&initial)).unwrap();

        let mut store = SnapshotStore::new(false, false, false);
        store.record(&spec, "math.spec.luau", "adds", "4").unwrap();
        let summary = store.finish().unwrap();
        assert_eq!(summary.failed, 1);
        assert_eq!(store.failures.len(), 1);
        // The stored value is preserved on failure.
        let parsed = parse_snapshots(&fs::read_to_string(&snap).unwrap()).unwrap();
        assert_eq!(parsed.get("adds").map(String::as_str), Some("3"));
    }

    #[test]
    fn different_overwrites_under_update() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path());
        let snap = snapshot_path(&spec);
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        let mut initial = SnapshotFile::new();
        initial.insert("adds".into(), "3".into());
        fs::write(&snap, serialize_snapshots(&initial)).unwrap();

        let mut store = SnapshotStore::new(true, false, false);
        store.record(&spec, "math.spec.luau", "adds", "4").unwrap();
        let summary = store.finish().unwrap();
        assert_eq!(summary.updated, 1);
        let parsed = parse_snapshots(&fs::read_to_string(&snap).unwrap()).unwrap();
        assert_eq!(parsed.get("adds").map(String::as_str), Some("4"));
    }

    #[test]
    fn obsolete_keys_are_counted_and_pruned_on_update() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path());
        let snap = snapshot_path(&spec);
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        let mut initial = SnapshotFile::new();
        initial.insert("adds".into(), "3".into());
        initial.insert("gone".into(), "9".into());
        fs::write(&snap, serialize_snapshots(&initial)).unwrap();

        // Without update: reported, not pruned.
        let mut store = SnapshotStore::new(false, false, false);
        store.record(&spec, "math.spec.luau", "adds", "3").unwrap();
        let summary = store.finish().unwrap();
        assert_eq!(summary.obsolete, 1);
        let parsed = parse_snapshots(&fs::read_to_string(&snap).unwrap()).unwrap();
        assert!(parsed.contains_key("gone"));

        // With update: pruned.
        let mut store = SnapshotStore::new(true, false, false);
        store.record(&spec, "math.spec.luau", "adds", "3").unwrap();
        let summary = store.finish().unwrap();
        assert_eq!(summary.obsolete, 1);
        let parsed = parse_snapshots(&fs::read_to_string(&snap).unwrap()).unwrap();
        assert!(!parsed.contains_key("gone"));
    }

    #[test]
    fn filtered_update_never_prunes_unseen_keys() {
        // `lest -u -t "adds"` must not delete the four snapshots whose tests
        // the filter excluded — the run has no evidence they are obsolete.
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path());
        let snap = snapshot_path(&spec);
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        let mut initial = SnapshotFile::new();
        for key in ["adds", "subtracts", "multiplies", "divides", "negates"] {
            initial.insert(key.into(), "old".into());
        }
        fs::write(&snap, serialize_snapshots(&initial)).unwrap();

        let mut store = SnapshotStore::new(true, false, true);
        store
            .record(&spec, "math.spec.luau", "adds", "new")
            .unwrap();
        let summary = store.finish().unwrap();

        assert_eq!(summary.updated, 1);
        // Neither counted nor pruned under a filter.
        assert_eq!(summary.obsolete, 0);
        let parsed = parse_snapshots(&fs::read_to_string(&snap).unwrap()).unwrap();
        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed.get("adds").map(String::as_str), Some("new"));
        assert_eq!(parsed.get("divides").map(String::as_str), Some("old"));
    }

    #[test]
    fn filtered_run_does_not_delete_the_snapshot_file() {
        // The pruning path removes an emptied `.snap` outright, so a filtered
        // run that touched none of a file's keys must not reach it at all.
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path());
        let other = dir.path().join("other.spec.luau");
        fs::write(&other, "return nil").unwrap();
        let snap = snapshot_path(&spec);
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        let mut initial = SnapshotFile::new();
        initial.insert("adds".into(), "3".into());
        fs::write(&snap, serialize_snapshots(&initial)).unwrap();

        let mut store = SnapshotStore::new(true, false, true);
        store
            .record(&other, "other.spec.luau", "unrelated", "1")
            .unwrap();
        store.finish().unwrap();
        assert!(snap.is_file(), "a filtered run deleted a live .snap file");
    }

    #[test]
    fn duplicate_key_is_reported_as_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let spec = spec_in(dir.path());
        let mut store = SnapshotStore::new(false, false, false);
        store.record(&spec, "math.spec.luau", "dupe", "3").unwrap();
        store.record(&spec, "math.spec.luau", "dupe", "4").unwrap();
        let summary = store.finish().unwrap();

        // The first write stands; the second is a failure, not a silent
        // overwrite that would leave neither test pinned.
        assert_eq!(summary.written, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(store.failures.len(), 1);
        assert!(store.failures[0].detail.contains("duplicate snapshot key"));
        let parsed = parse_snapshots(&fs::read_to_string(snapshot_path(&spec)).unwrap()).unwrap();
        assert_eq!(parsed.get("dupe").map(String::as_str), Some("3"));
    }
}
