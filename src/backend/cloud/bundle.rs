//! The Luau bundler for the cloud backend.
//!
//! The bundle assumes nothing about the target place: everything lest needs
//! is inlined into one self-contained chunk that Open Cloud runs and whose
//! return value (the collector's buffered events) it ships back as JSON, so an
//! empty place works. A *populated* place (rojo-built fixtures, say) is
//! supported too — a require of a ModuleScript Instance is delegated to the
//! engine's own `require`, so place modules load through the native cache and
//! keep module identity shared with the rest of the place's code. Two sources
//! feed the bundle: lest/core and the specs come from the user's project (their
//! transitive closure, read off disk), while the in-engine runtime that collects
//! and returns events (see `EMBEDDED`) is compiled into the CLI. Running engine
//! tests therefore needs nothing installed beyond lest/core — the collector and
//! scheduler are lest's own plumbing, not something a user writes against.
//!
//! Each module body is wrapped in a factory `function()` given a private
//! `require` bound to a precomputed arg→module-id map, so `require('@self/x')`,
//! `require('./x')`, and relative string requires resolve among the inlined set
//! *without rewriting the module source*. A shared cache preserves singleton
//! module semantics (requiring the same module twice returns the same table),
//! so a spec and the entrypoint share one lest/core instance and its
//! registrations — exactly as the native and spawned-runtime backends arrange.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::resolve::{
    normalize, scan_requires, scan_requires_spanned, ResolveError, Resolved, Resolver,
};

use crate::error::ToolError;

/// One spec file to run in the bundle, with the display name used in event
/// paths and snapshot attribution.
pub struct SpecEntry {
    pub name: String,
    pub path: PathBuf,
}

/// A bundled entrypoint plus what the bundler noticed while building it.
pub struct Bundle {
    /// The self-contained Luau script an Open Cloud task runs.
    pub script: String,
    /// String requires that failed to resolve, in module-emission order. Not
    /// an error — the require may be dead code in the engine (a shared module
    /// branching on runtime) — but worth a warning with a real source
    /// position, because the alternative is the shim's runtime error at a
    /// bundle coordinate no reader can use.
    pub unresolved: Vec<UnresolvedRequire>,
    /// Where each module's source lines sit in `script`, so an engine error
    /// position — a bundle coordinate — can be translated back to the disk
    /// file and line it came from.
    pub source_map: SourceMap,
}

/// The bundle-line spans of every inlined module body, in emission order.
#[derive(Debug, Default)]
pub struct SourceMap {
    spans: Vec<SourceSpan>,
}

/// One module body's position in the emitted bundle. Spans cover the verbatim
/// source only; the factory scaffolding around it (the module comment, the
/// `__map` line, the require shim) belongs to no source line.
#[derive(Debug)]
pub struct SourceSpan {
    /// The module's file on disk; `None` for the CLI-embedded runtime
    /// modules, which have no path a user could open.
    pub file: Option<PathBuf>,
    /// The label the bundle's module comment carries — what to print when
    /// there is no file.
    pub label: String,
    /// 1-based bundle line where the module's source line 1 was emitted.
    start_line: usize,
    /// How many lines of source the module occupies.
    line_count: usize,
}

impl SourceMap {
    /// Translates a 1-based bundle line into the span containing it and the
    /// module-local (1-based) line. Bundle lines in scaffolding — the
    /// prelude, factory preambles, the entrypoint — belong to no module and
    /// return `None`, so callers leave those coordinates as they arrived.
    pub fn resolve(&self, bundle_line: usize) -> Option<(&SourceSpan, usize)> {
        // Emission order means `start_line`s are strictly increasing, so the
        // only candidate is the last span starting at or before the line.
        let candidates = self.spans.partition_point(|s| s.start_line <= bundle_line);
        let span = self.spans[..candidates].last()?;
        let offset = bundle_line - span.start_line;
        (offset < span.line_count).then_some((span, offset + 1))
    }

    fn push(&mut self, span: SourceSpan) {
        self.spans.push(span);
    }
}

/// A string require the bundler could not resolve, omitted from the emitted
/// `__map` exactly as before — this is the report, not a behavior change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnresolvedRequire {
    /// The requiring module, normalized — the same spelling used as its
    /// closure key.
    pub file: PathBuf,
    /// 1-based line of the `require` call site.
    pub line: usize,
    pub spec: String,
    /// A one-line reason, already phrased for a warning body.
    pub reason: String,
}

/// Compresses a [`ResolveError`] into a fragment a one-line warning can carry
/// — the error's own `Display` runs multi-line for some variants (`NotFound`
/// lists every probed path).
fn brief_reason(error: &ResolveError) -> String {
    match error {
        ResolveError::UnsupportedSpec { .. } => {
            "require paths start with \"./\", \"../\", or an \"@\" alias".to_string()
        }
        ResolveError::UnknownAlias { .. } => "unknown alias".to_string(),
        ResolveError::InvalidSelf { .. } => "@self is only valid from an init module".to_string(),
        ResolveError::Luaurc { path, .. } => {
            format!("unreadable .luaurc at {}", path.display())
        }
        ResolveError::Project { path, .. } => {
            format!("unreadable rojo project at {}", path.display())
        }
        ResolveError::NotFound { .. } => "no matching file on disk".to_string(),
    }
}

/// Everything the bundler needs to emit one self-contained entrypoint.
pub struct BundleInput<'a> {
    pub core_entry: &'a Path,
    pub specs: &'a [SpecEntry],
    pub name_filter: Option<&'a str>,
    /// Per-spec scheduler deadline inside the engine, in milliseconds.
    pub deadline_ms: u64,
}

/// Module sources read off disk, reusable across the bundles of one suite.
///
/// Cloud submits **one task per spec file** (that is what buys per-spec
/// attribution), so lest/core and every shared dependency is emitted into every
/// task. Uploading them each time is inherent to that choice; re-reading them
/// from disk each time is not, and a suite of N specs otherwise reads the whole
/// framework N times.
#[derive(Default)]
pub struct SourceCache {
    files: HashMap<PathBuf, String>,
}

impl SourceCache {
    fn read(&mut self, path: &Path) -> Result<&str, ToolError> {
        use std::collections::hash_map::Entry;
        let key = normalize(path);
        match self.files.entry(key) {
            Entry::Occupied(entry) => Ok(entry.into_mut().as_str()),
            Entry::Vacant(entry) => {
                let source = std::fs::read_to_string(path).map_err(|e| {
                    ToolError(format!(
                        "cannot read {} while bundling the cloud suite: {e}",
                        path.display()
                    ))
                })?;
                // Windows editors love UTF-8 BOMs; the Luau parser does not.
                // Written as a `starts_with` test rather than `strip_prefix` so
                // the untouched case can move the string instead of copying it.
                let source = if source.starts_with('\u{feff}') {
                    source['\u{feff}'.len_utf8()..].to_string()
                } else {
                    source
                };
                Ok(entry.insert(source).as_str())
            }
        }
    }
}

/// Builds the self-contained Luau entrypoint for `input`, reading every module
/// fresh. Production always bundles a whole suite and shares a [`SourceCache`]
/// via [`bundle_with_cache`], so this convenience form exists for tests only.
#[cfg(test)]
pub fn bundle(input: &BundleInput) -> Result<Bundle, ToolError> {
    bundle_with_cache(input, &mut SourceCache::default())
}

/// Builds the self-contained Luau entrypoint for `input`. Reads every module in
/// the transitive closure from disk (through `cache`); a missing/unreadable
/// module is a tool error rather than a broken upload.
pub fn bundle_with_cache(
    input: &BundleInput,
    cache: &mut SourceCache,
) -> Result<Bundle, ToolError> {
    let core = normalize(input.core_entry);
    let resolver = Resolver::new();

    // Roots whose transitive closure we inline from the project: lest/core and
    // the specs. The in-engine collector/scheduler are not here — they are
    // embedded in the CLI, so the run needs nothing extra installed.
    let mut roots: Vec<PathBuf> = vec![core.clone()];
    for spec in input.specs {
        roots.push(normalize(&spec.path));
    }

    // One walk over every root with one shared visited set — an independent
    // closure per root re-reads every shared dependency (the whole framework
    // included) once per root. The walk follows resolvable string requires
    // transitively and ignores builtins, which the cloud backend never has to
    // inline (engine specs use ambient globals). Every read goes through
    // `cache` — the same read `emit_module` will get — so the closure and the
    // emitted source can never disagree about a file's content; two
    // independent reads left a window where an edit between them produced a
    // require missing from `__map`.
    let mut closure: HashSet<PathBuf> = HashSet::new();
    let mut queue: Vec<PathBuf> = roots;
    while let Some(file) = queue.pop() {
        if !closure.insert(file.clone()) {
            continue;
        }
        let requires = scan_requires(cache.read(&file)?);
        for spec in requires {
            if let Ok(Resolved::File(path)) = resolver.resolve(&file, &spec) {
                if !closure.contains(&path) {
                    queue.push(path);
                }
            }
        }
    }

    // Deterministic id assignment for stable, testable output.
    let mut modules: Vec<PathBuf> = closure.into_iter().collect();
    modules.sort();
    let mut id_of: BTreeMap<PathBuf, String> = BTreeMap::new();
    for (index, path) in modules.iter().enumerate() {
        id_of.insert(path.clone(), format!("m{index}"));
    }

    let module_id = |path: &Path| -> Result<String, ToolError> {
        id_of.get(&normalize(path)).cloned().ok_or_else(|| {
            ToolError(format!(
                "cannot bundle {} for the cloud suite: it is not in the computed require closure",
                path.display()
            ))
        })
    };

    let mut out = String::new();
    out.push_str(PRELUDE);

    let mut unresolved = Vec::new();
    let mut source_map = SourceMap::default();
    for path in &modules {
        emit_module(
            &mut out,
            path,
            &id_of,
            cache,
            &resolver,
            &mut unresolved,
            &mut source_map,
        )?;
    }
    // The CLI-embedded in-engine runtime, inlined from compiled-in source
    // under fixed `lr_*` ids.
    for module in EMBEDDED {
        emit_embedded(&mut out, module, &mut source_map);
    }

    // ── Entrypoint ──────────────────────────────────────────────────────────
    let core_id = module_id(&core)?;

    out.push_str(
        "-- Entrypoint: run each spec through the embedded collector, return its events.\n",
    );
    out.push_str(&format!("local Lest = __lest_require('{core_id}')\n"));
    out.push_str(&format!(
        "local Collector = __lest_require('{COLLECTOR_ID}')\n"
    ));
    out.push_str(&format!(
        "local Scheduler = __lest_require('{SCHEDULER_ID}')\n"
    ));
    out.push_str("local collector = Collector.new()\n");
    out.push_str("local __lest_specs = {\n");
    for spec in input.specs {
        let spec_id = module_id(&spec.path)?;
        out.push_str(&format!(
            "\t{{ name = '{}', load = function () return __lest_require('{spec_id}') end }},\n",
            luau_escape(&spec.name),
        ));
    }
    out.push_str("}\n");

    let name_filter = match input.name_filter {
        Some(filter) => format!("'{}'", luau_escape(filter)),
        None => "nil".to_string(),
    };

    out.push_str(&format!(
        r#"for _, spec in __lest_specs do
	Lest.reset()
	local ok, err = pcall(spec.load)
	if not ok then
		collector.emit({{
			kind = 'test_fail', path = {{ spec.name }}, name = '(load)',
			durationMs = 0,
			failure = {{ type = 'error', message = tostring(err), trace = '' }},
		}})
	else
		local result = Scheduler.runSuite(function ()
			Lest.run(collector.emit, {{ nameFilter = {name_filter} }})
		end, {{ task = task, deadlineMs = {deadline} }})
		if result.timedOut then
			collector.emit({{
				kind = 'test_fail', path = {{ spec.name }}, name = '(timeout)',
				durationMs = result.durationMs,
				failure = {{ type = 'error', message = 'spec exceeded its deadline', trace = '' }},
			}})
		elseif result.error ~= nil then
			-- Scheduler.runSuite captures a raised error rather than re-raising
			-- it, so without this branch a mid-run error after some tests passed
			-- leaves outcomes > 0, disarms the zero-outcome guard, and reports a
			-- green run with the remaining tests silently missing — on the
			-- backend CI turns on by itself.
			collector.emit({{
				kind = 'test_fail', path = {{ spec.name }}, name = '(error)',
				durationMs = result.durationMs,
				failure = {{ type = 'error', message = tostring(result.error), trace = '' }},
			}})
		end
	end
end

return collector.events()
"#,
        name_filter = name_filter,
        deadline = input.deadline_ms,
    ));

    Ok(Bundle {
        script: out,
        unresolved,
        source_map,
    })
}

/// One module of the CLI-embedded in-engine runtime.
struct EmbeddedModule {
    /// Base name, matched against `require('./name')` / `require('@self/name')`.
    name: &'static str,
    id: &'static str,
    source: &'static str,
}

const COLLECTOR_ID: &str = "lr_collector";
const SCHEDULER_ID: &str = "lr_scheduler";

/// The in-engine runtime the cloud entrypoint drives: a collector that buffers
/// protocol events for the task to return, the task-scheduler integration that
/// runs a suite under real engine async, and the collector's JSON sanitizer.
///
/// These files live in `luau/runtime/cloud/` and are compiled into the binary,
/// same way the spawned-runtime harness template is — this is lest's own
/// plumbing, not a package a user installs or writes against. An engine run
/// therefore needs nothing beyond lest/core in the project.
///
/// Ids live in a dedicated `lr_*` namespace, disjoint from disk modules' `mN`.
const EMBEDDED: &[EmbeddedModule] = &[
    EmbeddedModule {
        name: "collector",
        id: COLLECTOR_ID,
        source: include_str!("../../../luau/runtime/cloud/collector.luau"),
    },
    EmbeddedModule {
        name: "scheduler",
        id: SCHEDULER_ID,
        source: include_str!("../../../luau/runtime/cloud/scheduler.luau"),
    },
    EmbeddedModule {
        name: "sanitize",
        id: "lr_sanitize",
        source: include_str!("../../../luau/runtime/cloud/sanitize.luau"),
    },
];

/// Resolves an embedded module's require arg (`./sanitize`, `@self/sanitize`)
/// to a sibling embedded module id by its final path segment.
fn embedded_id_for(arg: &str) -> Option<&'static str> {
    let base = arg.rfind('/').map_or(arg, |slash| &arg[slash + 1..]);
    EMBEDDED.iter().find(|m| m.name == base).map(|m| m.id)
}

/// Emits a factory for a project module (lest/core, a spec, or a dependency),
/// taking its source from `cache` — always a hit, since the closure walk read
/// every module through the same cache — and resolving its requires against
/// the on-disk closure. Requires that fail to resolve are reported into
/// `unresolved` with their call-site line, since the alternative report is the
/// shim's runtime error at a bundle coordinate.
fn emit_module(
    out: &mut String,
    path: &Path,
    id_of: &BTreeMap<PathBuf, String>,
    cache: &mut SourceCache,
    resolver: &Resolver,
    unresolved: &mut Vec<UnresolvedRequire>,
    source_map: &mut SourceMap,
) -> Result<(), ToolError> {
    let id = id_of
        .get(&normalize(path))
        .expect("every module has an id")
        .clone();
    let source = cache.read(path)?;

    // Build the arg→id map from this module's own requires, resolved relative
    // to it. Unresolvable requires and builtins are omitted; the injected
    // `require` errors clearly if the module reaches for one at run time.
    let mut mappings: BTreeMap<String, String> = BTreeMap::new();
    for found in scan_requires_spanned(source) {
        if mappings.contains_key(&found.spec) {
            continue;
        }
        match resolver.resolve(path, &found.spec) {
            Ok(Resolved::File(target)) => {
                if let Some(target_id) = id_of.get(&normalize(&target)) {
                    mappings.insert(found.spec, target_id.clone());
                }
            }
            // Builtins (`@lune/*`, `@lute/*`) are deliberate in modules shared
            // across runtimes, where the engine branch never reaches them —
            // legal dead code, not worth a warning. One that *is* reached
            // still hits the shim's loud runtime error.
            Ok(Resolved::Builtin { .. }) => {}
            Err(error) => unresolved.push(UnresolvedRequire {
                file: path.to_path_buf(),
                line: found.line,
                spec: found.spec,
                reason: brief_reason(&error),
            }),
        }
    }
    let label = path.display().to_string();
    let (start_line, line_count) = write_module_factory(out, &label, &id, source, &mappings);
    source_map.push(SourceSpan {
        file: Some(path.to_path_buf()),
        label,
        start_line,
        line_count,
    });
    Ok(())
}

/// Emits a factory for an embedded runtime module, resolving its sibling
/// requires among the embedded set rather than against the filesystem.
fn emit_embedded(out: &mut String, module: &EmbeddedModule, source_map: &mut SourceMap) {
    let source = module
        .source
        .strip_prefix('\u{feff}')
        .unwrap_or(module.source);
    let mut mappings: BTreeMap<String, String> = BTreeMap::new();
    for spec in scan_requires(source) {
        if mappings.contains_key(&spec) {
            continue;
        }
        if let Some(id) = embedded_id_for(&spec) {
            mappings.insert(spec, id.to_string());
        }
    }
    let label = format!("embedded lest/roblox: {}", module.name);
    let (start_line, line_count) = write_module_factory(out, &label, module.id, source, &mappings);
    source_map.push(SourceSpan {
        file: None,
        label,
        start_line,
        line_count,
    });
}

/// Writes one `__lest_modules['id'] = function() <require map> <source> end`
/// factory. The injected `require` shadows the global for the module body,
/// mapping each require literal to an inlined module id. Returns the
/// (1-based bundle start line, line count) of the verbatim body — the span
/// the source map needs, measured here because only this function knows where
/// scaffolding ends and source begins.
fn write_module_factory(
    out: &mut String,
    label: &str,
    id: &str,
    source: &str,
    mappings: &BTreeMap<String, String>,
) -> (usize, usize) {
    // The label goes through the same escaping as every other interpolation in
    // this file. It is only a comment, but it is built from a filesystem path,
    // and a path containing a newline would end the comment early and splice
    // whatever followed into the chunk as code.
    out.push_str(&format!(
        "-- Module: {}\n__lest_modules['{id}'] = function ()\n",
        luau_escape(label)
    ));
    out.push_str("\tlocal __map = {");
    let mut first = true;
    for (arg, target_id) in mappings {
        if !first {
            out.push_str(", ");
        }
        first = false;
        out.push_str(&format!("['{}'] = '{target_id}'", luau_escape(arg)));
    }
    out.push_str("}\n");
    // The shim owns *string* requires only — those were resolvable at bundle
    // time or they are a loud error, guarding the class of bug where a miss
    // would silently load the wrong thing. The error names the requiring
    // module: the argument value alone makes two call sites requiring the
    // same bad string print identically, and the error's own position is a
    // bundle coordinate no reader can use. A non-string argument (a
    // ModuleScript Instance, a legacy asset id) is delegated to the engine's
    // captured `require` and never enters `__lest_cache`: the native module
    // cache must own place-module identity, or a spec and an in-place fixture
    // requiring the same ModuleScript would see two copies.
    out.push_str(&format!(
        "\tlocal function require (spec)\n\
         \t\tlocal id = __map[spec]\n\
         \t\tif id ~= nil then\n\
         \t\t\treturn __lest_require(id)\n\
         \t\tend\n\
         \t\tif type(spec) ~= 'string' then\n\
         \t\t\treturn __lest_native_require(spec)\n\
         \t\tend\n\
         \t\terror('lest cloud bundle: unresolved require(' .. spec .. ') in {label}; string requires must resolve at bundle time, instance requires are delegated to the engine', 2)\n\
         \tend\n",
        label = luau_escape(label)
    ));
    // The module body is inlined verbatim; its top-level `return` becomes the
    // factory's return value. Recounting `out` per module is quadratic in
    // principle, but bundles are a few hundred KB — not worth threading a
    // running line counter through every push site to avoid.
    let start_line = count_newlines(out) + 1;
    let line_count = count_newlines(source) + usize::from(!source.ends_with('\n'));
    out.push_str(source);
    if !source.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("end\n\n");
    (start_line, line_count)
}

fn count_newlines(text: &str) -> usize {
    text.bytes().filter(|&b| b == b'\n').count()
}

/// Escapes a string for a single-quoted Luau literal (mirrors the runtime
/// harness's escaping so control characters cannot break the generated chunk).
fn luau_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

const PRELUDE: &str = r#"-- Generated by lest — self-contained cloud bundle. Do not edit.
-- Every lest module is inlined as a lazily-evaluated factory keyed by a
-- synthetic id; a shared cache preserves singleton module semantics so the
-- entrypoint and the specs share one lest/core instance and its registrations.

-- The engine's own require, captured before any module factory shadows it.
-- Strings are the bundler's domain; anything else (a ModuleScript Instance, a
-- legacy asset id) is the engine's, and goes through here so the native module
-- cache owns identity — a place fixture and a spec requiring the same
-- ModuleScript must get the same table.
local __lest_native_require = require

local __lest_modules = {}
local __lest_cache = {}
local __lest_loading = {}

local function __lest_require (id)
	local cached = __lest_cache[id]
	if cached ~= nil then
		return cached.value
	end
	local factory = __lest_modules[id]
	if factory == nil then
		error('lest cloud bundle: unknown module id ' .. tostring(id), 2)
	end
	-- The cache entry is only written after the factory returns, so a cyclic
	-- require would otherwise recurse until the engine's C stack overflows —
	-- surfacing as an opaque FAILED task after a full network round trip. The
	-- native backend catches the same mistake locally; this makes the cloud
	-- backend say the same thing. The marker is cleared even when the factory
	-- raises, so one module's load error cannot make a later require of it look
	-- like a cycle.
	if __lest_loading[id] then
		error('lest cloud bundle: cyclic require of ' .. id, 2)
	end
	__lest_loading[id] = true
	local ok, value = pcall(factory)
	__lest_loading[id] = nil
	if not ok then
		error(value, 0)
	end
	__lest_cache[id] = { value = value }
	return value
end

"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The crate is the repo, so the manifest directory *is* the root. These
    /// tests bundle lest's own core and specs, so they need the real tree.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn core_entry(root: &Path) -> PathBuf {
        root.join("luau/core/init.luau")
    }

    #[test]
    fn bundles_real_core_and_embedded_roblox_with_a_spec() {
        let root = repo_root();
        let spec = root.join("tests/core/expect.spec.luau");
        assert!(
            spec.is_file(),
            "fixture spec must exist: {}",
            spec.display()
        );

        let specs = vec![SpecEntry {
            name: "tests/core/expect.spec".to_string(),
            path: spec.clone(),
        }];
        let input = BundleInput {
            core_entry: &core_entry(&root),
            specs: &specs,
            name_filter: None,
            deadline_ms: 30000,
        };
        let bundle = bundle(&input).expect("bundle should succeed");
        // Every require in lest's own core and specs resolves — a warning here
        // would mean the framework ships a require its own bundler cannot see.
        assert_eq!(bundle.unresolved, vec![]);
        let script = bundle.script;

        // Structural properties of a self-contained bundle:
        assert!(script.contains("__lest_modules"));
        assert!(script.contains("__lest_require"));
        assert!(script.contains("return collector.events()"));
        assert!(script.contains("Collector.new()"));
        assert!(script.contains("Scheduler.runSuite"));
        // Core's own dependencies come off disk.
        assert!(script.contains("expect.luau"), "core's expect must inline");
        // The in-engine runtime comes from the CLI's embedded source, not disk.
        for id in ["lr_collector", "lr_scheduler", "lr_sanitize"] {
            assert!(
                script.contains(&format!("__lest_modules['{id}']")),
                "embedded module {id} must inline"
            );
        }
        // The embedded collector's sibling require resolves within the set.
        assert!(script.contains("['./sanitize'] = 'lr_sanitize'"));
        // The spec's own require of core resolves within the inlined set.
        assert!(script.contains("expect.spec"), "the spec body must inline");

        // Every module referenced by an id must be defined. Collect the ids
        // used in requires and ensure each is declared as a module.
        for id in 0..40 {
            let marker = format!("__lest_require('m{id}')");
            if script.contains(&marker) {
                assert!(
                    script.contains(&format!("__lest_modules['m{id}']")),
                    "referenced module m{id} must be defined"
                );
            }
        }
    }

    #[test]
    fn shim_delegates_non_string_requires_to_the_engine() {
        let root = repo_root();
        let spec = root.join("tests/core/expect.spec.luau");
        let specs = vec![SpecEntry {
            name: "expect.spec".to_string(),
            path: spec,
        }];
        let input = BundleInput {
            core_entry: &core_entry(&root),
            specs: &specs,
            name_filter: None,
            deadline_ms: 30000,
        };
        let script = bundle(&input).unwrap().script;

        // The engine's require is captured in the prelude, before any factory
        // shadows the name — a capture after the first factory would grab the
        // shim instead.
        let capture = script
            .find("local __lest_native_require = require")
            .expect("prelude must capture the native require");
        let first_factory = script
            .find("__lest_modules['")
            .expect("bundle must define modules");
        assert!(
            capture < first_factory,
            "native require must be captured before the first module factory"
        );

        // Every shim delegates non-string arguments and still errors loudly on
        // an unresolved string.
        assert!(script.contains("return __lest_native_require(spec)"));
        assert!(script.contains("string requires must resolve at bundle time"));
    }

    /// Executes the generated shim in an embedded Luau VM instead of just
    /// grepping the output: a table stands in for a ModuleScript Instance
    /// (both are non-strings, which is all the shim discriminates on), and the
    /// stubbed global `require` plays the engine's.
    #[test]
    fn generated_shim_delegates_at_runtime_and_stays_loud_on_string_misses() {
        use mlua::{Lua, Table, Value};

        let mut script = String::new();
        script.push_str(PRELUDE);
        write_module_factory(
            &mut script,
            "synthetic",
            "t0",
            "local dynamicRequire = require\n\
             return { attempt = function (target) return dynamicRequire(target) end }\n",
            &BTreeMap::new(),
        );
        script.push_str("return __lest_require('t0')\n");

        let lua = Lua::new();
        let native = lua
            .create_function(|_, value: Value| Ok(format!("native:{}", value.type_name())))
            .unwrap();
        lua.globals().set("require", native).unwrap();

        let module: Table = lua.load(&script).eval().expect("bundle chunk must load");
        let attempt: mlua::Function = module.get("attempt").unwrap();

        // A non-string argument reaches the captured native require.
        let delegated: String = attempt.call(lua.create_table().unwrap()).unwrap();
        assert_eq!(delegated, "native:table");

        // An unresolved *string* is still a loud bundler error, not a
        // fallback — and it names the requiring module, since the argument
        // value alone makes two call sites print identically.
        let err = attempt.call::<Value>("nope").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("lest cloud bundle: unresolved require(nope) in synthetic"),
            "string miss must stay a loud error naming the module, got: {message}"
        );
    }

    /// The exact shape from the field report: a require of a DataModel path is
    /// invisible to the static scan and must not block bundling — it resolves
    /// at runtime through the delegation path.
    #[test]
    fn dynamic_instance_requires_do_not_block_bundling() {
        let root = repo_root();
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("dynamic.spec.luau");
        std::fs::write(
            &spec,
            "local ServerStorage = game:GetService('ServerStorage')\n\
             local Bin = require(ServerStorage.ChiefTests.packages.bin.src)\n\
             return Bin\n",
        )
        .unwrap();

        let specs = vec![SpecEntry {
            name: "dynamic.spec".to_string(),
            path: spec,
        }];
        let input = BundleInput {
            core_entry: &core_entry(&root),
            specs: &specs,
            name_filter: None,
            deadline_ms: 1000,
        };
        let bundle = bundle(&input).expect("a dynamic instance require must not block bundling");
        assert!(
            bundle.script.contains("ChiefTests"),
            "the spec body must inline"
        );
        // Invisible to the static scan means unwarned too — a dynamic
        // instance require is the supported pattern, not a miss.
        assert_eq!(bundle.unresolved, vec![]);
    }

    /// The source map's core invariant, verified against the emitted script
    /// itself so the line accounting cannot drift from what
    /// `write_module_factory` really wrote: every span's bundle lines are
    /// exactly its module's source lines, for disk and embedded modules both.
    #[test]
    fn source_map_spans_reproduce_each_module_verbatim() {
        let root = repo_root();
        let spec = root.join("tests/core/expect.spec.luau");
        let specs = vec![SpecEntry {
            name: "expect.spec".to_string(),
            path: spec,
        }];
        let input = BundleInput {
            core_entry: &core_entry(&root),
            specs: &specs,
            name_filter: None,
            deadline_ms: 1000,
        };
        let bundle = bundle(&input).unwrap();

        let lines: Vec<&str> = bundle.script.lines().collect();
        assert!(!bundle.source_map.spans.is_empty());
        for span in &bundle.source_map.spans {
            let source = match &span.file {
                Some(file) => std::fs::read_to_string(file).unwrap(),
                None => {
                    let embedded = EMBEDDED
                        .iter()
                        .find(|m| span.label.ends_with(m.name))
                        .expect("a file-less span must be an embedded module");
                    embedded.source.to_string()
                }
            };
            let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
            let mut src_lines = 0;
            for (offset, src_line) in source.lines().enumerate() {
                assert_eq!(
                    lines[span.start_line - 1 + offset],
                    src_line,
                    "span for {} drifted at offset {offset}",
                    span.label
                );
                src_lines += 1;
            }
            assert_eq!(span.line_count, src_lines, "line count for {}", span.label);
        }
    }

    #[test]
    fn source_map_rejects_scaffolding_lines() {
        let root = repo_root();
        let spec = root.join("tests/core/expect.spec.luau");
        let specs = vec![SpecEntry {
            name: "expect.spec".to_string(),
            path: spec,
        }];
        let input = BundleInput {
            core_entry: &core_entry(&root),
            specs: &specs,
            name_filter: None,
            deadline_ms: 1000,
        };
        let map = bundle(&input).unwrap().source_map;

        let span = &map.spans[0];
        // Body edges resolve to module-local lines…
        let (first, line) = map.resolve(span.start_line).unwrap();
        assert_eq!((first.label.as_str(), line), (span.label.as_str(), 1));
        let last = span.start_line + span.line_count - 1;
        assert_eq!(map.resolve(last).unwrap().1, span.line_count);
        // …while the factory scaffolding on either side belongs to no module:
        // the shim above the body, the `end` below it.
        assert!(map.resolve(span.start_line - 1).is_none());
        assert!(map.resolve(last + 1).is_none());
        // The prelude and out-of-range coordinates map nowhere.
        assert!(map.resolve(1).is_none());
        assert!(map.resolve(0).is_none());
        assert!(map.resolve(usize::MAX).is_none());
    }

    /// The other half of the field report: `require('src')` failed in the
    /// engine with neither the file nor the line. The bundler knows both at
    /// bundle time; it reports them so the CLI can warn before the upload.
    #[test]
    fn unresolvable_string_requires_are_reported_with_call_sites() {
        let root = repo_root();
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("bad.spec.luau");
        std::fs::write(
            &spec,
            "--!strict\n\
             local bare = require('src')\n\
             local missing = require('./missing')\n\
             local fs = require('@lune/fs')\n\
             return nil\n",
        )
        .unwrap();

        let specs = vec![SpecEntry {
            name: "bad.spec".to_string(),
            path: spec.clone(),
        }];
        let input = BundleInput {
            core_entry: &core_entry(&root),
            specs: &specs,
            name_filter: None,
            deadline_ms: 1000,
        };
        let bundle = bundle(&input).expect("unresolvable requires must not block bundling");

        let spec_key = normalize(&spec);
        let misses: Vec<(&str, usize)> = bundle
            .unresolved
            .iter()
            .filter(|miss| miss.file == spec_key)
            .map(|miss| (miss.spec.as_str(), miss.line))
            .collect();
        // The builtin is absent: legal dead code in a shared module, so it
        // earns no warning. The other two carry their call-site lines.
        assert_eq!(misses, vec![("src", 2), ("./missing", 3)]);
        // Reasons are one-line fragments a warning body can carry.
        for miss in &bundle.unresolved {
            assert!(!miss.reason.contains('\n'), "multi-line: {}", miss.reason);
        }
    }

    #[test]
    fn name_filter_is_embedded_when_present() {
        let root = repo_root();
        let spec = root.join("tests/core/expect.spec.luau");
        let specs = vec![SpecEntry {
            name: "expect.spec".to_string(),
            path: spec,
        }];
        let input = BundleInput {
            core_entry: &core_entry(&root),
            specs: &specs,
            name_filter: Some("adds numbers"),
            deadline_ms: 5000,
        };
        let script = bundle(&input).unwrap().script;
        assert!(script.contains("nameFilter = 'adds numbers'"));
        assert!(script.contains("deadlineMs = 5000"));
    }

    #[test]
    fn escaping_survives_quotes_in_filter() {
        assert_eq!(luau_escape("it's fine"), "it\\'s fine");
        assert_eq!(luau_escape("a\nb"), "a\\nb");
    }
}
