//! Watch mode: re-run affected specs when the files they depend on change.
//!
//! A saved file maps to specs through the require graph — the same
//! `DependencyGraph`/`affected_specs` machinery `--changed` uses — and the
//! watcher re-runs every spec whose closure touches a changed file. Cloud
//! suites never participate; a config change reloads the config and re-runs
//! everything.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use crate::config::{self, BackendKind, Config, Suite};
use crate::error::ToolError;
use crate::report::{paint, render_warning};
use crate::{
    changed, discover, run_suites, run_suites_with, select_suites, RunArgs, RunExtras, RunParams,
    Severity,
};

const DEBOUNCE: Duration = Duration::from_millis(200);

/// Directory names whose contents never trigger a re-run. Dot-directories
/// (`.git`, lest's own `.lest`) need no entry: the hidden-entry check in
/// `is_interesting` already rejects every dot-prefixed component.
const IGNORED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "Packages",
    "luau_packages",
    "lune_packages",
];

/// A watch status line in the reporters' note voice: dim lowercase fragment,
/// no prefix, on stderr. `"2"` is the palette's `DIM` code — `report::pretty`
/// keeps the constant private to the reporters.
fn print_note(message: &str, color: bool) {
    eprintln!("{}", paint(color, "2", message));
}

/// The one watching banner, used at startup and after every pass — a single
/// spelling, so a session never renders the "same" line two ways.
fn print_banner(root: &Path, color: bool) {
    eprintln!();
    print_note(
        &format!(
            "watching {} — save a file to re-run (ctrl+c to quit)",
            root.display()
        ),
        color,
    );
}

/// `config_path` must already be absolute (or `None`): `config::load` resolves a
/// relative path against the directory it is handed, and everything here is
/// handed the project *root* — which is the config's own directory, so a
/// relative path would be joined onto itself.
pub fn run(
    args: &RunArgs,
    root: &Path,
    config_path: Option<&Path>,
    color: bool,
    err_color: bool,
) -> Result<(), ToolError> {
    let suite_names = &args.suites;
    let backend_override = args.backend;
    let name_filter = args.filter.as_deref();

    let (mut config, _) = config::load(config_path, root)?;
    let mut selected = watchable(select_suites(&config, suite_names, backend_override)?);
    if selected.is_empty() {
        return Err(ToolError(
            "nothing to watch — every selected suite uses the cloud backend".to_string(),
        ));
    }

    // The actual config file this run resolved, so a change to it is detected
    // by identity rather than a hardcoded `lest.toml` name.
    let config_file = resolved_config_path(&config, root);
    // Both spellings, for the same reason `collect_paths` records both: an
    // editor that saves by replacing the file emits its event while the old
    // inode is gone, and only the lexical key exists at that instant.
    let config_key = crate::resolve::cache_key_path(&config_file);
    let config_key_lexical = crate::resolve::normalize(&config_file);
    let watch_names = WatchNames {
        config_file: config_file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned()),
    };

    let outcome = run_suites(
        &config,
        root,
        &selected,
        &run_params(args, &config, None, color),
    )?;
    if outcome.specs == 0 {
        return Err(ToolError(crate::no_specs_message(&config)));
    }
    report_coverage(args, &config, &outcome, color, err_color);
    note_if_no_match(name_filter, &outcome, err_color);

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(tx)
        .map_err(|e| ToolError(format!("cannot start the file watcher: {e}")))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|e| ToolError(format!("cannot watch {}: {e}", root.display())))?;

    print_banner(root, err_color);

    loop {
        // Block for the first event, then drain the burst an editor save
        // produces before acting on it.
        let first = match rx.recv() {
            Ok(event) => event,
            // The sender half lives inside the watcher: a closed channel means
            // the watch backend died, not that the user asked to stop —
            // returning Ok here exited 0 while silently no longer watching.
            Err(_) => {
                return Err(ToolError(
                    "cannot keep watching — the file watcher stopped unexpectedly".to_string(),
                ))
            }
        };
        let mut changed_set = HashSet::new();
        let mut rescan = false;
        collect_paths(
            first,
            root,
            &watch_names,
            &mut changed_set,
            &mut rescan,
            err_color,
        );
        loop {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(event) => collect_paths(
                    event,
                    root,
                    &watch_names,
                    &mut changed_set,
                    &mut rescan,
                    err_color,
                ),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if changed_set.is_empty() && !rescan {
            continue;
        }

        // A rescan means events were dropped, so anything — including the
        // config — may have changed; treat it like a config change and re-run
        // everything under a freshly loaded config.
        let config_changed = rescan
            || changed_set.contains(&config_key)
            || changed_set.contains(&config_key_lexical);
        if config_changed {
            if !rescan {
                print_note(
                    &format!(
                        "{} changed — reloading configuration",
                        config_file.display()
                    ),
                    err_color,
                );
            }
            // The fresh config and the fresh selection are adopted together or
            // not at all: adopting the config first and then failing selection
            // left the loop running the *old* suites' globs and backends under
            // the *new* timeout/workers/core indefinitely.
            let reloaded = config::load(config_path, root).and_then(|(fresh, _)| {
                let fresh_selected = select_suites(&fresh, suite_names, backend_override)?;
                Ok((fresh, fresh_selected))
            });
            match reloaded {
                Ok((fresh, fresh_selected)) => {
                    config = fresh;
                    selected = watchable(fresh_selected);
                }
                Err(err) => {
                    eprint!(
                        "{}",
                        render_warning(
                            &format!(
                                "the configuration reload was rejected — still watching with the \
                                 previous configuration: {err}"
                            ),
                            err_color,
                        )
                    );
                    continue;
                }
            }
        }

        if selected.is_empty() {
            // Only reachable after a reload — at startup an all-cloud
            // selection is a hard error. Falling through would report "no
            // tests depend on the changed files" on every save, which
            // misdiagnoses: nothing is watchable at all.
            eprint!(
                "{}",
                render_warning(
                    "nothing to watch — every selected suite now uses the cloud backend; edit \
                     lest.toml to select a watchable suite",
                    err_color,
                )
            );
            continue;
        }

        let (pre_discovered, affected) = if config_changed {
            (None, None) // full re-run under the fresh config
        } else {
            match affected_specs(root, &selected, &changed_set) {
                Ok((_, affected)) if affected.is_empty() => {
                    print_note("no tests depend on the changed files", err_color);
                    continue;
                }
                Ok((discovered, affected)) => (Some(discovered), Some(affected)),
                Err(err) => {
                    print_error(&err, err_color);
                    continue;
                }
            }
        };

        let params = run_params(args, &config, affected.as_ref(), color);
        let extras = RunExtras {
            pre_discovered: pre_discovered.as_ref(),
            ..RunExtras::default()
        };
        match run_suites_with(&config, root, &selected, &params, &extras) {
            Ok(outcome) => {
                report_coverage(args, &config, &outcome, color, err_color);
                note_if_no_match(name_filter, &outcome, err_color);
            }
            Err(err) => print_error(&err, err_color),
        }
        print_banner(root, err_color);
    }
}

/// The per-run parameters, rebuilt each pass because the config can be reloaded
/// underneath the loop. `-u` and coverage are honored here rather than pinned
/// off: watch mode is where snapshot updates and coverage feedback are *most*
/// wanted, and a flag that silently does nothing is worse than one that costs a
/// little time.
fn run_params<'a>(
    args: &'a RunArgs,
    config: &Config,
    only_specs: Option<&'a HashSet<PathBuf>>,
    color: bool,
) -> RunParams<'a> {
    RunParams {
        only_specs,
        name_filter: args.filter.as_deref(),
        reporter: args.reporter,
        color,
        update: args.update,
        coverage: coverage_on(args, config),
    }
}

/// `--coverage`, `--min`, or a `[coverage] min` in the config all mean "measure
/// it" — the last one re-read on every pass, since a config reload may have
/// just added or removed it.
fn coverage_on(args: &RunArgs, config: &Config) -> bool {
    args.coverage || args.min.is_some() || config.coverage.min.is_some()
}

/// Prints the coverage report for a pass. The gate's exit code is discarded —
/// watch mode has no exit code to give — but the diagnostic still prints, so a
/// shortfall is visible the moment it appears rather than only in CI.
fn report_coverage(
    args: &RunArgs,
    config: &Config,
    outcome: &crate::RunOutcome,
    color: bool,
    err_color: bool,
) {
    if coverage_on(args, config) {
        crate::report_coverage(args, config, outcome.coverage.as_ref(), color, err_color);
    }
}

/// Renders a recoverable error the same way a fatal one is rendered. Watch mode
/// changes whether the loop survives an error, never how it reads.
fn print_error(err: &ToolError, color: bool) {
    eprint!(
        "{}",
        crate::render_diagnostic(Severity::Error, &err.to_string(), color)
    );
}

/// Reports a `-t` that selected nothing without ending the session. A one-shot
/// run treats this as a mistake in the invocation and exits non-zero, but in
/// watch mode it is transient by nature — renaming the test you are filtering on
/// passes through this state on the way to matching again — so the loop reports
/// it and keeps watching.
fn note_if_no_match(name_filter: Option<&str>, outcome: &crate::RunOutcome, color: bool) {
    if let Some(filter) = name_filter {
        if outcome.ran_nothing() {
            print_error(&ToolError(crate::no_match_message(filter)), color);
        }
    }
}

/// File names, beyond spec sources, whose changes matter to a watch run.
struct WatchNames {
    /// The resolved config file's name (may be a custom `--config` path).
    config_file: Option<String>,
}

/// The config file to watch. `config.file` is the one the load actually used,
/// already absolute — deriving it here from `--config` again is how the
/// relative case ended up joined twice, producing a key no notify event could
/// ever match. In zero-config mode there is no file yet, so watch the root's
/// `lest.toml`: creating one is exactly the change that should take effect.
fn resolved_config_path(config: &Config, root: &Path) -> PathBuf {
    config
        .file
        .clone()
        .unwrap_or_else(|| root.join("lest.toml"))
}

fn watchable(suites: Vec<Suite>) -> Vec<Suite> {
    suites
        .into_iter()
        // Cloud is excluded by physics (network round-trips per save); studio
        // is excluded until its watch integration lands — every re-run would
        // demand a fresh Run press, which needs its own design (armed-on-save).
        .filter(|suite| suite.backend != BackendKind::Cloud && suite.backend != BackendKind::Studio)
        .collect()
}

fn collect_paths(
    event: notify::Result<notify::Event>,
    root: &Path,
    names: &WatchNames,
    changed: &mut HashSet<PathBuf>,
    rescan: &mut bool,
    err_color: bool,
) {
    let event = match event {
        Ok(event) => event,
        Err(err) => {
            // A watcher error usually means events were dropped (queue
            // overflow, a path the backend lost track of), and a dropped event
            // is a save that never re-runs. Missing one silently is worse than
            // an extra run, so warn and re-run everything, exactly as if the
            // backend had requested a rescan.
            eprint!(
                "{}",
                render_warning(
                    &format!("the file watcher reported an error: {err}"),
                    err_color
                )
            );
            *rescan = true;
            return;
        }
    };
    if event.need_rescan() {
        // The backend admits it dropped events; only a full re-run is safe.
        *rescan = true;
    }
    for path in event.paths {
        if !is_interesting(&path, root, names) {
            continue;
        }
        // `cache_key_path` canonicalizes when the file exists — which is what
        // makes it line up with graph nodes (short 8.3 names expand, symlinks
        // resolve, `\\?\` appears on Windows) — and falls back to a lexical
        // `normalize` when it does not. A delete or rename-away arrives *after*
        // the file is gone, so it takes the lexical branch and can no longer
        // equal the canonical key the graph recorded, and the change reports
        // "no tests depend on the changed files". Insert both spellings; a
        // changed set is only ever tested for membership, so the extra key is
        // free when it matches nothing.
        let canonical = crate::resolve::cache_key_path(&path);
        let lexical = crate::resolve::normalize(&path);
        if lexical != canonical {
            changed.insert(lexical);
        }
        changed.insert(canonical);
    }
}

fn is_interesting(path: &Path, root: &Path, names: &WatchNames) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    for component in rel.components() {
        let name = component.as_os_str().to_string_lossy();
        if IGNORED_DIRS.contains(&name.as_ref()) {
            return false;
        }
        // Hidden entries are ignored except .luaurc, which affects require
        // resolution and therefore which tests a change reaches.
        if name.starts_with('.') && name != ".luaurc" {
            return false;
        }
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("luau") | Some("lua") => true,
        _ => {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            match name.as_deref() {
                Some(".luaurc") => true,
                Some(file) => names.config_file.as_deref() == Some(file),
                None => false,
            }
        }
    }
}

/// What one watch pass selected: the per-suite discovery (suite name → spec
/// files, pre-filtered to the affected set) and the affected cache-key paths.
type PassSelection = (HashMap<String, Vec<PathBuf>>, HashSet<PathBuf>);

/// Every spec whose transitive require closure touches a changed file, keyed
/// by cache-key path (matching `run_suites`' spec filter), plus the per-suite
/// discovery that found them. Shares the graph machinery with `--changed`.
///
/// The discovery is handed back so the run can reuse it via
/// [`RunExtras::pre_discovered`] instead of walking the tree a second time in
/// the same pass. Each suite's list is pre-filtered down to the affected set
/// through one cached [`crate::resolve::Resolver`] — `cache_key_path` is a
/// canonicalize syscall, so the run's own per-spec filter is left only the few
/// affected files to key.
fn affected_specs(
    root: &Path,
    suites: &[Suite],
    changed_set: &HashSet<PathBuf>,
) -> Result<PassSelection, ToolError> {
    let mut discovered: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut all_specs: Vec<PathBuf> = Vec::new();
    for suite in suites {
        let specs = discover::discover(root, &suite.include)?;
        all_specs.extend(specs.iter().cloned());
        discovered.insert(suite.name.clone(), specs);
    }
    let affected = changed::affected_specs(root, all_specs, changed_set);
    let resolver = crate::resolve::Resolver::new();
    for specs in discovered.values_mut() {
        specs.retain(|spec| affected.contains(&resolver.cache_key_path(spec)));
    }
    Ok((discovered, affected))
}
