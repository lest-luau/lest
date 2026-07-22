//! The native backend: the embedded Luau VM, lest's fast path for pure-logic
//! suites and the only backend fast enough for watch mode.
//!
//! Each spec file gets a fresh Luau VM and a fresh [`Loader`] — its own module
//! cache — and files are distributed across a worker pool. Nothing is shared
//! between spec files: isolation is per-VM, and a module required by two specs
//! is read and compiled once per spec, not once per run. That is the price of
//! real isolation, and it is deliberate; one spec's mutation of a shared module
//! table must not be visible to the next.
//!
//! Within one VM the framework is loaded first; the spec's own relative require
//! of lest/core resolves to the same normalized path and hits *that VM's* module
//! cache, so the spec and the runner share one framework instance. Runtime
//! builtins (`@lune/*`, `@lute/*`) are refused with a pointer to the backend
//! that has them — no environment is ever faked.
//!
//! Coverage (native-only) compiles every module with Luau's statement-coverage
//! level and, after the run, walks each loaded function's recorded hit counts
//! into a per-file line map. Aggregation across the worker pool sums hit counts
//! and unions the instrumentable-line set.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::report::{check_protocol_version, Event, Failure};
use crate::resolve::{normalize, on_disk_spelling, Resolved, Resolver};
use mlua::{Compiler, Error as LuaError, Function, Lua, Table, Value, VmState};

use crate::backend::{display_rel, EventSink, SuitePlan};
use crate::error::ToolError;

/// Per-file instrumentable line → hit count, aggregated for a whole run.
pub type CoverageMap = HashMap<PathBuf, BTreeMap<u32, u64>>;

/// One spec file's execution result: its event stream plus (when coverage is
/// on) the per-file line hits recorded in its VM. Extracted as an alias to keep
/// the parallel result-slot type readable (and clippy's `type_complexity`
/// quiet).
type SpecOutput = (Vec<Event>, CoverageMap);
type SpecResult = Result<SpecOutput, ToolError>;

/// Require bookkeeping for one VM: the stack of files currently being loaded
/// (for relative resolution and cycle detection), the module cache keyed by
/// normalized absolute path, and — when coverage is on — the loaded functions
/// retained so their hit counts can be read after the run.
struct Loader {
    stack: RefCell<Vec<PathBuf>>,
    cache: RefCell<HashMap<PathBuf, Value>>,
    coverage: bool,
    /// Retained `(attribution path, function)` pairs. The path here is the
    /// file's *on-disk spelling*, never the folded cache key: coverage display
    /// paths and the case-sensitive `[coverage] exclude` globs consume it.
    functions: RefCell<Vec<(PathBuf, Function)>>,
    /// Folded identity key → on-disk spelling, memoized so every load of one
    /// file attributes coverage to exactly one spelling (two spellings of one
    /// file in the map would double-count its lines).
    spellings: RefCell<HashMap<PathBuf, PathBuf>>,
    /// Memoizes `.luaurc` lookups for this VM's lifetime; discarded with it.
    resolver: Resolver,
}

pub fn run(
    plan: &SuitePlan,
    on_event: &mut EventSink,
    mut coverage_out: Option<&mut CoverageMap>,
) -> Result<(), ToolError> {
    let workers = effective_workers(plan);
    let results = if workers <= 1 {
        plan.specs
            .iter()
            .map(|spec| run_spec_file(plan, spec))
            .collect::<Vec<_>>()
    } else {
        run_parallel(plan, workers)
    };

    // A run that loaded spec files but never produced one test outcome means
    // the framework or the specs failed to register anything — a false green we
    // refuse, matching the spawned-runtime and cloud guards. Native is the
    // *default* backend, so this is the one place the guard matters most.
    let mut outcomes = 0usize;

    for (spec, result) in plan.specs.iter().zip(results) {
        let (events, coverage) = result.map_err(|e| {
            ToolError(format!(
                "cannot run {} on the native backend: {e}",
                display_rel(spec, &plan.root)
            ))
        })?;
        for event in &events {
            if let Event::RunStart {
                protocol_version, ..
            } = event
            {
                check_protocol_version(*protocol_version).map_err(|e| {
                    ToolError(format!(
                        "framework/CLI protocol mismatch in {}: {e}",
                        display_rel(spec, &plan.root)
                    ))
                })?;
            }
            if matches!(
                event,
                Event::TestPass { .. } | Event::TestFail { .. } | Event::TestSkip { .. }
            ) {
                outcomes += 1;
            }
            on_event(Some(spec), event);
        }
        if let Some(out) = coverage_out.as_deref_mut() {
            merge_coverage(out, coverage);
        }
    }

    // A name filter legitimately produces zero outcomes, so it disarms the
    // guard — the CLI reports the no-match case itself, with a message that
    // names the real cause instead of blaming the framework.
    if !plan.specs.is_empty() && outcomes == 0 && plan.name_filter.is_none() {
        return Err(ToolError(format!(
            "native suite \"{}\" ran {} spec file(s) but produced no test outcomes — the specs \
             likely registered nothing, or reached a second copy of lest/core",
            plan.name,
            plan.specs.len()
        )));
    }
    Ok(())
}

/// Sums `incoming` hit counts into `acc`, unioning the instrumentable-line set.
fn merge_coverage(acc: &mut CoverageMap, incoming: CoverageMap) {
    for (file, lines) in incoming {
        let entry = acc.entry(file).or_default();
        for (line, hits) in lines {
            *entry.entry(line).or_insert(0) += hits;
        }
    }
}

fn effective_workers(plan: &SuitePlan) -> usize {
    let configured = if plan.workers == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        plan.workers
    };
    configured.min(plan.specs.len().max(1))
}

/// Work-steals spec files across a scoped thread pool. Each worker owns its
/// VMs outright (`Lua` never crosses threads); results are collected per
/// file and replayed in file order so output stays deterministic.
fn run_parallel(plan: &SuitePlan, workers: usize) -> Vec<SpecResult> {
    let next = AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<SpecResult>>> =
        Mutex::new((0..plan.specs.len()).map(|_| None).collect());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::SeqCst);
                if index >= plan.specs.len() {
                    break;
                }
                // A panic anywhere under `run_spec_file` (mlua, the resolver, a
                // slice index) would otherwise unwind the worker, poison the
                // mutex, and abort the whole process with exit 101 — no
                // reporter footer, and every spec that already finished lost.
                // Contained here it becomes one spec's tool error instead, and
                // the exit-code policy still applies.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_spec_file(plan, &plan.specs[index])
                }))
                .unwrap_or_else(|payload| {
                    Err(ToolError(format!(
                        "the native worker panicked: {}",
                        panic_message(&*payload)
                    )))
                });
                // Tolerate a poisoned mutex: another worker panicking must not
                // discard the results this one just produced.
                let mut slots = slots.lock().unwrap_or_else(PoisonError::into_inner);
                slots[index] = Some(result);
            });
        }
    });

    slots
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .map(|slot| slot.expect("every spec slot filled"))
        .collect()
}

/// Best-effort rendering of a panic payload; `panic!` produces a `&str` or a
/// `String` and nothing else in practice.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn run_spec_file(plan: &SuitePlan, spec: &Path) -> SpecResult {
    let lua = Lua::new();
    if plan.coverage {
        // Statement-level coverage is enough for line hits and keeps the
        // recorded data small; expression coverage (level 2) is verbose.
        lua.set_compiler(Compiler::new().set_coverage_level(1));
    }
    let timeout = plan.timeout;
    let deadline = Rc::new(Cell::new(Instant::now() + timeout));
    // Timeout detection is this flag, never the error message text: a test
    // that raises the same phrase the interrupt uses must not be misreported
    // as a timeout, so only the interrupt path may arm it.
    let timed_out = Rc::new(Cell::new(false));

    {
        let deadline = Rc::clone(&deadline);
        let timed_out = Rc::clone(&timed_out);
        lua.set_interrupt(move |_| {
            if Instant::now() >= deadline.get() {
                // Defuse before erroring so the runner's bookkeeping after
                // the failed test does not immediately trip the interrupt
                // again; the next test_start re-arms it.
                deadline.set(Instant::now() + Duration::from_secs(3600));
                timed_out.set(true);
                return Err(LuaError::RuntimeError(format!(
                    "test timed out after {}ms",
                    timeout.as_millis()
                )));
            }
            Ok(VmState::Continue)
        });
    }

    let loader = Rc::new(Loader {
        stack: RefCell::new(Vec::new()),
        cache: RefCell::new(HashMap::new()),
        coverage: plan.coverage,
        functions: RefCell::new(Vec::new()),
        spellings: RefCell::new(HashMap::new()),
        resolver: Resolver::new(),
    });

    let require = {
        let loader = Rc::clone(&loader);
        lua.create_function(move |lua, spec_str: String| {
            let from = loader
                .stack
                .borrow()
                .last()
                .cloned()
                .ok_or_else(|| LuaError::runtime("require called outside of a module"))?;
            match loader.resolver.resolve(&from, &spec_str) {
                Ok(Resolved::File(path)) => load_module(lua, &loader, &path),
                Ok(Resolved::Builtin { runtime, module }) => Err(LuaError::runtime(format!(
                    "require(\"{module}\") needs the {runtime} runtime — the native backend has \
                     no runtime APIs; set `backend = \"{runtime}\"` on this suite in lest.toml"
                ))),
                Err(err) => Err(LuaError::runtime(err.to_string())),
            }
        })
        .map_err(|e| ToolError(format!("cannot install require into the VM: {e}")))?
    };
    lua.globals()
        .set("require", require)
        .map_err(|e| ToolError(e.to_string()))?;

    let core_value = load_module(&lua, &loader, &plan.core_entry).map_err(|e| {
        ToolError(format!(
            "cannot load lest/core from {}: {e}",
            plan.core_entry.display()
        ))
    })?;
    let core = match core_value {
        Value::Table(table) => table,
        other => {
            return Err(ToolError(format!(
                "lest/core returned a {} instead of a table",
                other.type_name()
            )))
        }
    };
    core.get::<Function>("reset")
        .and_then(|f| f.call::<()>(()))
        .map_err(|e| ToolError(format!("cannot reset lest/core: {e}")))?;

    let events: Rc<RefCell<Vec<Event>>> = Rc::new(RefCell::new(Vec::new()));

    // Collection phase: loading the spec registers its tests. A spec that
    // fails to load is a failing spec, not a broken tool.
    deadline.set(Instant::now() + timeout);
    if let Err(load_err) = load_module(&lua, &loader, spec) {
        events.borrow_mut().push(Event::TestFail {
            path: vec![display_rel(spec, &plan.root)],
            name: "(load)".to_string(),
            duration_ms: 0.0,
            failure: Failure::Error {
                message: load_err.to_string(),
                trace: None,
            },
        });
        let collected = std::mem::take(&mut *events.borrow_mut());
        return Ok((collected, collect_coverage(plan, &loader)));
    }

    let emit = {
        let events = Rc::clone(&events);
        let deadline = Rc::clone(&deadline);
        let timed_out = Rc::clone(&timed_out);
        lua.create_function(move |_, table: Table| {
            let event = event_from_table(&table).map_err(LuaError::runtime)?;
            if matches!(event, Event::TestStart { .. }) {
                deadline.set(Instant::now() + timeout);
                // Disarm alongside the re-arm: a timeout an earlier test's
                // pcall swallowed must not relabel a later, unrelated abort.
                timed_out.set(false);
            }
            events.borrow_mut().push(event);
            Ok(())
        })
        .map_err(|e| ToolError(e.to_string()))?
    };

    let options = match &plan.name_filter {
        Some(filter) => {
            let table = lua.create_table().map_err(|e| ToolError(e.to_string()))?;
            table
                .set("nameFilter", filter.as_str())
                .map_err(|e| ToolError(e.to_string()))?;
            Value::Table(table)
        }
        None => Value::Nil,
    };

    deadline.set(Instant::now() + timeout);
    // A timeout swallowed by a pcall during collection must not pre-arm the
    // flag before a single test has run.
    timed_out.set(false);
    if let Err(err) = core
        .get::<Function>("run")
        .and_then(|f| f.call::<()>((emit, options)))
    {
        // core wraps each `it` body in a pcall, so an ordinary test error is
        // already reported as a `test_fail`. A timeout interrupt, however, can
        // escape core.run (it fires between tests or unwinds through code core
        // does not pcall). That is still a *test* failure — surfacing it as a
        // tool error would misclassify a slow test as a broken tool — so it
        // becomes a synthetic `test_fail` (exit 1). Anything else is a genuine
        // tool error (exit 2). The `timed_out` flag, set only by the interrupt
        // path, is what distinguishes the two — matching on the message text
        // would let a test that raises the same phrase forge a timeout.
        let message = err.to_string();
        if timed_out.get() {
            events.borrow_mut().push(Event::TestFail {
                path: vec![display_rel(spec, &plan.root)],
                name: "(timeout)".to_string(),
                duration_ms: timeout.as_millis() as f64,
                failure: Failure::Error {
                    message: format!("test timed out after {}ms", timeout.as_millis()),
                    trace: None,
                },
            });
        } else {
            return Err(ToolError(format!("test run aborted: {message}")));
        }
    }

    // Take rather than clone: the Rc is still shared with the `emit` closure the
    // VM holds, so `try_unwrap` is not available and a full clone of every
    // event would be pure waste.
    let collected = std::mem::take(&mut *events.borrow_mut());
    Ok((collected, collect_coverage(plan, &loader)))
}

/// Reads coverage back out of the retained functions. Empty when coverage was
/// not requested. Line index maps directly to source line number; Luau reports
/// `-1` for non-instrumentable lines, which are dropped.
fn collect_coverage(plan: &SuitePlan, loader: &Loader) -> CoverageMap {
    if !plan.coverage {
        return CoverageMap::new();
    }
    let mut map = CoverageMap::new();
    for (path, function) in loader.functions.borrow().iter() {
        let lines = map.entry(path.clone()).or_default();
        function.coverage(|info| {
            for (line, &hits) in info.hits.iter().enumerate() {
                if hits >= 0 {
                    *lines.entry(line as u32).or_insert(0) += hits as u64;
                }
            }
        });
    }
    map
}

fn load_module(lua: &Lua, loader: &Loader, path: &Path) -> mlua::Result<Value> {
    // Identity is the *folded* key, per the Loader contract. Spec files arrive
    // here in their on-disk spelling (from `plan.specs`) while everything the
    // resolver produces arrives folded — on a case-insensitive host those are
    // one module, and an unfolded key would let a cycle back through the spec
    // (spec → helper → spec) miss both the cache and the cycle check and
    // re-execute the spec body. `normalize` is idempotent for keys the
    // resolver already folded.
    let key = normalize(path);
    if let Some(cached) = loader.cache.borrow().get(&key) {
        return Ok(cached.clone());
    }
    if loader.stack.borrow().iter().any(|entry| entry == &key) {
        return Err(LuaError::runtime(format!(
            "cyclic require detected at {}",
            path.display()
        )));
    }
    let source = std::fs::read_to_string(path)
        .map_err(|e| LuaError::runtime(format!("cannot read {}: {e}", path.display())))?;
    // Windows editors love UTF-8 BOMs; the Luau parser does not.
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);

    loader.stack.borrow_mut().push(key.clone());
    // Compile to a retainable function so coverage can be read back after the
    // run; calling it runs the module body exactly as `eval` would.
    let function = lua
        .load(source)
        .set_name(format!("@{}", path.display()))
        .into_function();
    let result = function.and_then(|f| {
        if loader.coverage {
            // Attribution is not identity: coverage display paths and the
            // case-sensitive `[coverage] exclude` globs (`Packages/**`) need
            // the on-disk spelling, so the folded key must never leak into
            // the coverage map.
            let attribution = attribution_spelling(loader, &key, path);
            loader.functions.borrow_mut().push((attribution, f.clone()));
        }
        f.call::<Value>(())
    });
    loader.stack.borrow_mut().pop();

    let value = result?;
    loader.cache.borrow_mut().insert(key, value.clone());
    Ok(value)
}

/// One on-disk spelling per module, memoized under its folded identity key —
/// however a file is reached (original spelling from `plan.specs`, folded from
/// the resolver), its coverage lands on a single map entry, so one file can
/// never appear under two casings and double-count its lines.
fn attribution_spelling(loader: &Loader, key: &Path, path: &Path) -> PathBuf {
    if let Some(known) = loader.spellings.borrow().get(key) {
        return known.clone();
    }
    let spelling = on_disk_spelling(path);
    loader
        .spellings
        .borrow_mut()
        .insert(key.to_path_buf(), spelling.clone());
    spelling
}

/// Manual event decoding instead of serde-through-Lua: explicit field errors,
/// and an empty Luau table decodes as an empty path rather than an ambiguous
/// empty map.
fn event_from_table(table: &Table) -> Result<Event, String> {
    let kind = get_string(table, "kind")?;
    let event = match kind.as_str() {
        "run_start" => Event::RunStart {
            spec_count: get_u32(table, "specCount")?,
            protocol_version: get_u32(table, "protocolVersion")?,
        },
        "suite_start" => Event::SuiteStart {
            path: get_path(table)?,
        },
        "test_start" => Event::TestStart {
            path: get_path(table)?,
            name: get_string(table, "name")?,
        },
        "test_pass" => Event::TestPass {
            path: get_path(table)?,
            name: get_string(table, "name")?,
            duration_ms: get_f64(table, "durationMs")?,
        },
        "test_fail" => Event::TestFail {
            path: get_path(table)?,
            name: get_string(table, "name")?,
            duration_ms: get_f64(table, "durationMs")?,
            failure: failure_from_table(table)?,
        },
        "test_skip" => Event::TestSkip {
            path: get_path(table)?,
            name: get_string(table, "name")?,
            reason: get_opt_string(table, "reason")?,
        },
        "snapshot" => Event::Snapshot {
            path: get_path(table)?,
            name: get_string(table, "name")?,
            key: get_string(table, "key")?,
            received: get_string(table, "received")?,
        },
        "run_end" => Event::RunEnd {
            passed: get_u32(table, "passed")?,
            failed: get_u32(table, "failed")?,
            skipped: get_u32(table, "skipped")?,
        },
        other => return Err(format!("unknown event kind \"{other}\"")),
    };
    Ok(event)
}

fn failure_from_table(event: &Table) -> Result<Failure, String> {
    let failure: Table = event
        .get("failure")
        .map_err(|e| format!("test_fail event without failure table: {e}"))?;
    let failure_type = get_string(&failure, "type")?;
    match failure_type.as_str() {
        "assertion" => Ok(Failure::Assertion {
            message: get_string(&failure, "message")?,
            expected: get_opt_string(&failure, "expected")?,
            received: get_opt_string(&failure, "received")?,
        }),
        "error" => Ok(Failure::Error {
            message: get_string(&failure, "message")?,
            trace: get_opt_string(&failure, "trace")?,
        }),
        other => Err(format!("unknown failure type \"{other}\"")),
    }
}

fn get_string(table: &Table, key: &str) -> Result<String, String> {
    table
        .get::<String>(key)
        .map_err(|e| format!("event field \"{key}\": {e}"))
}

fn get_opt_string(table: &Table, key: &str) -> Result<Option<String>, String> {
    table
        .get::<Option<String>>(key)
        .map_err(|e| format!("event field \"{key}\": {e}"))
}

fn get_f64(table: &Table, key: &str) -> Result<f64, String> {
    table
        .get::<f64>(key)
        .map_err(|e| format!("event field \"{key}\": {e}"))
}

fn get_u32(table: &Table, key: &str) -> Result<u32, String> {
    table
        .get::<u32>(key)
        .map_err(|e| format!("event field \"{key}\": {e}"))
}

fn get_path(table: &Table) -> Result<Vec<String>, String> {
    let path: Table = table
        .get("path")
        .map_err(|e| format!("event field \"path\": {e}"))?;
    let mut segments = Vec::new();
    for segment in path.sequence_values::<String>() {
        segments.push(segment.map_err(|e| format!("event field \"path\": {e}"))?);
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// A minimal stand-in for lest/core: enough surface for `run_spec_file`
    /// (a `reset` and a `run`) without dragging the real framework into a
    /// fixture that only exists to exercise the loader's key production.
    const FAKE_CORE: &str =
        "return {\n\treset = function () end,\n\trun = function (emit, options) end,\n}\n";

    /// Coverage keys must carry the on-disk spelling, produced by the *real*
    /// loader — a fabricated map cannot catch the folded keys `resolve()`
    /// hands `load_module`, which broke the case-sensitive `Packages/**`
    /// exclude on Windows/macOS and lowercased every displayed path.
    #[test]
    fn coverage_keys_keep_on_disk_casing_through_the_real_loader() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("core/init.luau"), FAKE_CORE);
        write(&root.join("Packages/Dep.luau"), "return { answer = 42 }\n");
        write(
            &root.join("Specs/Case.spec.luau"),
            "local dep = require('../Packages/Dep')\nreturn dep\n",
        );

        let spec = root.join("Specs/Case.spec.luau");
        let plan = SuitePlan {
            name: "unit".to_string(),
            specs: vec![spec.clone()],
            root: root.to_path_buf(),
            core_entry: normalize(&root.join("core/init.luau")),
            timeout: Duration::from_secs(5),
            workers: 1,
            name_filter: None,
            coverage: true,
        };
        // On case-insensitive hosts, hand the loader a deliberately mangled
        // spelling — the filesystem still finds the file, and attribution must
        // come out in the on-disk spelling regardless of how it was reached.
        let spec_arg = if cfg!(any(windows, target_os = "macos")) {
            root.join("specs/CASE.SPEC.LUAU")
        } else {
            spec.clone()
        };
        let (_events, coverage) = run_spec_file(&plan, &spec_arg).unwrap();

        let displays: Vec<String> = coverage
            .keys()
            .map(|key| key.to_string_lossy().replace('\\', "/"))
            .collect();
        // core + dependency + spec, each under exactly one spelling — a folded
        // and an unfolded key for one file would double-count its lines.
        assert_eq!(displays.len(), 3, "one key per file: {displays:?}");
        assert!(
            displays
                .iter()
                .any(|display| display.ends_with("Packages/Dep.luau")),
            "dependency must keep its on-disk casing: {displays:?}"
        );
        assert!(
            displays
                .iter()
                .any(|display| display.ends_with("Specs/Case.spec.luau")),
            "spec must keep its on-disk casing: {displays:?}"
        );
    }

    /// The audit's fix 3: a require cycle back through the spec file must hit
    /// the cycle check even though the spec entered the loader in its on-disk
    /// spelling while the resolver hands back a folded path.
    #[test]
    fn a_cycle_back_through_the_spec_is_detected_not_reexecuted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("core/init.luau"), FAKE_CORE);
        write(
            &root.join("Specs/Loop.spec.luau"),
            "local helper = require('./helper')\nreturn nil\n",
        );
        write(
            &root.join("Specs/helper.luau"),
            "return require('./Loop.spec')\n",
        );

        let spec = root.join("Specs/Loop.spec.luau");
        let plan = SuitePlan {
            name: "unit".to_string(),
            specs: vec![spec.clone()],
            root: root.to_path_buf(),
            core_entry: normalize(&root.join("core/init.luau")),
            timeout: Duration::from_secs(5),
            workers: 1,
            name_filter: None,
            coverage: false,
        };
        let (events, _coverage) = run_spec_file(&plan, &spec).unwrap();
        // The cycle surfaces as the spec's load failure — and it must be
        // caught *at the spec*: an unfolded spec key would miss the cycle
        // check there, re-execute the spec body, and only trip on the helper
        // one lap later.
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::TestFail { failure: Failure::Error { message, .. }, .. }
                    if message.contains("cyclic require")
                        && message.to_lowercase().contains("loop.spec")
            )),
            "expected a cyclic-require load failure at the spec: {events:?}"
        );
    }
}
