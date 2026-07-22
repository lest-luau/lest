use std::fs;
use std::path::{Path, PathBuf};

use super::{
    builtin_runtime, cache_key, cache_key_path, content_hash, hash_bytes, normalize, resolve,
    DependencyGraph, ResolveError, Resolved, Runtime,
};

/// Writes `contents` to `rel` under `dir`, creating parent directories.
fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, contents).unwrap();
}

/// Builds a file tree under a tempdir from (relative path, contents) pairs.
fn tree(files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for rel in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "return {}\n").unwrap();
    }
    dir
}

fn expect_file(result: Result<Resolved, ResolveError>) -> PathBuf {
    match result {
        Ok(Resolved::File(path)) => path,
        other => panic!("expected a file resolution, got {other:?}"),
    }
}

fn assert_resolves(root: &Path, from: &str, spec: &str, expected: &str) {
    let resolved = expect_file(resolve(&root.join(from), spec));
    assert_eq!(resolved, normalize(&root.join(expected)));
}

#[test]
fn resolves_sibling_module() {
    let dir = tree(&["a.luau", "b.luau"]);
    assert_resolves(dir.path(), "a.luau", "./b", "b.luau");
}

#[test]
fn resolves_parent_directory_module() {
    let dir = tree(&["shared.luau", "sub/mod.luau"]);
    assert_resolves(dir.path(), "sub/mod.luau", "../shared", "shared.luau");
}

#[test]
fn resolves_nested_path() {
    let dir = tree(&["main.luau", "sub/deep/mod.luau"]);
    assert_resolves(
        dir.path(),
        "main.luau",
        "./sub/deep/mod",
        "sub/deep/mod.luau",
    );
}

#[test]
fn resolves_directory_to_init_luau() {
    let dir = tree(&["main.luau", "pkg/init.luau"]);
    assert_resolves(dir.path(), "main.luau", "./pkg", "pkg/init.luau");
}

#[test]
fn falls_back_to_lua_extension() {
    let dir = tree(&["main.luau", "legacy.lua"]);
    assert_resolves(dir.path(), "main.luau", "./legacy", "legacy.lua");
}

#[test]
fn prefers_luau_over_lua() {
    let dir = tree(&["main.luau", "both.luau", "both.lua"]);
    assert_resolves(dir.path(), "main.luau", "./both", "both.luau");
}

#[test]
fn prefers_file_over_directory_init() {
    let dir = tree(&["main.luau", "pkg.luau", "pkg/init.luau"]);
    assert_resolves(dir.path(), "main.luau", "./pkg", "pkg.luau");
}

#[test]
fn keeps_dots_in_module_names() {
    let dir = tree(&["main.luau", "app.config.luau"]);
    assert_resolves(dir.path(), "main.luau", "./app.config", "app.config.luau");
}

#[test]
fn recognizes_runtime_builtins_as_terminal() {
    let dir = tree(&["main.luau"]);
    let from = dir.path().join("main.luau");
    assert_eq!(
        resolve(&from, "@lune/fs"),
        Ok(Resolved::Builtin {
            runtime: Runtime::Lune,
            module: "@lune/fs".into()
        })
    );
    assert_eq!(
        resolve(&from, "@lute/task"),
        Ok(Resolved::Builtin {
            runtime: Runtime::Lute,
            module: "@lute/task".into()
        })
    );
}

#[test]
fn resolves_self_alias_to_sibling_module() {
    // The canonical use: an init module reaching its own directory's files.
    let dir = tree(&["pkg/init.luau", "pkg/inner.luau"]);
    assert_resolves(dir.path(), "pkg/init.luau", "@self/inner", "pkg/inner.luau");
}

#[test]
fn rejects_unknown_alias() {
    let dir = tree(&["main.luau"]);
    let result = resolve(&dir.path().join("main.luau"), "@pkg/thing");
    assert_eq!(
        result,
        Err(ResolveError::UnknownAlias {
            spec: "@pkg/thing".into()
        })
    );
}

#[test]
fn rejects_bare_names() {
    let dir = tree(&["main.luau"]);
    let result = resolve(&dir.path().join("main.luau"), "somemodule");
    assert_eq!(
        result,
        Err(ResolveError::UnsupportedSpec {
            spec: "somemodule".into()
        })
    );
}

#[test]
fn not_found_reports_every_candidate() {
    let dir = tree(&["main.luau"]);
    match resolve(&dir.path().join("main.luau"), "./missing") {
        Err(ResolveError::NotFound { spec, tried }) => {
            assert_eq!(spec, "./missing");
            // The exact set, in precedence order — a bare length assertion
            // passes just as happily on a wrong candidate list.
            let base = normalize(&dir.path().join("missing"));
            assert_eq!(
                tried,
                vec![
                    base.with_extension("luau"),
                    base.with_extension("lua"),
                    base.join("init.luau"),
                    base.join("init.lua"),
                ]
            );
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn not_found_with_explicit_extension_reports_the_requested_path() {
    // An explicit `.luau` names one file. Reporting `mod.luau.luau` and
    // `mod.luau/init.luau` while omitting `mod.luau` is the least useful
    // possible error for the require being debugged.
    let dir = tree(&["main.luau"]);
    match resolve(&dir.path().join("main.luau"), "./mod.luau") {
        Err(ResolveError::NotFound { spec, tried }) => {
            assert_eq!(spec, "./mod.luau");
            assert_eq!(tried, vec![normalize(&dir.path().join("mod.luau"))]);
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn builtin_namespace_requires_separator() {
    // "@lunex/foo" is not the lune namespace.
    assert_eq!(builtin_runtime("@lunex/foo"), None);
    assert_eq!(builtin_runtime("@lune/fs"), Some(Runtime::Lune));
    assert_eq!(builtin_runtime("@lute"), Some(Runtime::Lute));
}

#[test]
fn resolves_luaurc_alias_from_nearest_config() {
    let dir = tree(&["libs/util.luau", "src/deep/mod.luau"]);
    std::fs::write(
        dir.path().join(".luaurc"),
        r#"
        {
            // Comments are legal in .luaurc.
            "aliases": { "util": "libs/util" }
        }
        "#,
    )
    .unwrap();
    assert_resolves(dir.path(), "src/deep/mod.luau", "@util", "libs/util.luau");
}

#[test]
fn luaurc_alias_supports_subpaths() {
    let dir = tree(&["libs/net/http.luau", "src/mod.luau"]);
    std::fs::write(
        dir.path().join(".luaurc"),
        r#"{ "aliases": { "libs": "libs" } }"#,
    )
    .unwrap();
    assert_resolves(
        dir.path(),
        "src/mod.luau",
        "@libs/net/http",
        "libs/net/http.luau",
    );
}

#[test]
fn nearer_luaurc_wins() {
    let dir = tree(&["a/dep.luau", "b/dep.luau", "b/src/mod.luau"]);
    std::fs::write(
        dir.path().join(".luaurc"),
        r#"{ "aliases": { "dep": "a/dep" } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b/.luaurc"),
        r#"{ "aliases": { "dep": "dep" } }"#,
    )
    .unwrap();
    assert_resolves(dir.path(), "b/src/mod.luau", "@dep", "b/dep.luau");
}

#[test]
fn scan_requires_finds_literals_only() {
    let source = r#"
        local a = require('./a')
        local b = require("../b/mod")
        local c = require("@lune/fs")
        -- require("./commented") still counts; over-matching is safe here
        local dynamic = require(somePath)
        local notrequire = xrequire("./nope")
    "#;
    let found = crate::resolve::scan_requires(source);
    assert_eq!(found, vec!["./a", "../b/mod", "@lune/fs", "./commented"]);
}

#[test]
fn dependency_closure_walks_transitive_requires() {
    let dir = tree(&["spec.luau", "a.luau", "b.luau", "unrelated.luau"]);
    std::fs::write(
        dir.path().join("spec.luau"),
        "local a = require('./a')\nreturn nil\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("a.luau"), "return { b = require('./b') }\n").unwrap();

    let closure = crate::resolve::dependency_closure(&dir.path().join("spec.luau"));
    assert!(closure.contains(&normalize(&dir.path().join("spec.luau"))));
    assert!(closure.contains(&normalize(&dir.path().join("a.luau"))));
    assert!(closure.contains(&normalize(&dir.path().join("b.luau"))));
    assert!(!closure.contains(&normalize(&dir.path().join("unrelated.luau"))));
}

#[test]
fn normalize_folds_dot_segments() {
    let dir = tree(&["shared.luau", "a/b/mod.luau"]);
    // A require that walks down and back up lands on the same cache key as a
    // direct sibling require.
    let via_detour = expect_file(resolve(&dir.path().join("a/b/mod.luau"), "../../shared"));
    let direct = normalize(&dir.path().join("shared.luau"));
    assert_eq!(via_detour, direct);
}

// ── audit fix: normalize clamps `..` above an absolute root ──────────────────

#[test]
fn normalize_clamps_parent_above_root() {
    // A malformed `..`-above-root must not survive into a cache key.
    let base = Path::new("/a");
    let normalized = normalize(&base.join("../../b"));
    // `..` past the root is dropped, never emitted as a literal component.
    assert!(
        !normalized.components().any(|c| c.as_os_str() == ".."),
        "normalize left a `..` above root in {normalized:?}"
    );
}

// ── audit fix #3/#4: case-insensitive cache keys ─────────────────────────────

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[test]
fn resolve_folds_case_for_cache_keys() {
    // Two differently-cased requires of the same on-disk file must produce the
    // same key so a watcher change to canonical `utils.luau` matches both.
    let dir = tree(&["Utils.luau", "main.luau"]);
    let upper = expect_file(resolve(&dir.path().join("main.luau"), "./Utils"));
    let lower = expect_file(resolve(&dir.path().join("main.luau"), "./utils"));
    // Both spellings collapse to one lexical (case-folded) key.
    assert_eq!(upper, lower);
    assert_eq!(upper, normalize(&dir.path().join("Utils.luau")));
    // And the symlink-stable key agrees regardless of the require's casing.
    assert_eq!(
        cache_key_path(&dir.path().join("Utils.luau")),
        cache_key_path(&dir.path().join("utils.luau")),
    );
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[test]
fn normalize_is_case_folded() {
    assert_eq!(
        normalize(Path::new("/Foo/Bar")),
        normalize(Path::new("/foo/bar"))
    );
}

// ── audit fix #5: case-insensitive alias names ───────────────────────────────

#[test]
fn luaurc_alias_names_are_case_insensitive() {
    let dir = tree(&["vendor/roact.luau", "src/mod.luau"]);
    write(
        dir.path(),
        ".luaurc",
        r#"{ "aliases": { "Roact": "vendor/roact" } }"#,
    );
    // `.luaurc` defines `Roact`; `require("@roact")` must still resolve.
    assert_resolves(dir.path(), "src/mod.luau", "@roact", "vendor/roact.luau");
}

// ── audit fix: .luaurc parses are memoized per resolver lifetime ─────────────

#[test]
fn resolver_reads_each_luaurc_at_most_once() {
    let dir = tree(&["libs/util.luau", "libs/other.luau", "src/mod.luau"]);
    write(
        dir.path(),
        ".luaurc",
        r#"{ "aliases": { "util": "libs/util" } }"#,
    );
    let resolver = super::Resolver::new();
    let from = dir.path().join("src/mod.luau");
    let first = expect_file(resolver.resolve(&from, "@util"));
    assert_eq!(first, normalize(&dir.path().join("libs/util.luau")));

    // Redefine the alias on disk. The live resolver must keep its memoized
    // parse — its correctness contract is "discarded with its owner", and a
    // re-read here would mean every alias require pays read+parse again.
    write(
        dir.path(),
        ".luaurc",
        r#"{ "aliases": { "util": "libs/other" } }"#,
    );
    let second = expect_file(resolver.resolve(&from, "@util"));
    assert_eq!(second, first, "one .luaurc parse per resolver lifetime");

    // A fresh resolver — a new run — sees the edit.
    let fresh = expect_file(super::Resolver::new().resolve(&from, "@util"));
    assert_eq!(fresh, normalize(&dir.path().join("libs/other.luau")));
}

// ── audit fix #6: trailing commas in .luaurc ─────────────────────────────────

#[test]
fn luaurc_tolerates_trailing_commas() {
    let dir = tree(&["libs/util.luau", "src/mod.luau"]);
    write(
        dir.path(),
        ".luaurc",
        "{\n  \"aliases\": {\n    \"util\": \"libs/util\",\n  },\n}\n",
    );
    assert_resolves(dir.path(), "src/mod.luau", "@util", "libs/util.luau");
}

#[test]
fn luaurc_tolerates_block_comments() {
    let dir = tree(&["libs/util.luau", "src/mod.luau"]);
    write(
        dir.path(),
        ".luaurc",
        "{ /* block comment */ \"aliases\": { \"util\": \"libs/util\" } }",
    );
    assert_resolves(dir.path(), "src/mod.luau", "@util", "libs/util.luau");
}

#[test]
fn malformed_luaurc_reports_error() {
    let dir = tree(&["main.luau"]);
    write(dir.path(), ".luaurc", "{ this is not json ]");
    match resolve(&dir.path().join("main.luau"), "@util") {
        Err(ResolveError::Luaurc { .. }) => {}
        other => panic!("expected a Luaurc parse error, got {other:?}"),
    }
}

// ── audit fix: missing alias key falls through to an ancestor .luaurc ─────────

#[test]
fn missing_alias_key_falls_through_to_ancestor() {
    let dir = tree(&["libs/util.luau", "src/deep/mod.luau"]);
    // The nearer `.luaurc` defines a different alias; lookup must continue up.
    write(
        dir.path(),
        "src/.luaurc",
        r#"{ "aliases": { "other": "nope" } }"#,
    );
    write(
        dir.path(),
        ".luaurc",
        r#"{ "aliases": { "util": "libs/util" } }"#,
    );
    assert_resolves(dir.path(), "src/deep/mod.luau", "@util", "libs/util.luau");
}

// ── audit fix #7: alias branch only accepts .luau/.lua explicit files ────────

#[test]
fn alias_does_not_resolve_non_luau_file() {
    let dir = tree(&["main.luau"]);
    write(dir.path(), "data/config.json", "{}");
    write(
        dir.path(),
        ".luaurc",
        r#"{ "aliases": { "cfg": "data/config.json" } }"#,
    );
    // A `.json` target must not be accepted as a module.
    match resolve(&dir.path().join("main.luau"), "@cfg") {
        Err(ResolveError::NotFound { .. }) => {}
        other => panic!("expected NotFound for a .json alias target, got {other:?}"),
    }
}

#[test]
fn explicit_luau_extension_resolves_directly() {
    // The relative and alias branches agree on an explicit `.luau` extension.
    let dir = tree(&["main.luau", "mod.luau"]);
    assert_resolves(dir.path(), "main.luau", "./mod.luau", "mod.luau");
    write(
        dir.path(),
        ".luaurc",
        r#"{ "aliases": { "m": "mod.luau" } }"#,
    );
    assert_resolves(dir.path(), "main.luau", "@m", "mod.luau");
}

// ── audit fix #8: @self semantics ────────────────────────────────────────────

#[test]
fn self_subpath_from_init_resolves_sibling() {
    let dir = tree(&["pkg/init.luau", "pkg/inner.luau"]);
    assert_resolves(dir.path(), "pkg/init.luau", "@self/inner", "pkg/inner.luau");
}

#[test]
fn bare_self_from_init_is_invalid() {
    let dir = tree(&["pkg/init.luau"]);
    // A module cannot require itself.
    assert_eq!(
        resolve(&dir.path().join("pkg/init.luau"), "@self"),
        Err(ResolveError::InvalidSelf {
            spec: "@self".into()
        })
    );
}

#[test]
fn self_from_non_init_is_invalid() {
    let dir = tree(&["pkg/mod.luau", "pkg/inner.luau"]);
    assert_eq!(
        resolve(&dir.path().join("pkg/mod.luau"), "@self/inner"),
        Err(ResolveError::InvalidSelf {
            spec: "@self/inner".into()
        })
    );
}

// ── audit fix #9: parenthesis-free require form ──────────────────────────────

#[test]
fn scan_requires_matches_paren_free_form() {
    let found =
        crate::resolve::scan_requires("local a = require \"./a\"\nlocal b = require '@lune/fs'\n");
    assert_eq!(found, vec!["./a", "@lune/fs"]);
}

// ── content-hash cache keys ──────────────────────────────────────────────────

#[test]
fn content_hash_is_deterministic_and_content_sensitive() {
    let dir = tree(&["a.luau"]);
    write(dir.path(), "a.luau", "return 1\n");
    let first = content_hash(&dir.path().join("a.luau")).unwrap();
    let again = content_hash(&dir.path().join("a.luau")).unwrap();
    assert_eq!(first, again, "same bytes must hash identically");

    // A real edit changes the hash.
    write(dir.path(), "a.luau", "return 2\n");
    let edited = content_hash(&dir.path().join("a.luau")).unwrap();
    assert_ne!(first, edited, "an edit must change the content hash");
}

#[test]
fn hash_bytes_matches_known_fnv1a_vector() {
    // FNV-1a/64 of the empty input is the offset basis.
    assert_eq!(hash_bytes(b""), 0xcbf2_9ce4_8422_2325);
    // FNV-1a/64 of "a".
    assert_eq!(hash_bytes(b"a"), 0xaf63_dc4c_8601_ec8c);
}

#[test]
fn cache_key_pairs_path_and_hash() {
    let dir = tree(&["a.luau"]);
    write(dir.path(), "a.luau", "return 1\n");
    let (path, hash) = cache_key(&dir.path().join("a.luau")).unwrap();
    assert_eq!(path, cache_key_path(&dir.path().join("a.luau")));
    assert_eq!(hash, content_hash(&dir.path().join("a.luau")).unwrap());
}

// ── DependencyGraph ──────────────────────────────────────────────────────────

#[test]
fn graph_affected_specs_walks_inverse() {
    let dir = tree(&[
        "spec_a.luau",
        "spec_b.luau",
        "shared.luau",
        "leaf.luau",
        "lonely.luau",
    ]);
    write(
        dir.path(),
        "spec_a.luau",
        "local s = require('./shared')\nreturn nil\n",
    );
    write(
        dir.path(),
        "spec_b.luau",
        "local s = require('./shared')\nreturn nil\n",
    );
    write(dir.path(), "shared.luau", "return require('./leaf')\n");
    write(dir.path(), "leaf.luau", "return {}\n");
    write(dir.path(), "lonely.luau", "return {}\n");

    let specs = [
        dir.path().join("spec_a.luau"),
        dir.path().join("spec_b.luau"),
        dir.path().join("lonely.luau"),
    ];
    let graph = DependencyGraph::build(dir.path(), &specs);

    // A change to a deep leaf re-runs both specs that transitively require it.
    let affected = graph.affected_specs([dir.path().join("leaf.luau")]);
    assert_eq!(affected.len(), 2);
    assert!(affected.contains(&cache_key_path(&dir.path().join("spec_a.luau"))));
    assert!(affected.contains(&cache_key_path(&dir.path().join("spec_b.luau"))));

    // A changed spec that nothing depends on re-runs only itself.
    let affected = graph.affected_specs([dir.path().join("lonely.luau")]);
    assert_eq!(
        affected,
        hashset_of([cache_key_path(&dir.path().join("lonely.luau"))])
    );

    // A mid-graph module re-runs exactly its two dependents — which two, not
    // just how many.
    let affected = graph.affected_specs([dir.path().join("shared.luau")]);
    assert_eq!(
        affected,
        hashset_of([
            cache_key_path(&dir.path().join("spec_a.luau")),
            cache_key_path(&dir.path().join("spec_b.luau")),
        ])
    );
}

#[test]
fn graph_handles_cycles() {
    let dir = tree(&["spec.luau", "a.luau", "b.luau"]);
    // a <-> b cycle must not infinite-loop at build or query time.
    write(dir.path(), "spec.luau", "return require('./a')\n");
    write(dir.path(), "a.luau", "return require('./b')\n");
    write(dir.path(), "b.luau", "return require('./a')\n");

    let graph = DependencyGraph::build(dir.path(), [dir.path().join("spec.luau")]);
    let affected = graph.affected_specs([dir.path().join("b.luau")]);
    assert!(affected.contains(&cache_key_path(&dir.path().join("spec.luau"))));
}

#[test]
fn graph_ignores_runtime_builtins() {
    let dir = tree(&["spec.luau", "a.luau"]);
    write(
        dir.path(),
        "spec.luau",
        "local fs = require('@lune/fs')\nreturn require('./a')\n",
    );
    write(dir.path(), "a.luau", "return {}\n");

    let graph = DependencyGraph::build(dir.path(), [dir.path().join("spec.luau")]);
    // The builtin is terminal: never a node, so never a dependency.
    assert!(!graph.contains(Path::new("@lune/fs")));
    let deps = graph.dependencies(&dir.path().join("spec.luau")).unwrap();
    assert_eq!(deps.len(), 1);
    assert!(deps.contains(&cache_key_path(&dir.path().join("a.luau"))));
}

#[test]
fn graph_follows_luaurc_alias_transitively() {
    let dir = tree(&["src/spec.luau", "libs/util.luau"]);
    write(
        dir.path(),
        ".luaurc",
        r#"{ "aliases": { "util": "libs/util" } }"#,
    );
    write(dir.path(), "src/spec.luau", "return require('@util')\n");
    write(dir.path(), "libs/util.luau", "return {}\n");

    let graph = DependencyGraph::build(dir.path(), [dir.path().join("src/spec.luau")]);
    // A change to the alias target re-runs the spec that requires it.
    let affected = graph.affected_specs([dir.path().join("libs/util.luau")]);
    assert!(affected.contains(&cache_key_path(&dir.path().join("src/spec.luau"))));
}

// ── audit fix: path identity across a Windows drive prefix ──────────────────

#[cfg(target_os = "windows")]
#[test]
fn normalize_folds_a_windows_prefix_and_its_components() {
    // `Component::Prefix` compares case-insensitively but `Component::Normal`
    // does not, so mixing a normalized path with an unnormalized one diverges
    // right after the drive letter — the mechanism behind the generated-require
    // bug that loads a second copy of the framework. `/Foo/Bar` has no prefix
    // component at all and cannot exercise it.
    let upper = normalize(Path::new(r"C:\Proj\Src\mod.luau"));
    let lower = normalize(Path::new(r"c:\proj\src\mod.luau"));
    assert_eq!(upper, lower);

    // And a relative path can be taken in either direction of casing.
    let root_upper = normalize(Path::new(r"C:\Proj"));
    let root_lower = normalize(Path::new(r"c:\proj"));
    assert_eq!(
        upper.strip_prefix(&root_lower).unwrap(),
        Path::new(r"src\mod.luau")
    );
    assert!(lower.strip_prefix(&root_upper).is_ok());
}

// ── audit fix: previously untested resolver branches ─────────────────────────

#[test]
fn dependency_closure_survives_a_require_cycle() {
    let dir = tree(&["a.luau", "b.luau"]);
    write(dir.path(), "a.luau", "return require('./b')\n");
    write(dir.path(), "b.luau", "return require('./a')\n");
    let closure = crate::resolve::dependency_closure(&dir.path().join("a.luau"));
    assert_eq!(
        closure,
        hashset_of([
            normalize(&dir.path().join("a.luau")),
            normalize(&dir.path().join("b.luau")),
        ])
    );
}

#[test]
fn dependency_closure_all_unions_every_entry() {
    let dir = tree(&["spec_a.luau", "spec_b.luau", "shared.luau", "solo.luau"]);
    write(dir.path(), "spec_a.luau", "return require('./shared')\n");
    write(dir.path(), "spec_b.luau", "return require('./shared')\n");
    write(dir.path(), "shared.luau", "return {}\n");

    let closure = crate::resolve::dependency_closure_all([
        dir.path().join("spec_a.luau"),
        dir.path().join("spec_b.luau"),
    ]);
    assert_eq!(
        closure,
        hashset_of([
            normalize(&dir.path().join("spec_a.luau")),
            normalize(&dir.path().join("spec_b.luau")),
            normalize(&dir.path().join("shared.luau")),
        ])
    );
}

#[test]
fn content_hash_of_a_missing_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(content_hash(&dir.path().join("nope.luau")).is_err());
}

#[test]
fn self_subpath_works_from_init_lua() {
    let dir = tree(&["pkg/init.lua", "pkg/inner.luau"]);
    assert_resolves(dir.path(), "pkg/init.lua", "@self/inner", "pkg/inner.luau");
}

#[test]
fn self_from_init_server_script_is_invalid() {
    // `init.server.luau` has a file stem of "init.server". Rojo's server/client
    // init forms describe how a directory becomes a Script in a built place;
    // they are not modules a string require can reach, so `@self` from one is
    // an error rather than a surprise resolution.
    let dir = tree(&["pkg/init.server.luau", "pkg/inner.luau"]);
    assert_eq!(
        resolve(&dir.path().join("pkg/init.server.luau"), "@self/inner"),
        Err(ResolveError::InvalidSelf {
            spec: "@self/inner".into()
        })
    );
}

#[test]
fn luaurc_alias_value_may_be_absolute() {
    // An alias value is joined onto the `.luaurc`'s directory; an absolute
    // value replaces it outright, which is how a vendored path outside the
    // project is spelled.
    let dir = tree(&["src/mod.luau"]);
    let vendor = tempfile::tempdir().unwrap();
    fs::write(vendor.path().join("lib.luau"), "return {}\n").unwrap();
    // JSON string escaping for Windows separators.
    let value = vendor
        .path()
        .join("lib")
        .to_string_lossy()
        .replace('\\', "\\\\");
    write(
        dir.path(),
        ".luaurc",
        &format!(r#"{{ "aliases": {{ "lib": "{value}" }} }}"#),
    );
    let resolved = expect_file(resolve(&dir.path().join("src/mod.luau"), "@lib"));
    assert_eq!(resolved, normalize(&vendor.path().join("lib.luau")));
}

// ── audit fix: sanitize_luaurc must not mangle UTF-8 ─────────────────────────

#[test]
fn sanitize_luaurc_round_trips_non_ascii() {
    // Byte-wise `as char` turned every multi-byte character into mojibake
    // (`é` → `Ã©`). `lest init` detects comments by asking whether sanitizing
    // changed the text, so this also made a comment-free `.luaurc` holding one
    // accented character look like it had comments.
    for input in [
        "{ \"aliases\": { \"café\": \"libs/café\" } }",
        "\u{feff}{ \"aliases\": { \"a\": \"b\" } }", // UTF-8 BOM
        "{ \"note\": \"日本語のパス\" }",
        "{ \"emoji\": \"🎉\" }",
    ] {
        assert_eq!(
            crate::resolve::sanitize_luaurc(input),
            input,
            "comment-free non-ASCII input must round-trip unchanged"
        );
    }
}

#[test]
fn sanitize_luaurc_strips_comments_around_non_ascii() {
    let input = "{ // héllo\n  \"aliases\": { \"ü\": \"libs/ü\" }, /* añadido */ }";
    let out = crate::resolve::sanitize_luaurc(input);
    assert!(!out.contains("héllo"));
    assert!(!out.contains("añadido"));
    assert!(out.contains("\"ü\""));
    assert!(out.contains("libs/ü"));
    // And the result is valid JSON with the alias intact.
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["aliases"]["ü"], "libs/ü");
}

#[test]
fn scan_requires_is_not_confused_by_a_preceding_multibyte_char() {
    // `bytes[start - 1] as char` decoded the *last byte* of the preceding
    // character: for U+2039 (E2 80 B9) that byte is 0xB9 → '¹', which
    // `is_alphanumeric` accepts, so the require looked like an identifier
    // suffix and its edge was silently dropped — under-selection, the wrong
    // direction for watch mode.
    let found = crate::resolve::scan_requires("local a = \u{2039}require('./a')\n");
    assert_eq!(found, vec!["./a"]);
}

/// Small helper to build a HashSet literal for assertions.
fn hashset_of<const N: usize>(items: [PathBuf; N]) -> std::collections::HashSet<PathBuf> {
    items.into_iter().collect()
}
