//! Require resolution for lest: given (requiring file, require argument),
//! produce a filesystem path or a terminal builtin.
//!
//! Phase 1 covers string requires: `./` and `../` relative paths with the
//! Luau extension and `init.luau` conventions, plus the terminal runtime
//! builtins `@lune/*` and `@lute/*`, which are never resolved to disk — the
//! native backend refuses them with a pointer to the right backend, and the
//! spawned backends pass them straight through to the real runtime.
//! `.luaurc` aliases and `@self` requires resolve here too. Rojo project
//! mapping (phase 4, [`VirtualDataModel`]) turns a `default.project.json` into
//! a bidirectional filesystem ↔ DataModel map for the cloud backend; the pesde
//! lockfile walk arrives in a later phase.
//!
//! # Cache-key case policy
//!
//! Module paths double as cache keys, so two spellings of the same file must
//! collapse to one key. On case-insensitive hosts (Windows, macOS) keys are
//! case-folded to lowercase, so `require("./Utils")` and `require("./utils")`
//! land on the same key as the on-disk `utils.luau`. See [`normalize`] and
//! [`cache_key_path`].
//!
//! # Why this module allows dead code
//!
//! Resolution is a self-contained unit with a complete public API, exercised
//! end to end by its own tests and kept importing nothing from the rest of the
//! crate — extracting it into its own crate stays a directory move plus a
//! manifest. Some of that API has no caller in the CLI *yet*
//! ([`VirtualDataModel`] waits on the rojo build path), and some exists for
//! callers rather than for us ([`cache_key`], [`content_hash`]). Trimming to
//! only what `main` reaches today would make the boundary a fiction, so the
//! lint is off here and nowhere else.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Whether the host filesystem treats paths case-insensitively. Cache keys are
/// case-folded on such hosts so differently-cased spellings share one key.
const CASE_INSENSITIVE: bool = cfg!(any(target_os = "windows", target_os = "macos"));

/// A spawned runtime whose builtin modules are terminal requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Lune,
    Lute,
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Runtime::Lune => write!(f, "lune"),
            Runtime::Lute => write!(f, "lute"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A module on disk, as a normalized (and, on case-insensitive hosts,
    /// case-folded) absolute path.
    File(PathBuf),
    /// A runtime builtin (`@lune/*`, `@lute/*`): terminal, never on disk.
    Builtin { runtime: Runtime, module: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Bare names like `require("foo")` — not valid Luau string requires.
    UnsupportedSpec {
        spec: String,
    },
    /// An `@` alias that is neither a runtime builtin, `@self`, nor defined
    /// in any `.luaurc` up the directory tree.
    UnknownAlias {
        spec: String,
    },
    /// `@self` used from a non-init module, or a bare `@self` (a module cannot
    /// require itself). `@self` is only meaningful as `@self/<module>` from an
    /// `init.luau`/`init.lua` reaching its own directory's siblings.
    InvalidSelf {
        spec: String,
    },
    /// A `.luaurc` on the lookup path could not be read or parsed.
    Luaurc {
        path: PathBuf,
        message: String,
    },
    /// A rojo project file (`*.project.json`) could not be read or parsed.
    Project {
        path: PathBuf,
        message: String,
    },
    NotFound {
        spec: String,
        tried: Vec<PathBuf>,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::UnsupportedSpec { spec } => write!(
                f,
                "cannot resolve require(\"{spec}\"): require paths must start with \"./\", \"../\", or an \"@\" alias"
            ),
            ResolveError::UnknownAlias { spec } => write!(
                f,
                "cannot resolve require(\"{spec}\"): unknown alias — not a runtime builtin (@lune/*, @lute/*), not @self, and no `.luaurc` up the tree defines it"
            ),
            ResolveError::InvalidSelf { spec } => write!(
                f,
                "cannot resolve require(\"{spec}\"): `@self` is only valid as `@self/<module>` from within an init module"
            ),
            ResolveError::Luaurc { path, message } => write!(
                f,
                "cannot read aliases from {}: {message}",
                path.display()
            ),
            ResolveError::Project { path, message } => write!(
                f,
                "cannot read rojo project {}: {message}",
                path.display()
            ),
            ResolveError::NotFound { spec, tried } => {
                write!(f, "cannot resolve require(\"{spec}\"): no file at")?;
                for path in tried {
                    write!(f, "\n  {}", path.display())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Returns the runtime whose builtin namespace `spec` belongs to, if any.
pub fn builtin_runtime(spec: &str) -> Option<Runtime> {
    if spec == "@lune" || spec.starts_with("@lune/") {
        Some(Runtime::Lune)
    } else if spec == "@lute" || spec.starts_with("@lute/") {
        Some(Runtime::Lute)
    } else {
        None
    }
}

/// Resolves a require argument relative to the file containing the require.
/// `requiring_file` must be an absolute path.
///
/// This is the uncached convenience form: each call builds (and discards) a
/// fresh [`Resolver`], so every `@alias` require re-walks and re-parses
/// `.luaurc` files. Callers that resolve many requires against one filesystem
/// state — a VM's `require`, a graph build, a bundle — should hold a
/// [`Resolver`] for that lifetime instead.
pub fn resolve(requiring_file: &Path, spec: &str) -> Result<Resolved, ResolveError> {
    Resolver::new().resolve(requiring_file, spec)
}

/// The parsed alias table of one `.luaurc`: alias names lowercased for the
/// RFC's case-insensitive matching. `None` means the directory has no
/// `.luaurc`; a parse failure is memoized too, so a broken file is read once
/// and reported on every resolution that reaches it.
type LuaurcAliases = Option<Result<HashMap<String, String>, ResolveError>>;

/// Memoized resolution state, scoped to one "run" of resolution work: a native
/// VM's lifetime, one dependency-graph build, one cloud bundle.
///
/// Without it, every `@alias` require performs an upward directory walk with a
/// per-level `.luaurc` probe plus a full read-and-parse of each config it finds
/// — file IO multiplied by requires and directory depth on every watch
/// rebuild, against a sub-500ms watch target. The cache is deliberately *not*
/// process-global: it holds no invalidation hook, so its correctness comes
/// from being discarded with its owner (watch mode already re-runs — with
/// fresh resolvers — when a `.luaurc` changes).
#[derive(Debug, Default)]
pub struct Resolver {
    /// Normalized directory → its `.luaurc` alias table (see [`LuaurcAliases`]).
    luaurc: RefCell<HashMap<PathBuf, LuaurcAliases>>,
    /// Queried path → its canonical cache key, memoizing the
    /// `fs::canonicalize` syscall behind [`cache_key_path`].
    canonical: RefCell<HashMap<PathBuf, PathBuf>>,
}

impl Resolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolves a require argument relative to the file containing the
    /// require, memoizing `.luaurc` lookups. `requiring_file` must be an
    /// absolute path.
    pub fn resolve(&self, requiring_file: &Path, spec: &str) -> Result<Resolved, ResolveError> {
        if let Some(runtime) = builtin_runtime(spec) {
            return Ok(Resolved::Builtin {
                runtime,
                module: spec.to_string(),
            });
        }

        // `@self` names the requiring module's own directory. It is only
        // meaningful from an init module reaching its siblings
        // (`@self/sibling`); anything else is an error rather than a
        // surprising self-cycle. See `resolve_self`.
        if spec == "@self" || spec.starts_with("@self/") {
            return resolve_self(requiring_file, spec);
        }

        if spec.starts_with('@') {
            return self.resolve_alias(requiring_file, spec);
        }

        if !(spec.starts_with("./") || spec.starts_with("../")) {
            return Err(ResolveError::UnsupportedSpec {
                spec: spec.to_string(),
            });
        }

        let base = requiring_file.parent().unwrap_or_else(|| Path::new("."));
        let target = normalize(&base.join(spec));
        resolve_target(spec, &target)
    }

    /// `.luaurc` aliases: walks up from the requiring file's directory; the
    /// nearest `.luaurc` defining the alias wins, and its value is a path
    /// relative to that `.luaurc`'s own directory (per the require-by-string
    /// RFC). Missing keys fall through to ancestor configs by continuing the
    /// walk. Alias names are matched case-insensitively (the RFC treats them
    /// so).
    fn resolve_alias(&self, requiring_file: &Path, spec: &str) -> Result<Resolved, ResolveError> {
        let body = &spec[1..];
        let (alias, rest) = match body.split_once('/') {
            Some((alias, rest)) => (alias, Some(rest)),
            None => (body, None),
        };

        let mut dir = requiring_file.parent();
        while let Some(current) = dir {
            if let Some(value) = self.alias_in(current, alias)? {
                let base = normalize(&current.join(&value));
                let target = match rest {
                    Some(rest) => normalize(&base.join(rest)),
                    None => base,
                };
                return resolve_target(spec, &target);
            }
            dir = current.parent();
        }
        Err(ResolveError::UnknownAlias {
            spec: spec.to_string(),
        })
    }

    /// Looks `alias` up in `dir`'s `.luaurc`, reading and parsing the file at
    /// most once per resolver lifetime. `Ok(None)` means "keep walking up":
    /// either no `.luaurc` here, or one that does not define the alias.
    fn alias_in(&self, dir: &Path, alias: &str) -> Result<Option<String>, ResolveError> {
        let key = normalize(dir);
        let mut cache = self.luaurc.borrow_mut();
        let entry = cache.entry(key).or_insert_with(|| {
            let luaurc = dir.join(".luaurc");
            luaurc.is_file().then(|| parse_luaurc_aliases(&luaurc))
        });
        match entry {
            None => Ok(None),
            Some(Err(err)) => Err(err.clone()),
            Some(Ok(aliases)) => Ok(aliases.get(&alias.to_lowercase()).cloned()),
        }
    }

    /// [`cache_key_path`] with the `fs::canonicalize` syscall memoized per
    /// resolver lifetime — graph builds and watch queries otherwise pay it
    /// once per edge.
    pub fn cache_key_path(&self, path: &Path) -> PathBuf {
        if let Some(known) = self.canonical.borrow().get(path) {
            return known.clone();
        }
        let key = cache_key_path(path);
        self.canonical
            .borrow_mut()
            .insert(path.to_path_buf(), key.clone());
        key
    }
}

/// `@self`: valid only as `@self/<module>` from an init module, resolving the
/// remainder against the init module's own directory. A bare `@self`, or any
/// `@self` from a non-init file, is a self-require and errors.
///
/// "Init module" here means `init.luau`/`init.lua` only. Rojo's extra forms
/// (`init.server.luau`, `init.client.luau`) are a place-*building* concept —
/// they promote a directory to a `Script`/`LocalScript` instance — and are not
/// modules a string require can reach, so they are deliberately not recognized
/// here. See [`INIT_SCRIPT_NAMES`], which is the rojo-side list.
fn resolve_self(requiring_file: &Path, spec: &str) -> Result<Resolved, ResolveError> {
    let is_init = requiring_file
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|stem| stem.eq_ignore_ascii_case("init"))
        .unwrap_or(false);
    if !is_init {
        return Err(ResolveError::InvalidSelf {
            spec: spec.to_string(),
        });
    }
    let Some(rest) = spec.strip_prefix("@self/") else {
        // Bare `@self` from an init module would require the init file itself.
        return Err(ResolveError::InvalidSelf {
            spec: spec.to_string(),
        });
    };
    let base = requiring_file.parent().unwrap_or_else(|| Path::new("."));
    let target = normalize(&base.join(rest));
    resolve_target(spec, &target)
}

/// Reads and parses one `.luaurc`'s alias table, lowercasing alias names for
/// the RFC's case-insensitive matching. Two keys that fold to the same name
/// keep the first (matching the pre-memoization "first match wins" scan).
fn parse_luaurc_aliases(path: &Path) -> Result<HashMap<String, String>, ResolveError> {
    let text = std::fs::read_to_string(path).map_err(|e| ResolveError::Luaurc {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let value: serde_json::Value =
        serde_json::from_str(&sanitize_luaurc(&text)).map_err(|e| ResolveError::Luaurc {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
    let mut aliases = HashMap::new();
    if let Some(map) = value.get("aliases").and_then(|aliases| aliases.as_object()) {
        for (name, target) in map {
            if let Some(target) = target.as_str() {
                aliases
                    .entry(name.to_lowercase())
                    .or_insert_with(|| target.to_string());
            }
        }
    }
    Ok(aliases)
}

/// Probes a resolved target path for an on-disk module. An explicit `.luau`/
/// `.lua` extension is accepted directly; otherwise the `init`/extension
/// candidates are tried in precedence order. Both the relative and alias
/// branches funnel through here so they treat explicit extensions alike, and
/// neither ever resolves a non-Luau file (`.json`, `.txt`) as a module.
fn resolve_target(spec: &str, target: &Path) -> Result<Resolved, ResolveError> {
    if let Some(ext) = target.extension().and_then(|e| e.to_str()) {
        if ext.eq_ignore_ascii_case("luau") || ext.eq_ignore_ascii_case("lua") {
            if target.is_file() {
                return Ok(Resolved::File(normalize(target)));
            }
            // An explicit Luau extension names exactly one file, so a miss is
            // final. Falling through to `candidates` would append *another*
            // extension and report `mod.luau.luau` / `mod.luau/init.luau` while
            // omitting the path the user actually typed — the least useful
            // possible error for the require they are debugging.
            return Err(ResolveError::NotFound {
                spec: spec.to_string(),
                tried: vec![target.to_path_buf()],
            });
        }
    }
    let mut tried = Vec::new();
    for candidate in candidates(target) {
        if candidate.is_file() {
            return Ok(Resolved::File(normalize(&candidate)));
        }
        tried.push(candidate);
    }
    Err(ResolveError::NotFound {
        spec: spec.to_string(),
        tried,
    })
}

/// Luau's `.luaurc` parser is lenient: it allows `//` and `/* */` comments and
/// trailing commas that strict JSON rejects. This makes such a file parseable
/// by serde_json by stripping both.
///
/// Public because the CLI needs the same leniency when it edits a `.luaurc`
/// (`lest init`'s alias question) — and because a result that differs from the
/// input is exactly how it detects the comments it must not clobber.
pub fn sanitize_luaurc(text: &str) -> String {
    strip_trailing_commas(&strip_json_comments(text))
}

/// Removes `//` and `/* */` comments outside of strings.
///
/// Decoding is per-`char`, never per-byte: `bytes[i] as char` would reinterpret
/// each byte of a multi-byte UTF-8 sequence as a Latin-1 code point, so `é`
/// came back as `Ã©`. That corrupts alias paths, mojibakes rojo project files,
/// and — because `lest init` detects comments by asking whether sanitizing
/// changed the text — made any comment-free `.luaurc` holding one non-ASCII
/// character (or a BOM) look like it had comments. Byte-wise scanning is kept
/// only where the byte is necessarily ASCII (`/`, `*`, `\n`), which cannot
/// appear inside a multi-byte sequence.
fn strip_json_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let c = text[i..].chars().next().expect("i is a char boundary");
        let width = c.len_utf8();
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = text[i + width..].chars().next() {
                    out.push(next);
                    i += width + next.len_utf8();
                    continue;
                }
            }
            if c == '"' {
                in_string = false;
            }
            i += width;
        } else if c == '"' {
            in_string = true;
            out.push(c);
            i += width;
        } else if c == '/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else {
            out.push(c);
            i += width;
        }
    }
    out
}

/// Removes commas that immediately precede a `}` or `]` (ignoring whitespace),
/// outside of strings — the trailing commas Luau tolerates but JSON does not.
///
/// Per-`char` for the same reason as [`strip_json_comments`]: a byte-wise
/// `as char` mangles every non-ASCII character it copies through.
fn strip_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let c = text[i..].chars().next().expect("i is a char boundary");
        let width = c.len_utf8();
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = text[i + width..].chars().next() {
                    out.push(next);
                    i += width + next.len_utf8();
                    continue;
                }
            }
            if c == '"' {
                in_string = false;
            }
            i += width;
        } else if c == '"' {
            in_string = true;
            out.push(c);
            i += width;
        } else if c == ',' {
            let mut j = i + 1;
            while let Some(next) = text[j..].chars().next() {
                if !next.is_whitespace() {
                    break;
                }
                j += next.len_utf8();
            }
            if matches!(bytes.get(j), Some(b'}') | Some(b']')) {
                // Trailing comma: drop it.
                i += 1;
            } else {
                out.push(c);
                i += 1;
            }
        } else {
            out.push(c);
            i += width;
        }
    }
    out
}

/// Candidate order defines precedence: a sibling file beats a directory's
/// init module, and `.luau` beats `.lua`.
fn candidates(target: &Path) -> [PathBuf; 4] {
    [
        append_extension(target, "luau"),
        append_extension(target, "lua"),
        target.join("init.luau"),
        target.join("init.lua"),
    ]
}

/// The filenames rojo treats as a directory's own script — an `init.*` file
/// promotes its directory from a Folder to a script instance. Order is
/// precedence: `.luau` over `.lua`, plain module over server/client.
///
/// Used by the rojo directory walker ([`init_script_in_dir`]) only. String
/// requires deliberately recognize a *narrower* set: [`candidates`] probes just
/// `init.luau`/`init.lua`, because `init.server.luau`/`init.client.luau`
/// describe how a directory becomes a `Script`/`LocalScript` in a built place,
/// not a module another file can require. The two lists agree on the overlap
/// and are intentionally not shared.
const INIT_SCRIPT_NAMES: [&str; 6] = [
    "init.luau",
    "init.lua",
    "init.server.luau",
    "init.server.lua",
    "init.client.luau",
    "init.client.lua",
];

/// Appends an extension without clobbering dots already in the file name
/// (`./foo.config` must probe `foo.config.luau`, not `foo.luau`).
fn append_extension(path: &Path, ext: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".");
    os.push(ext);
    PathBuf::from(os)
}

/// Extracts the string literal from every `require("...")` in a source file.
/// Purely lexical — comments and dead code produce extra edges, dynamic
/// require arguments produce none — which errs on the side of re-running
/// more tests, exactly the right failure mode for watch mode. Also matches
/// the parenthesis-free `require "foo"` call form.
pub fn scan_requires(source: &str) -> Vec<String> {
    scan_requires_spanned(source)
        .into_iter()
        .map(|found| found.spec)
        .collect()
}

/// A require literal found by [`scan_requires_spanned`]: the spec string and
/// the 1-based line of the `require` keyword — the call site, so a literal
/// wrapped onto the next line still reports the line a reader would look at.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScannedRequire {
    pub spec: String,
    pub line: usize,
}

/// [`scan_requires`] with source positions, for callers that turn a require
/// into a diagnostic (the cloud bundler warns on unresolvable literals) —
/// resolution itself never needs them. Same lexical scan, same caveats.
pub fn scan_requires_spanned(source: &str) -> Vec<ScannedRequire> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut search_from = 0;
    // Match positions strictly increase, so the line of each is counted
    // incrementally from the previous match rather than from the top.
    let mut line = 1;
    let mut counted_upto = 0;
    while let Some(found) = source[search_from..].find("require") {
        let start = search_from + found;
        search_from = start + "require".len();

        if start > 0 {
            // Decode the preceding *character*, not the preceding byte: a raw
            // `as char` turns a UTF-8 lead/continuation byte into a Latin-1
            // letter, so `require` abutting any non-ASCII character looked like
            // an identifier suffix and its edge was dropped — under-selection,
            // the wrong direction for watch mode.
            if let Some(before) = source[..start].chars().next_back() {
                if before.is_alphanumeric() || before == '_' || before == '.' || before == ':' {
                    continue;
                }
            }
        }

        let mut i = search_from;
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'(' {
            i += 1;
            while i < bytes.len() && (bytes[i] as char).is_whitespace() {
                i += 1;
            }
        }
        if i >= bytes.len() {
            break;
        }
        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            continue;
        }
        i += 1;
        let literal_start = i;
        while i < bytes.len() && bytes[i] != quote && bytes[i] != b'\n' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == quote {
            line += bytes[counted_upto..start]
                .iter()
                .filter(|&&b| b == b'\n')
                .count();
            counted_upto = start;
            out.push(ScannedRequire {
                spec: source[literal_start..i].to_string(),
                line,
            });
            search_from = i + 1;
        }
    }
    out
}

/// Every file reachable from `entry` through resolvable string requires,
/// including `entry` itself. Builtins and unresolvable requires are ignored
/// (the spawned runtimes resolve their own world). This is the forward
/// closure; for watch-mode selection over many specs prefer
/// [`DependencyGraph`], which precomputes the inverse.
pub fn dependency_closure(entry: &Path) -> HashSet<PathBuf> {
    dependency_closure_all([entry])
}

/// The union of every entry's [`dependency_closure`], walked with one shared
/// visited set. Calling `dependency_closure` per entry re-reads and re-scans
/// every shared dependency once per entry; this reads each file exactly once,
/// which is what a caller enumerating a whole suite's sources wants.
///
/// Keys are [`normalize`]d, not [`cache_key_path`]ed — unlike
/// [`DependencyGraph`], whose canonicalized nodes are for identity comparison
/// rather than display. Callers that render these paths depend on that.
pub fn dependency_closure_all<I, P>(entries: I) -> HashSet<PathBuf>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    // One resolver for the whole walk, so shared `.luaurc` files are parsed
    // once instead of once per alias require encountered.
    let resolver = Resolver::new();
    let mut seen = HashSet::new();
    let mut queue: Vec<PathBuf> = entries
        .into_iter()
        .map(|entry| normalize(entry.as_ref()))
        .collect();
    while let Some(file) = queue.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        for spec in scan_requires(&source) {
            if let Ok(Resolved::File(path)) = resolver.resolve(&file, &spec) {
                if !seen.contains(&path) {
                    queue.push(path);
                }
            }
        }
    }
    seen
}

/// Lexically normalizes a path: removes `.` components, folds `..` into their
/// parent, and (on case-insensitive hosts) lowercases each component so
/// differently-cased spellings share one cache key. Purely textual — it never
/// touches the filesystem — so under symlinks it is not guaranteed to match
/// the real path; use [`cache_key_path`] when a symlink-stable key is needed.
///
/// A `..` that would escape an absolute path's root is dropped (clamped at the
/// root) rather than emitted, so a malformed `C:\..\b` never becomes a key.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut poppable = 0usize;
    let mut has_root = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) => out.push(fold_os(component.as_os_str())),
            Component::RootDir => {
                has_root = true;
                out.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::Normal(part) => {
                out.push(fold_os(part));
                poppable += 1;
            }
            Component::ParentDir => {
                if poppable > 0 {
                    out.pop();
                    poppable -= 1;
                } else if has_root {
                    // `..` above an absolute root is meaningless; clamp at root
                    // rather than emit a malformed `..`-above-root key.
                } else {
                    // `..` above a relative path's start is kept.
                    out.push("..");
                }
            }
        }
    }
    out
}

/// Case-folds a path component on case-insensitive hosts; a no-op elsewhere.
fn fold_os(part: &std::ffi::OsStr) -> std::ffi::OsString {
    if CASE_INSENSITIVE {
        std::ffi::OsString::from(part.to_string_lossy().to_lowercase())
    } else {
        part.to_os_string()
    }
}

/// Recovers a path's on-disk spelling on case-insensitive hosts by matching
/// each component case-insensitively against its parent's directory listing.
///
/// Identity and attribution are separate concerns: [`normalize`]'s case-folded
/// keys exist so two spellings of one file share a cache slot, but anything
/// *shown* to a user or matched against case-sensitive patterns — coverage
/// display paths, `[coverage] exclude` globs like `Packages/**` — needs the
/// file's actual spelling, and a folded key leaking into those silently stops
/// the globs matching. A component with no listing match (a deleted file, an
/// unreadable directory, an 8.3 short name) keeps the spelling it was given;
/// on case-sensitive hosts the path already *is* its spelling and is returned
/// unchanged.
pub fn on_disk_spelling(path: &Path) -> PathBuf {
    if !CASE_INSENSITIVE {
        return path.to_path_buf();
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let wanted = name.to_string_lossy().to_lowercase();
                let actual = std::fs::read_dir(&out).ok().and_then(|entries| {
                    entries
                        .filter_map(|entry| entry.ok())
                        .map(|entry| entry.file_name())
                        .find(|entry| entry.to_string_lossy().to_lowercase() == wanted)
                });
                match actual {
                    Some(actual) => out.push(actual),
                    None => out.push(name),
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The canonical cache key for a module path: symlink-resolved where the file
/// exists on disk (via [`std::fs::canonicalize`]), then [`normalize`]d and
/// case-folded. Where the path does not exist, falls back to lexical
/// normalization — under symlinks such a key is only guaranteed canonical when
/// the file is present. This is the key [`DependencyGraph`] uses for nodes.
pub fn cache_key_path(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(real) => normalize(&real),
        Err(_) => normalize(path),
    }
}

/// Deterministic 64-bit FNV-1a hash of `bytes`. Stable across runs and
/// platforms: identical input always yields the same value.
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Content hash of the file at `path` (FNV-1a over its bytes). Lets a
/// saved-but-unchanged file be told apart from a real edit: same content
/// always yields the same hash, so an invalidation can compare hashes instead
/// of trusting the filesystem's mtime.
pub fn content_hash(path: &Path) -> std::io::Result<u64> {
    let bytes = std::fs::read(path)?;
    Ok(hash_bytes(&bytes))
}

/// Cache key for a module: its [`cache_key_path`] paired with a
/// [`content_hash`] of the file's current bytes. A stored `(path, hash)` that
/// still matches means the file is unchanged; a differing hash means a real
/// edit even if the path is identical.
pub fn cache_key(path: &Path) -> std::io::Result<(PathBuf, u64)> {
    Ok((cache_key_path(path), content_hash(path)?))
}

/// A precomputed require graph over a set of spec files and their transitive
/// dependencies. Holds both forward edges (file → the files it requires) and
/// the inverted map (file → the files that directly require it), so
/// watch-mode / `--changed` selection is a walk of the precomputed inverse
/// rather than an O(specs × files) re-closure per query.
///
/// Nodes are keyed by [`cache_key_path`]. Runtime builtins (`@lune/*`,
/// `@lute/*`) are terminal and never appear as nodes. Cycles are handled
/// safely at both build and query time.
#[derive(Debug, Clone)]
pub struct DependencyGraph {
    root: PathBuf,
    /// file → the files it directly requires.
    forward: HashMap<PathBuf, HashSet<PathBuf>>,
    /// file → the files that directly require it (the inverted edges).
    reverse: HashMap<PathBuf, HashSet<PathBuf>>,
    /// the spec files this graph was built from (a subset of the nodes).
    specs: HashSet<PathBuf>,
}

impl DependencyGraph {
    /// Builds the graph by scanning each spec file's requires, resolving them,
    /// and following resolved file requires transitively. `root` is the project
    /// root (retained for context and future rojo/pesde resolution). Files that
    /// cannot be read, builtins, and unresolvable requires are skipped.
    pub fn build<I, P>(root: &Path, spec_files: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        // One resolver for the whole build: `.luaurc` parses and
        // `fs::canonicalize` calls are per-edge costs otherwise, and a graph
        // rebuild is on the watch-mode hot path.
        let resolver = Resolver::new();
        let mut forward: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        let mut reverse: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        let mut specs: HashSet<PathBuf> = HashSet::new();
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<PathBuf> = VecDeque::new();

        for spec in spec_files {
            let key = resolver.cache_key_path(spec.as_ref());
            specs.insert(key.clone());
            queue.push_back(key);
        }

        while let Some(file) = queue.pop_front() {
            if !visited.insert(file.clone()) {
                // Already expanded — cycle-safe.
                continue;
            }
            forward.entry(file.clone()).or_default();
            reverse.entry(file.clone()).or_default();

            let Ok(source) = std::fs::read_to_string(&file) else {
                continue;
            };
            for spec in scan_requires(&source) {
                if let Ok(Resolved::File(dep)) = resolver.resolve(&file, &spec) {
                    let dep_key = resolver.cache_key_path(&dep);
                    forward
                        .entry(file.clone())
                        .or_default()
                        .insert(dep_key.clone());
                    reverse
                        .entry(dep_key.clone())
                        .or_default()
                        .insert(file.clone());
                    if !visited.contains(&dep_key) {
                        queue.push_back(dep_key);
                    }
                }
            }
        }

        DependencyGraph {
            root: cache_key_path(root),
            forward,
            reverse,
            specs,
        }
    }

    /// The project root this graph was built for (as a cache-key path).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The spec files this graph was built from.
    pub fn specs(&self) -> &HashSet<PathBuf> {
        &self.specs
    }

    /// Whether `path` is a node in the graph.
    pub fn contains(&self, path: &Path) -> bool {
        self.forward.contains_key(&cache_key_path(path))
    }

    /// The files `path` directly requires, if it is a node.
    pub fn dependencies(&self, path: &Path) -> Option<&HashSet<PathBuf>> {
        self.forward.get(&cache_key_path(path))
    }

    /// The files that directly require `path`, if it is a node.
    pub fn direct_dependents(&self, path: &Path) -> Option<&HashSet<PathBuf>> {
        self.reverse.get(&cache_key_path(path))
    }

    /// Given a set of changed files, returns the spec files transitively
    /// affected — every spec that requires a changed file (at any depth), plus
    /// any changed file that is itself a spec. This powers both watch-mode
    /// re-run selection and `--changed`. Walks only the precomputed inverse
    /// edges reachable from the changes; cycles are handled via a visited set.
    pub fn affected_specs<I, P>(&self, changed: I) -> HashSet<PathBuf>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut affected: HashSet<PathBuf> = HashSet::new();
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<PathBuf> = VecDeque::new();

        for change in changed {
            queue.push_back(cache_key_path(change.as_ref()));
        }

        while let Some(file) = queue.pop_front() {
            if !visited.insert(file.clone()) {
                continue;
            }
            if self.specs.contains(&file) {
                affected.insert(file.clone());
            }
            if let Some(dependents) = self.reverse.get(&file) {
                for dependent in dependents {
                    if !visited.contains(dependent) {
                        queue.push_back(dependent.clone());
                    }
                }
            }
        }
        affected
    }
}

// ── Rojo project mapping (phase 4) ───────────────────────────────────────────
//
// A `default.project.json` describes how the filesystem maps into a Roblox
// DataModel tree. The cloud backend needs both directions: filesystem → the
// DataModel path a source file will occupy in the built place, and DataModel
// path → the file that backs an instance (for locating a module an
// instance-based require names). `VirtualDataModel` parses the project file,
// walks every `$path` directory once, and stores the resulting nodes with two
// lookup indexes so both directions are O(1).

/// One instance in the [`VirtualDataModel`]: where it lives in the DataModel,
/// its Roblox class, and the filesystem entry it is sourced from (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataModelNode {
    /// DataModel path segments below the root `game`/`DataModel`, e.g.
    /// `["ReplicatedStorage", "Common", "Util"]`. The root itself is never a
    /// node; services are the shallowest nodes.
    pub path: Vec<String>,
    /// The Roblox class name: an explicit `$className`, a service class
    /// inferred from the service's own name, `Folder` for a plain directory, or
    /// the script class inferred from a file's extension (`ModuleScript`,
    /// `Script` for `*.server.*`, `LocalScript` for `*.client.*`).
    pub class_name: String,
    /// The filesystem entry backing this node: a script file for a module, the
    /// directory for a `Folder`, the `init.*` script for a directory promoted
    /// to a module, or `None` for a purely structural node (a service that only
    /// nests children). A missing `$path` target is still recorded so the
    /// intended mapping is visible.
    pub source: Option<PathBuf>,
}

impl DataModelNode {
    /// Whether this node is a Luau module on disk (a `.luau`/`.lua` source),
    /// i.e. something an instance-based `require` can resolve to.
    pub fn is_module(&self) -> bool {
        self.source.as_deref().is_some_and(is_script_file)
    }
}

/// A parsed rojo project file as a bidirectional filesystem ↔ DataModel map.
///
/// Build it with [`VirtualDataModel::from_project_file`] (reads
/// `default.project.json` from disk) or [`VirtualDataModel::from_json`] (parses
/// an in-memory string against a project root). Then query either direction:
/// [`datamodel_path`](Self::datamodel_path) maps a source file to its instance
/// path, [`filesystem_path`](Self::filesystem_path) maps an instance path back
/// to its file, and [`pairs`](Self::pairs) enumerates every mapping the cloud
/// backend assembles into a place.
///
/// Filesystem keys are canonicalized through [`cache_key_path`] so a query path
/// and the walked path collapse to one key regardless of case or 8.3 short
/// names on Windows. DataModel segments are matched exactly (Roblox instance
/// names are case-sensitive); use [`parse_datamodel_path`] to turn a
/// `"game.ReplicatedStorage.Common"` string into segments.
#[derive(Debug, Clone)]
pub struct VirtualDataModel {
    name: String,
    project_root: PathBuf,
    nodes: Vec<DataModelNode>,
    /// canonical filesystem key → index into `nodes`.
    by_fs: HashMap<PathBuf, usize>,
    /// DataModel path segments → index into `nodes`.
    by_dm: HashMap<Vec<String>, usize>,
}

impl VirtualDataModel {
    /// Parses the rojo project file at `project_file` (typically
    /// `default.project.json`), resolving `$path` entries relative to the file's
    /// own directory. Errors are [`ResolveError::Project`] for a missing or
    /// malformed file — never a panic.
    pub fn from_project_file(project_file: &Path) -> Result<Self, ResolveError> {
        let text = std::fs::read_to_string(project_file).map_err(|e| ResolveError::Project {
            path: project_file.to_path_buf(),
            message: e.to_string(),
        })?;
        let root = project_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::from_json_at(&root, &text, project_file)
    }

    /// Parses project JSON held in memory, resolving `$path` entries relative to
    /// `project_root` (the directory the project file lives in). Useful for
    /// tests and for callers that already hold the file contents.
    pub fn from_json(project_root: &Path, json: &str) -> Result<Self, ResolveError> {
        Self::from_json_at(
            project_root,
            json,
            &project_root.join("default.project.json"),
        )
    }

    fn from_json_at(
        project_root: &Path,
        json: &str,
        error_path: &Path,
    ) -> Result<Self, ResolveError> {
        // Rojo project files are strict JSON, but tolerate the same comment /
        // trailing-comma leniency `.luaurc` gets — harmless and consistent.
        let value: serde_json::Value =
            serde_json::from_str(&sanitize_luaurc(json)).map_err(|e| ResolveError::Project {
                path: error_path.to_path_buf(),
                message: e.to_string(),
            })?;

        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        let mut nodes = Vec::new();
        if let Some(tree) = value.get("tree") {
            // A missing `tree` yields an empty model rather than an error.
            walk_tree(tree, Vec::new(), project_root, &mut nodes)?;
        }

        let mut by_fs = HashMap::new();
        let mut by_dm = HashMap::new();
        for (index, node) in nodes.iter().enumerate() {
            if let Some(source) = &node.source {
                // Last writer wins on a collision (e.g. `foo.luau` beside a
                // `foo/` directory) — rojo would flag it; we stay total.
                by_fs.insert(cache_key_path(source), index);
            }
            by_dm.insert(node.path.clone(), index);
        }

        Ok(VirtualDataModel {
            name,
            project_root: cache_key_path(project_root),
            nodes,
            by_fs,
            by_dm,
        })
    }

    /// The project's `name` field (empty string if absent).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The directory `$path` entries were resolved against (the project file's
    /// directory), as a canonical [`cache_key_path`].
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Every mapped instance, in walk order (services first, then their
    /// descendants depth-first).
    pub fn nodes(&self) -> &[DataModelNode] {
        &self.nodes
    }

    /// The DataModel path a source file occupies, or `None` if the file is not
    /// covered by any `$path` mapping. Accepts any spelling of the path — it is
    /// canonicalized to the same key the walk stored.
    pub fn datamodel_path(&self, fs_path: &Path) -> Option<&[String]> {
        self.by_fs
            .get(&cache_key_path(fs_path))
            .map(|&i| self.nodes[i].path.as_slice())
    }

    /// The filesystem entry backing a DataModel path, or `None` if no instance
    /// lives there or it is a structural node with no source. Segments are
    /// matched exactly.
    pub fn filesystem_path<S: AsRef<str>>(&self, dm_path: &[S]) -> Option<&Path> {
        let key: Vec<String> = dm_path.iter().map(|s| s.as_ref().to_string()).collect();
        self.by_dm
            .get(&key)
            .and_then(|&i| self.nodes[i].source.as_deref())
    }

    /// The node at a DataModel path, if one exists there.
    pub fn node_at<S: AsRef<str>>(&self, dm_path: &[S]) -> Option<&DataModelNode> {
        let key: Vec<String> = dm_path.iter().map(|s| s.as_ref().to_string()).collect();
        self.by_dm.get(&key).map(|&i| &self.nodes[i])
    }

    /// The node backed by a filesystem entry, if any.
    pub fn node_for_file(&self, fs_path: &Path) -> Option<&DataModelNode> {
        self.by_fs
            .get(&cache_key_path(fs_path))
            .map(|&i| &self.nodes[i])
    }

    /// Every `(datamodel_path, filesystem_path)` pair with a backing source, in
    /// node order. This is what the cloud backend walks to assemble the place —
    /// each file's destination instance and each folder's source directory.
    pub fn pairs(&self) -> impl Iterator<Item = (&[String], &Path)> {
        self.nodes.iter().filter_map(|node| {
            node.source
                .as_deref()
                .map(|source| (node.path.as_slice(), source))
        })
    }

    /// Resolves an instance-based require named by a DataModel path (e.g. the
    /// segments behind `require(game.ReplicatedStorage.Common.Util)`) to the
    /// module on disk. Mirrors [`resolve`]'s output ([`Resolved::File`]) so
    /// callers merge project and string requires into one graph. A path with no
    /// node, or one whose node is a folder rather than a module, is
    /// [`ResolveError::NotFound`].
    pub fn resolve<S: AsRef<str>>(&self, dm_path: &[S]) -> Result<Resolved, ResolveError> {
        let key: Vec<String> = dm_path.iter().map(|s| s.as_ref().to_string()).collect();
        let spec = key.join(".");
        match self.by_dm.get(&key).map(|&i| &self.nodes[i]) {
            Some(node) if node.is_module() => {
                // `is_module` guarantees a script source.
                Ok(Resolved::File(normalize(node.source.as_ref().unwrap())))
            }
            Some(node) => Err(ResolveError::NotFound {
                spec,
                tried: node.source.clone().into_iter().collect(),
            }),
            None => Err(ResolveError::NotFound {
                spec,
                tried: Vec::new(),
            }),
        }
    }
}

/// Splits a DataModel path string into segments, dropping a leading `game` or
/// `DataModel` root and empty parts. Accepts both `.` and `/` separators, so
/// `"game.ReplicatedStorage.Common"` and `"ReplicatedStorage/Common"` both
/// yield `["ReplicatedStorage", "Common"]`. The result feeds
/// [`VirtualDataModel::filesystem_path`], [`node_at`](VirtualDataModel::node_at),
/// and [`resolve`](VirtualDataModel::resolve).
pub fn parse_datamodel_path(path: &str) -> Vec<String> {
    let mut segments: Vec<String> = path
        .split(['.', '/'])
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    if let Some(first) = segments.first() {
        if first.eq_ignore_ascii_case("game") || first.eq_ignore_ascii_case("DataModel") {
            segments.remove(0);
        }
    }
    segments
}

/// Recursively turns a project `tree` node into [`DataModelNode`]s. `segments`
/// is the DataModel path to `value` (empty at the root, which is never emitted).
/// Handles `$className`, `$path` (file vs directory vs missing), and structural
/// service/folder nodes, then recurses explicit child keys. A `$path` naming a
/// nested `*.project.json` is an error: lest does not compose sub-projects, and
/// silently mapping the project file itself as a `ModuleScript` was a wrong
/// node nothing would ever flag.
fn walk_tree(
    value: &serde_json::Value,
    segments: Vec<String>,
    project_root: &Path,
    out: &mut Vec<DataModelNode>,
) -> Result<(), ResolveError> {
    let Some(obj) = value.as_object() else {
        return Ok(());
    };
    let explicit_class = obj.get("$className").and_then(serde_json::Value::as_str);
    let path_val = obj.get("$path").and_then(serde_json::Value::as_str);
    let is_root = segments.is_empty();

    if let Some(rel) = path_val {
        let fs = normalize(&project_root.join(rel));
        if is_project_json(&fs) {
            return Err(ResolveError::Project {
                path: fs,
                message: "this `$path` names a nested rojo project file, which lest does not \
                          compose — point `$path` at the module or directory itself, or build \
                          the composed place with rojo first"
                    .to_string(),
            });
        }
        if fs.is_dir() {
            map_directory(&segments, &fs, explicit_class, out);
        } else if !is_root {
            // A file `$path`, or a `$path` that does not exist on disk: record
            // the mapping so it is still visible either way. A *missing*
            // target keeps the filename heuristic (a name is all there is to
            // go on); an existing non-script file gets the class rojo would
            // give it, or is skipped explicitly when its class would need
            // parsing the file (`.rbxm` and friends) — never a bogus
            // `ModuleScript`.
            let inferred = if is_script_file(&fs) || !fs.exists() {
                Some(class_from_filename(&fs))
            } else {
                class_for_data_file(&fs)
            };
            let class = explicit_class
                .map(str::to_string)
                .or_else(|| inferred.map(str::to_string));
            if let Some(class) = class {
                out.push(DataModelNode {
                    path: segments.clone(),
                    class_name: class,
                    source: Some(fs),
                });
            }
        }
    } else if !is_root {
        // No `$path`: a structural node (a service or an explicit folder). A
        // service's class equals its name when `$className` is omitted.
        let class = explicit_class
            .map(str::to_string)
            .unwrap_or_else(|| segments.last().cloned().unwrap_or_default());
        out.push(DataModelNode {
            path: segments.clone(),
            class_name: class,
            source: None,
        });
    }

    for (key, child) in obj {
        if key.starts_with('$') {
            continue;
        }
        let mut child_segments = segments.clone();
        child_segments.push(key.clone());
        walk_tree(child, child_segments, project_root, out)?;
    }
    Ok(())
}

/// Maps a `$path` directory into the node at `segments` plus its descendants,
/// applying rojo's conventions: an `init.*` script promotes the directory to
/// that script (its siblings become children); otherwise the directory is a
/// `Folder`. Nested directories recurse; sibling `.luau`/`.lua` files become
/// script modules; everything else (`.rbxm`, `.json`, `.meta.json`, …) is
/// skipped for module mapping.
fn map_directory(
    segments: &[String],
    dir: &Path,
    explicit_class: Option<&str>,
    out: &mut Vec<DataModelNode>,
) {
    let init = init_script_in_dir(dir);
    let (class, source) = match &init {
        Some(init_file) => (
            explicit_class
                .map(str::to_string)
                .unwrap_or_else(|| class_from_filename(init_file).to_string()),
            Some(init_file.clone()),
        ),
        None => (
            explicit_class.unwrap_or("Folder").to_string(),
            Some(dir.to_path_buf()),
        ),
    };
    if !segments.is_empty() {
        out.push(DataModelNode {
            path: segments.to_vec(),
            class_name: class,
            source,
        });
    }

    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(read) => read.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return,
    };
    // Deterministic order so node/`pairs` output is stable across platforms.
    entries.sort();

    for entry in entries {
        if init
            .as_ref()
            .is_some_and(|init_file| cache_key_path(&entry) == cache_key_path(init_file))
        {
            continue; // The init script is the directory's own module.
        }
        if is_meta_json(&entry) {
            continue; // `.meta.json` decorates a sibling; not its own instance.
        }
        if entry.is_dir() {
            let name = file_name_string(&entry);
            let mut child = segments.to_vec();
            child.push(name);
            map_directory(&child, &entry, None, out);
        } else if is_script_file(&entry) {
            let name = module_name(&entry);
            let mut child = segments.to_vec();
            child.push(name);
            out.push(DataModelNode {
                path: child,
                class_name: class_from_filename(&entry).to_string(),
                source: Some(entry),
            });
        }
        // Any other file kind is not a Luau module: skip it. That includes a
        // nested `*.project.json` rojo would compose — composing sub-projects
        // is out of scope, and skipping produces no node rather than a wrong
        // one (a `$path` aimed *directly* at a sub-project errors instead, in
        // `walk_tree`).
    }
}

/// The `init.*` script that promotes `dir` to a module, if present, in
/// precedence order.
fn init_script_in_dir(dir: &Path) -> Option<PathBuf> {
    INIT_SCRIPT_NAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The Roblox script class implied by a filename: `Script` for `*.server.*`,
/// `LocalScript` for `*.client.*`, else `ModuleScript`. Works for both
/// `init.*` files and plain modules.
fn class_from_filename(path: &Path) -> &'static str {
    let name = file_name_string(path).to_ascii_lowercase();
    if name.ends_with(".server.luau") || name.ends_with(".server.lua") {
        "Script"
    } else if name.ends_with(".client.luau") || name.ends_with(".client.lua") {
        "LocalScript"
    } else {
        "ModuleScript"
    }
}

/// The instance name for a script file: its filename with the Luau extension
/// and any `.server`/`.client` qualifier stripped, preserving the base's case.
fn module_name(path: &Path) -> String {
    let name = file_name_string(path);
    let lower = name.to_ascii_lowercase();
    for suffix in [
        ".server.luau",
        ".server.lua",
        ".client.luau",
        ".client.lua",
        ".luau",
        ".lua",
    ] {
        if lower.ends_with(suffix) {
            return name[..name.len() - suffix.len()].to_string();
        }
    }
    name
}

/// Whether `path` is a Luau source file (`.luau` or `.lua`).
fn is_script_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("luau") || ext.eq_ignore_ascii_case("lua"))
}

/// Whether `path` is a rojo `.meta.json` sidecar (decorates its sibling rather
/// than being its own instance).
fn is_meta_json(path: &Path) -> bool {
    file_name_string(path)
        .to_ascii_lowercase()
        .ends_with(".meta.json")
}

/// Whether `path` names a rojo project file (`*.project.json`) — a
/// sub-project, which lest does not compose.
fn is_project_json(path: &Path) -> bool {
    file_name_string(path)
        .to_ascii_lowercase()
        .ends_with(".project.json")
}

/// The class rojo assigns a non-script file mapped by `$path`, for the kinds
/// whose class the filename alone determines: JSON becomes a `ModuleScript`
/// returning the data, `.txt` a `StringValue`, `.csv` a `LocalizationTable`.
/// `None` for kinds whose instance class cannot be known without parsing the
/// file (`.rbxm`/`.rbxmx` models carry their own root class) — callers skip
/// those explicitly rather than mislabel them.
fn class_for_data_file(path: &Path) -> Option<&'static str> {
    let name = file_name_string(path).to_ascii_lowercase();
    if name.ends_with(".json") {
        Some("ModuleScript")
    } else if name.ends_with(".txt") {
        Some("StringValue")
    } else if name.ends_with(".csv") {
        Some("LocalizationTable")
    } else {
        None
    }
}

/// The final path component as an owned `String` (lossy on non-UTF-8), or the
/// empty string for a path with no filename.
fn file_name_string(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod project_tests;
#[cfg(test)]
mod tests;
