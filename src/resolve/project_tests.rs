//! Rojo project mapping (phase 4): parsing `default.project.json` into a
//! bidirectional filesystem ↔ DataModel map.

use std::fs;
use std::path::Path;

use super::{
    cache_key_path, parse_datamodel_path, resolve, DataModelNode, ResolveError, Resolved,
    VirtualDataModel,
};

/// Writes `contents` to `rel` under `dir`, creating parent directories.
fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, contents).unwrap();
}

/// Canonical-key equality for two filesystem paths (folds case / short names).
fn same_path(a: &Path, b: &Path) -> bool {
    cache_key_path(a) == cache_key_path(b)
}

/// A realistic multi-service project laid over a source tree.
///
/// ```text
/// ReplicatedStorage.Common          -> src/common (init.luau => ModuleScript)
/// ReplicatedStorage.Common.Util     -> src/common/Util.luau
/// ReplicatedStorage.Common.Net      -> src/common/Net (init.luau => ModuleScript)
/// ReplicatedStorage.Common.Net.Http -> src/common/Net/Http.luau
/// ServerScriptService.Server        -> src/server (no init => Folder)
/// ServerScriptService.Server.Main   -> src/server/Main.server.luau (Script)
/// ServerScriptService.Server.Config -> src/server/Config.luau
/// StarterPlayer... (structural, no $path)
/// ```
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/common/init.luau", "return {}\n");
    write(dir.path(), "src/common/Util.luau", "return {}\n");
    write(dir.path(), "src/common/Net/init.luau", "return {}\n");
    write(dir.path(), "src/common/Net/Http.luau", "return {}\n");
    write(dir.path(), "src/common/data.json", "{}\n"); // non-script, must be ignored
    write(dir.path(), "src/server/Main.server.luau", "return nil\n");
    write(dir.path(), "src/server/Config.luau", "return {}\n");
    write(
        dir.path(),
        "default.project.json",
        r#"{
            "name": "my-game",
            "tree": {
                "$className": "DataModel",
                "ReplicatedStorage": {
                    "Common": { "$path": "src/common" }
                },
                "ServerScriptService": {
                    "Server": { "$path": "src/server" }
                },
                "Workspace": {
                    "Baseplate": { "$className": "Part" }
                }
            }
        }"#,
    );
    dir
}

fn model(dir: &Path) -> VirtualDataModel {
    VirtualDataModel::from_project_file(&dir.join("default.project.json")).unwrap()
}

fn node<'a>(m: &'a VirtualDataModel, dm: &[&str]) -> &'a DataModelNode {
    m.node_at(dm).unwrap_or_else(|| panic!("no node at {dm:?}"))
}

#[test]
fn maps_source_file_to_datamodel_path() {
    let dir = fixture();
    let m = model(dir.path());
    let util = dir.path().join("src/common/Util.luau");
    assert_eq!(
        m.datamodel_path(&util),
        Some(
            ["ReplicatedStorage", "Common", "Util"]
                .map(String::from)
                .as_slice()
        )
    );
}

#[test]
fn maps_datamodel_path_back_to_source_file() {
    let dir = fixture();
    let m = model(dir.path());
    let fs = m
        .filesystem_path(&["ReplicatedStorage", "Common", "Util"])
        .unwrap();
    assert!(same_path(fs, &dir.path().join("src/common/Util.luau")));
}

#[test]
fn init_promotes_directory_to_module() {
    let dir = fixture();
    let m = model(dir.path());
    // The `Common` node is the directory itself, sourced from its init.luau.
    let common = node(&m, &["ReplicatedStorage", "Common"]);
    assert_eq!(common.class_name, "ModuleScript");
    assert!(same_path(
        common.source.as_deref().unwrap(),
        &dir.path().join("src/common/init.luau")
    ));
    // And the init file maps back to the directory's DataModel path (no `init`
    // segment appended).
    assert_eq!(
        m.datamodel_path(&dir.path().join("src/common/init.luau")),
        Some(["ReplicatedStorage", "Common"].map(String::from).as_slice())
    );
}

#[test]
fn nested_directory_recurses() {
    let dir = fixture();
    let m = model(dir.path());
    let net = node(&m, &["ReplicatedStorage", "Common", "Net"]);
    assert_eq!(net.class_name, "ModuleScript");
    let http = node(&m, &["ReplicatedStorage", "Common", "Net", "Http"]);
    assert!(same_path(
        http.source.as_deref().unwrap(),
        &dir.path().join("src/common/Net/Http.luau")
    ));
}

#[test]
fn server_script_gets_script_class_and_stripped_name() {
    let dir = fixture();
    let m = model(dir.path());
    // `Main.server.luau` -> instance "Main", class Script.
    let main = node(&m, &["ServerScriptService", "Server", "Main"]);
    assert_eq!(main.class_name, "Script");
    assert!(same_path(
        main.source.as_deref().unwrap(),
        &dir.path().join("src/server/Main.server.luau")
    ));
}

#[test]
fn directory_without_init_is_a_folder() {
    let dir = fixture();
    let m = model(dir.path());
    let server = node(&m, &["ServerScriptService", "Server"]);
    assert_eq!(server.class_name, "Folder");
    // A Folder is sourced from its directory.
    assert!(same_path(
        server.source.as_deref().unwrap(),
        &dir.path().join("src/server")
    ));
}

#[test]
fn service_with_only_children_is_structural() {
    let dir = fixture();
    let m = model(dir.path());
    // Service inferred from its own name, no source.
    let workspace = node(&m, &["Workspace"]);
    assert_eq!(workspace.class_name, "Workspace");
    assert_eq!(workspace.source, None);
    // Explicit `$className` on a child with no `$path`.
    let base = node(&m, &["Workspace", "Baseplate"]);
    assert_eq!(base.class_name, "Part");
    assert_eq!(base.source, None);
}

#[test]
fn ignores_non_script_files() {
    let dir = fixture();
    let m = model(dir.path());
    // `src/common/data.json` must not become an instance.
    assert!(m
        .node_at(&["ReplicatedStorage", "Common", "data"])
        .is_none());
    assert!(m
        .datamodel_path(&dir.path().join("src/common/data.json"))
        .is_none());
}

#[test]
fn pairs_enumerates_every_backed_mapping() {
    let dir = fixture();
    let m = model(dir.path());
    let pairs: Vec<_> = m.pairs().collect();
    // Every pair round-trips both directions.
    for (dm, fs) in &pairs {
        assert_eq!(
            m.filesystem_path(dm).map(cache_key_path),
            Some(cache_key_path(fs))
        );
        assert_eq!(m.datamodel_path(fs), Some(*dm));
    }
    // The known modules and folders are all present (Common, Util, Net, Http,
    // Server folder, Main, Config).
    assert!(pairs
        .iter()
        .any(|(dm, _)| dm == &["ReplicatedStorage", "Common", "Net", "Http"]));
    assert!(pairs
        .iter()
        .any(|(dm, _)| dm == &["ServerScriptService", "Server", "Config"]));
}

#[test]
fn single_file_path_maps_one_module() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/lonely.luau", "return {}\n");
    write(
        dir.path(),
        "default.project.json",
        r#"{
            "name": "solo",
            "tree": {
                "$className": "DataModel",
                "ReplicatedStorage": {
                    "Lonely": { "$path": "src/lonely.luau" }
                }
            }
        }"#,
    );
    let m = model(dir.path());
    let lonely = node(&m, &["ReplicatedStorage", "Lonely"]);
    assert_eq!(lonely.class_name, "ModuleScript");
    assert!(same_path(
        lonely.source.as_deref().unwrap(),
        &dir.path().join("src/lonely.luau")
    ));
}

#[test]
fn missing_path_target_is_recorded_not_panicked() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "default.project.json",
        r#"{ "tree": { "ReplicatedStorage": { "Gone": { "$path": "src/gone" } } } }"#,
    );
    let m = model(dir.path());
    // The mapping is recorded even though nothing exists on disk yet.
    let gone = node(&m, &["ReplicatedStorage", "Gone"]);
    assert!(gone.source.is_some());
    // It is not a module (no file), so instance-resolve reports NotFound.
    assert!(matches!(
        m.resolve(&["ReplicatedStorage", "Gone"]),
        Err(ResolveError::NotFound { .. })
    ));
}

#[test]
fn missing_tree_yields_empty_model() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "default.project.json", r#"{ "name": "empty" }"#);
    let m = model(dir.path());
    assert_eq!(m.name(), "empty");
    assert_eq!(m.nodes().len(), 0);
    assert_eq!(m.pairs().count(), 0);
}

#[test]
fn absent_project_file_errors_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    match VirtualDataModel::from_project_file(&dir.path().join("nope.project.json")) {
        Err(ResolveError::Project { .. }) => {}
        other => panic!("expected Project error, got {other:?}"),
    }
}

#[test]
fn malformed_json_errors_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "default.project.json", "{ this is : not json ]");
    match VirtualDataModel::from_project_file(&dir.path().join("default.project.json")) {
        Err(ResolveError::Project { .. }) => {}
        other => panic!("expected Project error, got {other:?}"),
    }
}

#[test]
fn tolerates_comments_and_trailing_commas() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/Mod.luau", "return {}\n");
    write(
        dir.path(),
        "default.project.json",
        r#"{
            // a project with jsonc leniency
            "name": "jsonc",
            "tree": {
                "ReplicatedStorage": {
                    "Mod": { "$path": "src/Mod.luau" },
                },
            },
        }"#,
    );
    let m = model(dir.path());
    assert!(m.node_at(&["ReplicatedStorage", "Mod"]).is_some());
}

#[test]
fn instance_resolve_matches_string_resolve() {
    let dir = fixture();
    let m = model(dir.path());
    // Resolving an instance-based require yields the same File a string require
    // to the on-disk module would.
    let via_instance = m.resolve(&["ReplicatedStorage", "Common", "Util"]).unwrap();
    let via_string = resolve(&dir.path().join("src/common/init.luau"), "@self/Util").unwrap();
    match (&via_instance, &via_string) {
        (Resolved::File(a), Resolved::File(b)) => assert_eq!(a, b),
        other => panic!("expected two File resolutions, got {other:?}"),
    }
}

#[test]
fn instance_resolve_of_folder_is_not_found() {
    let dir = fixture();
    let m = model(dir.path());
    // `Server` is a Folder, not a module — instance-resolving it is NotFound.
    assert!(matches!(
        m.resolve(&["ServerScriptService", "Server"]),
        Err(ResolveError::NotFound { .. })
    ));
}

#[test]
fn from_json_resolves_paths_against_given_root() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "code/Thing.luau", "return {}\n");
    let m = VirtualDataModel::from_json(
        dir.path(),
        r#"{ "tree": { "ReplicatedStorage": { "Thing": { "$path": "code/Thing.luau" } } } }"#,
    )
    .unwrap();
    assert!(same_path(
        m.filesystem_path(&["ReplicatedStorage", "Thing"]).unwrap(),
        &dir.path().join("code/Thing.luau")
    ));
}

#[test]
fn parse_datamodel_path_strips_root_and_accepts_both_separators() {
    assert_eq!(
        parse_datamodel_path("game.ReplicatedStorage.Common"),
        vec!["ReplicatedStorage", "Common"]
    );
    assert_eq!(
        parse_datamodel_path("DataModel/ReplicatedStorage/Common/Util"),
        vec!["ReplicatedStorage", "Common", "Util"]
    );
    assert_eq!(
        parse_datamodel_path("ReplicatedStorage.Common"),
        vec!["ReplicatedStorage", "Common"]
    );
    assert_eq!(parse_datamodel_path(""), Vec::<String>::new());
}

#[test]
fn parsed_path_drives_lookup() {
    let dir = fixture();
    let m = model(dir.path());
    let segments = parse_datamodel_path("game.ReplicatedStorage.Common.Net.Http");
    let fs = m.filesystem_path(&segments).unwrap();
    assert!(same_path(fs, &dir.path().join("src/common/Net/Http.luau")));
}

#[test]
fn client_script_gets_localscript_class() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/ui/Button.client.luau", "return nil\n");
    write(
        dir.path(),
        "default.project.json",
        r#"{ "tree": { "StarterGui": { "UI": { "$path": "src/ui" } } } }"#,
    );
    let m = model(dir.path());
    let button = node(&m, &["StarterGui", "UI", "Button"]);
    assert_eq!(button.class_name, "LocalScript");
}

// ── audit fix: previously untested walker branches ──────────────────────────

#[test]
fn init_server_script_promotes_directory_to_a_script() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/boot/init.server.luau", "return nil\n");
    write(dir.path(), "src/boot/Helper.luau", "return {}\n");
    write(
        dir.path(),
        "default.project.json",
        r#"{ "tree": { "ServerScriptService": { "Boot": { "$path": "src/boot" } } } }"#,
    );
    let m = model(dir.path());
    let boot = node(&m, &["ServerScriptService", "Boot"]);
    assert_eq!(boot.class_name, "Script");
    assert!(same_path(
        boot.source.as_deref().unwrap(),
        &dir.path().join("src/boot/init.server.luau")
    ));
    // The init script *is* the directory; it never appears as a child.
    assert!(m
        .node_at(&["ServerScriptService", "Boot", "init.server"])
        .is_none());
    // Its siblings still become children.
    assert!(m
        .node_at(&["ServerScriptService", "Boot", "Helper"])
        .is_some());
}

#[test]
fn meta_json_sidecars_are_not_instances() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/ui/Panel.luau", "return {}\n");
    write(dir.path(), "src/ui/Panel.meta.json", "{}\n");
    write(
        dir.path(),
        "default.project.json",
        r#"{ "tree": { "StarterGui": { "UI": { "$path": "src/ui" } } } }"#,
    );
    let m = model(dir.path());
    assert!(m.node_at(&["StarterGui", "UI", "Panel"]).is_some());
    // The sidecar decorates `Panel`; it is not its own instance.
    assert!(m.node_at(&["StarterGui", "UI", "Panel.meta"]).is_none());
    assert!(m
        .datamodel_path(&dir.path().join("src/ui/Panel.meta.json"))
        .is_none());
}

#[test]
fn explicit_child_key_overrides_the_walked_directory_child() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/common/Util.luau", "return 'walked'\n");
    write(dir.path(), "overrides/Util.luau", "return 'explicit'\n");
    write(
        dir.path(),
        "default.project.json",
        r#"{ "tree": { "ReplicatedStorage": { "Common": {
                "$path": "src/common",
                "Util": { "$path": "overrides/Util.luau" }
        } } } }"#,
    );
    let m = model(dir.path());
    // The `$path` walk emits `Common.Util` first, then the explicit child key
    // re-emits it — the explicit mapping is what rojo honors.
    assert!(same_path(
        m.filesystem_path(&["ReplicatedStorage", "Common", "Util"])
            .unwrap(),
        &dir.path().join("overrides/Util.luau")
    ));
}

#[test]
fn one_file_mapped_twice_keeps_the_last_writer_in_the_reverse_index() {
    // `by_fs` is documented as last-writer-wins on a collision. Both forward
    // mappings survive; only the file → DataModel direction has to pick one.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/Util.luau", "return {}\n");
    write(
        dir.path(),
        "default.project.json",
        r#"{ "tree": { "ReplicatedStorage": {
                "First": { "$path": "src/Util.luau" },
                "Second": { "$path": "src/Util.luau" }
        } } }"#,
    );
    let m = model(dir.path());
    assert!(same_path(
        m.filesystem_path(&["ReplicatedStorage", "First"]).unwrap(),
        &dir.path().join("src/Util.luau")
    ));
    assert!(same_path(
        m.filesystem_path(&["ReplicatedStorage", "Second"]).unwrap(),
        &dir.path().join("src/Util.luau")
    ));
    assert_eq!(
        m.datamodel_path(&dir.path().join("src/Util.luau")),
        Some(["ReplicatedStorage", "Second"].map(String::from).as_slice())
    );
}

// ── audit fix: honest classes for non-script `$path` files, and a diagnostic
//    for `$path` aimed at a sub-project ────────────────────────────────────────

#[test]
fn subproject_path_is_a_clear_error_not_a_bogus_node() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "sub/sub.project.json", r#"{ "tree": {} }"#);
    write(
        dir.path(),
        "default.project.json",
        r#"{ "tree": { "ReplicatedStorage": { "Sub": { "$path": "sub/sub.project.json" } } } }"#,
    );
    // Silently mapping the project file itself as a ModuleScript was a wrong
    // node nothing would flag; composing sub-projects is out of scope, so the
    // walker must say so instead.
    match VirtualDataModel::from_project_file(&dir.path().join("default.project.json")) {
        Err(ResolveError::Project { path, message }) => {
            assert!(
                path.to_string_lossy()
                    .to_lowercase()
                    .ends_with("sub.project.json"),
                "the error names the sub-project file: {path:?}"
            );
            assert!(message.contains("compose"), "{message}");
        }
        other => panic!("expected a Project error for a sub-project $path, got {other:?}"),
    }
}

#[test]
fn non_script_path_files_get_honest_classes_or_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "assets/data.json", "{}\n");
    write(dir.path(), "assets/notes.txt", "hello\n");
    write(dir.path(), "assets/rows.csv", "key,value\n");
    write(dir.path(), "assets/model.rbxm", "binary\n");
    write(
        dir.path(),
        "default.project.json",
        r#"{ "tree": { "ReplicatedStorage": {
                "Data": { "$path": "assets/data.json" },
                "Notes": { "$path": "assets/notes.txt" },
                "Rows": { "$path": "assets/rows.csv" },
                "Model": { "$path": "assets/model.rbxm" }
        } } }"#,
    );
    let m = model(dir.path());
    // The kinds whose class the filename determines get rojo's real classes…
    assert_eq!(
        node(&m, &["ReplicatedStorage", "Data"]).class_name,
        "ModuleScript"
    );
    assert_eq!(
        node(&m, &["ReplicatedStorage", "Notes"]).class_name,
        "StringValue"
    );
    assert_eq!(
        node(&m, &["ReplicatedStorage", "Rows"]).class_name,
        "LocalizationTable"
    );
    // …and none of them are requirable modules.
    assert!(matches!(
        m.resolve(&["ReplicatedStorage", "Data"]),
        Err(ResolveError::NotFound { .. })
    ));
    // A model's root class would need parsing the file: skipped explicitly
    // rather than mislabeled ModuleScript.
    assert!(m.node_at(&["ReplicatedStorage", "Model"]).is_none());
}

#[test]
fn explicit_classname_overrides_inferred_folder() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "assets/x.luau", "return {}\n");
    write(
        dir.path(),
        "default.project.json",
        // A directory `$path` with an explicit non-Folder class.
        r#"{ "tree": { "ReplicatedStorage": { "Assets": { "$className": "Configuration", "$path": "assets" } } } }"#,
    );
    let m = model(dir.path());
    assert_eq!(
        node(&m, &["ReplicatedStorage", "Assets"]).class_name,
        "Configuration"
    );
}
