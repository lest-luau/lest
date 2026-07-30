use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use serde::Deserialize;

use crate::error::ToolError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum BackendKind {
    /// The embedded Luau VM — the backend native to lest.
    Native,
    Lune,
    Lute,
    Cloud,
    /// Roblox Studio, launched per run via its official CLI.
    Studio,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            BackendKind::Native => "native",
            BackendKind::Lune => "lune",
            BackendKind::Lute => "lute",
            BackendKind::Cloud => "cloud",
            BackendKind::Studio => "studio",
        };
        f.write_str(name)
    }
}

/// `lest.toml` as written by the user. Unknown keys are tolerated so configs
/// written for later versions still parse — but tolerated is not the same as
/// unmentioned, so [`unknown_keys`] names them back to the reader.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    /// DEPRECATION(0.5): the bare top-level `backend` moved to
    /// `[settings] backend` in 0.4; honored with a warning until 0.5.
    backend: Option<BackendKind>,
    #[serde(default)]
    suites: IndexMap<String, RawSuite>,
    #[serde(default)]
    settings: RawSettings,
    #[serde(default)]
    coverage: RawCoverage,
    /// DEPRECATION(0.5): `[cloud]` moved to `[place]` in 0.4 (the table
    /// held the engine target, not cloud transport); honored with a
    /// warning until 0.5.
    #[serde(default)]
    cloud: RawCloud,
    #[serde(default)]
    place: RawPlace,
    #[serde(default)]
    studio: RawStudio,
}

/// The `[place]` table: the Roblox place engine suites run in, agnostic of
/// which backend runs them — cloud uploads/pins `file` and targets the
/// ids; studio launches `file` (or the published ids); `rojo` maps string
/// requires into the place for both.
#[derive(Debug, Default, Clone, Deserialize)]
struct RawPlace {
    universe_id: Option<CloudId>,
    place_id: Option<CloudId>,
    /// Root-relative path to a built place file (`.rbxl`/`.rbxlx`).
    file: Option<String>,
    /// Root-relative rojo project file mapping the filesystem into the
    /// place, enabling string-require delegation to live instances.
    rojo: Option<String>,
}

/// The `[studio]` table: settings for launching Roblox Studio. Only the
/// executable override today (for non-standard install locations).
#[derive(Debug, Default, Deserialize)]
struct RawStudio {
    executable: Option<String>,
}

/// Open Cloud target for cloud-backend suites. `universe_id`/`place_id` are
/// non-secret Roblox identifiers and belong in config; the API key never does
/// (it is read from the environment). Numbers are accepted as TOML integers or
/// strings — Roblox ids fit in i64, but a string spelling is also honored so a
/// config can never lose precision.
#[derive(Debug, Default, Clone, Deserialize)]
struct RawCloud {
    universe_id: Option<CloudId>,
    place_id: Option<CloudId>,
    /// Root-relative path to a built place file (`.rbxl`/`.rbxlx`). When set,
    /// the cloud backend uploads it as a new saved version before running —
    /// skipped when the content hash is unchanged — and pins every task to
    /// that version.
    place_file: Option<String>,
}

/// A Roblox identifier that may be written as a bare TOML integer or a quoted
/// string; both normalize to the canonical decimal string used in URLs.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum CloudId {
    Int(i64),
    Text(String),
}

impl CloudId {
    fn into_string(self) -> String {
        match self {
            CloudId::Int(n) => n.to_string(),
            CloudId::Text(s) => s,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawCoverage {
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    min: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawSuite {
    include: Vec<String>,
    backend: Option<BackendKind>,
    default: Option<bool>,
    /// DEPRECATION(0.5): `[suites.X.cloud]` moved to `[suites.X.place]` in
    /// 0.4; honored with a warning until 0.5.
    #[serde(default)]
    cloud: RawCloud,
    /// Per-suite place, overriding the top-level `[place]` block.
    #[serde(default)]
    place: RawPlace,
}

#[derive(Debug, Default, Deserialize)]
struct RawSettings {
    /// Default backend for suites that don't declare one.
    backend: Option<BackendKind>,
    timeout_ms: Option<u64>,
    workers: Option<usize>,
    /// DEPRECATION(0.5): `[settings] rojo` moved to `[place] rojo` in 0.4;
    /// honored with a warning until 0.5.
    rojo: Option<String>,
    core: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Suite {
    pub name: String,
    pub include: Vec<String>,
    pub backend: BackendKind,
    /// Suites with `default = false` only run when named explicitly or when
    /// CI is detected.
    pub default_enabled: bool,
    /// The place this suite's engine tests run in: per-suite `[suites.X.place]`
    /// overriding the top-level `[place]`, field by field. Consulted by the
    /// cloud and studio backends.
    pub place: PlaceTarget,
}

/// The place resolved for a suite (per-suite overriding top-level). Ids may
/// still be `None` when nothing supplied them; the engine backends turn a
/// missing target into a clear tool error at run time. Never holds the API
/// key — that is environment-only.
#[derive(Debug, Clone, Default)]
pub struct PlaceTarget {
    pub universe_id: Option<String>,
    pub place_id: Option<String>,
    /// Root-relative path to a built place file. Cloud uploads it as a new
    /// saved version (hash-skipped) and pins tasks to it; studio launches
    /// it. `None` means the published place (cloud: latest version).
    pub file: Option<String>,
    /// Root-relative rojo project file for string-require delegation into
    /// the place.
    pub rojo: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub suites: Vec<Suite>,
    pub timeout: Duration,
    /// Native-backend worker threads; `0` means one per CPU.
    pub workers: usize,
    /// Path (relative to the project root) of the lest/core framework entry.
    /// `None` — the usual case — means the copy embedded in this binary,
    /// materialized into `.lest/core`. Setting it opts out, which is how this
    /// repo dogfoods its own working copy of the framework.
    pub core: Option<String>,
    /// Coverage settings (native suites only).
    pub coverage: Coverage,
    /// `[studio] executable` — a path to the Roblox Studio binary, for
    /// non-standard installs. `None` means the platform default location.
    pub studio_executable: Option<String>,
    /// The `lest.toml` this config was read from, or `None` in zero-config
    /// mode. Carried so callers can point at the real file (watch mode watches
    /// it by identity; the empty-discovery message only mentions a config file
    /// when one exists).
    pub file: Option<PathBuf>,
}

/// Line-coverage configuration. `include`/`exclude` globs are matched against
/// the root-relative, forward-slashed spec/source path; `min` gates CI when set.
#[derive(Debug, Clone)]
pub struct Coverage {
    /// When `Some`, only files matching one of these globs are reported —
    /// `exclude` still applies on top. `None` (the usual case) means every
    /// file the run loaded is a candidate. Never `Some` of an empty list: a
    /// present-but-empty `include` is rejected at load, because reading it as
    /// "cover nothing" and reading it as "cover everything" are both defensible
    /// and the config would silently mean one of them.
    pub include: Option<Vec<String>>,
    pub exclude: Vec<String>,
    pub min: Option<f64>,
}

const DEFAULT_TIMEOUT_MS: u64 = 5000;
/// Files that are never the user's own code under test, excluded from coverage
/// unless the config overrides `[coverage] exclude`.
const DEFAULT_COVERAGE_EXCLUDE: &[&str] = &["**/*.spec.luau", "**/*.spec.lua", "Packages/**"];

/// Loads `lest.toml`. Without an explicit `--config`, a `lest.toml` in the
/// working directory is used when present; otherwise everything defaults to
/// one native suite over `**/*.spec.luau` (zero configuration for a standard
/// project). Returns the config plus the project root (the config's
/// directory).
pub fn load(explicit: Option<&Path>, cwd: &Path) -> Result<(Config, PathBuf), ToolError> {
    let path = match explicit {
        Some(path) => {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            if !path.is_file() {
                return Err(ToolError(format!(
                    "config file not found: {}",
                    path.display()
                )));
            }
            Some(path)
        }
        None => {
            let candidate = cwd.join("lest.toml");
            candidate.is_file().then_some(candidate)
        }
    };

    let (raw, root, file) = match path {
        Some(path) => {
            let text = fs::read_to_string(&path)
                .map_err(|e| ToolError(format!("cannot read {}: {e}", path.display())))?;
            let raw: RawConfig = toml::from_str(&text)
                .map_err(|e| ToolError(format!("cannot parse {}:\n{e}", path.display())))?;
            for warning in config_warnings(&text, &raw, &path) {
                crate::report::warn_to_stderr(&warning);
            }
            let root = path.parent().unwrap_or(cwd).to_path_buf();
            (raw, root, Some(path))
        }
        None => (RawConfig::default(), cwd.to_path_buf(), None),
    };

    let mut config = resolve_raw(raw)?;
    config.file = file;
    Ok((config, root))
}

/// Every warning a parsed config earns. Serde drops what it does not
/// recognize, which is the tolerance we want — but silently, which is not:
/// `bakcend = "lune"` runs every spec on native and `deafult = false` leaves a
/// cloud suite enabled, both looking exactly like a working config. Split from
/// [`load`] so the triggers and wording are testable without capturing
/// stderr. (When a key is accepted ahead of being consumed — as
/// `[settings] rojo` once was — its unconsumed state belongs here too, so
/// acceptance never reads as support.)
fn config_warnings(text: &str, raw: &RawConfig, path: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let unknown = unknown_keys(text);
    if !unknown.is_empty() {
        warnings.push(unknown_keys_message(&unknown, path));
    }
    warnings.extend(deprecation_warnings(raw));
    warnings
}

/// The 0.3 spellings still honored in 0.4, each named with its new home so
/// a config migrates in one pass. DEPRECATION(0.5): when these fallbacks
/// are removed, these warnings become the only guidance — keep them until
/// the same release deletes both.
fn deprecation_warnings(raw: &RawConfig) -> Vec<String> {
    let mut out = Vec::new();
    let mut moved = |old: &str, new: &str| {
        out.push(format!(
            "`{old}` was renamed to `{new}` in 0.4 and is removed in 0.5 — update lest.toml"
        ));
    };
    if raw.backend.is_some() {
        moved("backend", "[settings] backend");
    }
    if raw.cloud.universe_id.is_some() {
        moved("[cloud] universe_id", "[place] universe_id");
    }
    if raw.cloud.place_id.is_some() {
        moved("[cloud] place_id", "[place] place_id");
    }
    if raw.cloud.place_file.is_some() {
        moved("[cloud] place_file", "[place] file");
    }
    if raw.settings.rojo.is_some() {
        moved("[settings] rojo", "[place] rojo");
    }
    for (name, suite) in &raw.suites {
        if suite.cloud.universe_id.is_some()
            || suite.cloud.place_id.is_some()
            || suite.cloud.place_file.is_some()
        {
            moved(
                &format!("[suites.{name}.cloud]"),
                &format!("[suites.{name}.place] (and `place_file` becomes `file`)"),
            );
        }
    }
    out
}

/// The unknown-key warning body — a lowercase fragment, capitalized by the
/// warning renderer. Split out so the wording is testable without capturing
/// stderr.
fn unknown_keys_message(unknown: &[String], path: &Path) -> String {
    format!(
        "ignoring unrecognized key{} in {}: {}",
        if unknown.len() == 1 { "" } else { "s" },
        path.display(),
        unknown.join(", ")
    )
}

/// Every key in `text` that lest does not understand, as a dotted path.
/// Deliberately schema-shaped rather than derived from the `Raw*` types: a
/// `#[serde(flatten)]` catch-all would route the whole config through serde's
/// buffered-content path, and the cloud ids depend on `untagged` integer
/// handling that is delicate there. Parse failures return nothing — the real
/// parse below reports those far better than a key list would.
fn unknown_keys(text: &str) -> Vec<String> {
    // DEPRECATION(0.5): these lists work on raw text, so the compiler will
    // NOT flag them when the Raw fields go — remove "backend" and "cloud"
    // from TOP, "cloud" from SUITE, "rojo" from SETTINGS, and the CLOUD
    // list (with its collect calls below) by hand, or removed spellings
    // will be silently ignored with no warning at all.
    const TOP: &[&str] = &[
        "backend", "suites", "settings", "coverage", "cloud", "place", "studio",
    ];
    const SUITE: &[&str] = &["include", "backend", "default", "cloud", "place"];
    const SETTINGS: &[&str] = &["backend", "timeout_ms", "workers", "rojo", "core"];
    const COVERAGE: &[&str] = &["include", "exclude", "min"];
    const CLOUD: &[&str] = &["universe_id", "place_id", "place_file"];
    const PLACE: &[&str] = &["universe_id", "place_id", "file", "rojo"];
    const STUDIO: &[&str] = &["executable"];

    fn collect(prefix: &str, table: &toml::Table, known: &[&str], out: &mut Vec<String>) {
        for key in table.keys() {
            if !known.contains(&key.as_str()) {
                out.push(format!("{prefix}{key}"));
            }
        }
    }
    fn table<'a>(parent: &'a toml::Table, key: &str) -> Option<&'a toml::Table> {
        parent.get(key).and_then(toml::Value::as_table)
    }

    let Ok(value) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let Some(root) = value.as_table() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    collect("", root, TOP, &mut out);
    if let Some(settings) = table(root, "settings") {
        collect("settings.", settings, SETTINGS, &mut out);
    }
    if let Some(coverage) = table(root, "coverage") {
        collect("coverage.", coverage, COVERAGE, &mut out);
    }
    if let Some(cloud) = table(root, "cloud") {
        collect("cloud.", cloud, CLOUD, &mut out);
    }
    if let Some(place) = table(root, "place") {
        collect("place.", place, PLACE, &mut out);
    }
    if let Some(studio) = table(root, "studio") {
        collect("studio.", studio, STUDIO, &mut out);
    }
    if let Some(suites) = table(root, "suites") {
        for (name, suite) in suites {
            let Some(suite) = suite.as_table() else {
                continue;
            };
            collect(&format!("suites.{name}."), suite, SUITE, &mut out);
            if let Some(cloud) = table(suite, "cloud") {
                collect(&format!("suites.{name}.cloud."), cloud, CLOUD, &mut out);
            }
            if let Some(place) = table(suite, "place") {
                collect(&format!("suites.{name}.place."), place, PLACE, &mut out);
            }
        }
    }
    out
}

fn resolve_raw(raw: RawConfig) -> Result<Config, ToolError> {
    // `[settings] backend` is the home; the bare top-level key is the 0.3
    // spelling, honored underneath it. DEPRECATION(0.5): drop `raw.backend`.
    let default_backend = raw
        .settings
        .backend
        .or(raw.backend)
        .unwrap_or(BackendKind::Native);
    // Top-level place, field by field: `[place]` wins over the deprecated
    // `[cloud]` spelling (and `[settings] rojo`). DEPRECATION(0.5): drop
    // the `raw.cloud` / `raw.settings.rojo` fallbacks.
    let top = PlaceTarget {
        universe_id: raw
            .place
            .universe_id
            .clone()
            .or_else(|| raw.cloud.universe_id.clone())
            .map(CloudId::into_string),
        place_id: raw
            .place
            .place_id
            .clone()
            .or_else(|| raw.cloud.place_id.clone())
            .map(CloudId::into_string),
        file: raw
            .place
            .file
            .clone()
            .or_else(|| raw.cloud.place_file.clone()),
        rojo: raw.place.rojo.clone().or_else(|| raw.settings.rojo.clone()),
    };

    let mut suites: Vec<Suite> = raw
        .suites
        .into_iter()
        .map(|(name, suite)| {
            // Per-suite `[suites.X.place]` wins over the deprecated
            // `[suites.X.cloud]`, then the top-level place, field by field.
            // DEPRECATION(0.5): drop the `suite.cloud` fallbacks.
            let place = PlaceTarget {
                universe_id: suite
                    .place
                    .universe_id
                    .or(suite.cloud.universe_id)
                    .map(CloudId::into_string)
                    .or_else(|| top.universe_id.clone()),
                place_id: suite
                    .place
                    .place_id
                    .or(suite.cloud.place_id)
                    .map(CloudId::into_string)
                    .or_else(|| top.place_id.clone()),
                file: suite
                    .place
                    .file
                    .or(suite.cloud.place_file)
                    .or_else(|| top.file.clone()),
                rojo: suite.place.rojo.or_else(|| top.rojo.clone()),
            };
            Suite {
                name,
                include: suite.include,
                backend: suite.backend.unwrap_or(default_backend),
                default_enabled: suite.default.unwrap_or(true),
                place,
            }
        })
        .collect();

    for suite in &suites {
        if suite.include.is_empty() {
            return Err(ToolError(format!(
                "suite \"{}\" has an empty `include` list",
                suite.name
            )));
        }
    }

    if suites.is_empty() {
        suites.push(Suite {
            name: "specs".to_string(),
            include: vec!["**/*.spec.luau".to_string()],
            backend: default_backend,
            default_enabled: true,
            place: top.clone(),
        });
    }

    if raw.coverage.include.as_ref().is_some_and(Vec::is_empty) {
        return Err(ToolError(
            "`[coverage] include` is an empty list; remove the key to report every covered file"
                .to_string(),
        ));
    }

    let coverage = Coverage {
        include: raw.coverage.include,
        exclude: raw.coverage.exclude.unwrap_or_else(|| {
            DEFAULT_COVERAGE_EXCLUDE
                .iter()
                .map(|s| s.to_string())
                .collect()
        }),
        min: raw.coverage.min,
    };

    Ok(Config {
        suites,
        timeout: Duration::from_millis(raw.settings.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
        workers: raw.settings.workers.unwrap_or(0),
        core: raw.settings.core,
        coverage,
        studio_executable: raw.studio.executable,
        // Filled in by `load`, which is the only place that knows the path.
        file: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        resolve_raw(toml::from_str(text).unwrap()).unwrap()
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let config = parse(
            r#"
            [suites.unit]
            include = ["src/**/*.spec.luau"]
            "#,
        );
        assert_eq!(config.suites.len(), 1);
        let suite = &config.suites[0];
        assert_eq!(suite.name, "unit");
        assert_eq!(suite.backend, BackendKind::Native);
        assert!(suite.default_enabled);
        assert_eq!(config.timeout, Duration::from_millis(5000));
    }

    #[test]
    fn per_suite_backend_overrides_top_level_default() {
        let config = parse(
            r#"
            backend = "native"

            [suites.unit]
            include = ["src/**"]

            [suites.scripts]
            include = ["tests/scripts/**"]
            backend = "lute"

            [suites.engine]
            include = ["tests/engine/**"]
            backend = "cloud"
            default = false
            "#,
        );
        assert_eq!(config.suites[0].backend, BackendKind::Native);
        assert_eq!(config.suites[1].backend, BackendKind::Lute);
        assert_eq!(config.suites[2].backend, BackendKind::Cloud);
        assert!(!config.suites[2].default_enabled);
    }

    #[test]
    fn empty_config_synthesizes_default_suite() {
        let config = parse("");
        assert_eq!(config.suites.len(), 1);
        assert_eq!(config.suites[0].include, vec!["**/*.spec.luau"]);
    }

    #[test]
    fn suite_order_follows_the_file() {
        let config = parse(
            r#"
            [suites.zeta]
            include = ["z/**"]

            [suites.alpha]
            include = ["a/**"]
            "#,
        );
        let names: Vec<_> = config.suites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["zeta", "alpha"]);
    }

    #[test]
    fn core_defaults_to_the_embedded_framework() {
        assert_eq!(parse("").core, None);
        let configured = parse(
            r#"
            [settings]
            core = "luau/core"
            "#,
        );
        assert_eq!(configured.core.as_deref(), Some("luau/core"));
    }

    #[test]
    fn every_documented_setting_parses() {
        let config = parse(
            r#"
            [suites.unit]
            include = ["src/**"]

            [settings]
            timeout_ms = 1000
            workers = 4
            rojo = "default.project.json"

            [coverage]
            include = ["src/**"]
            exclude = ["Packages/**"]
            min = 80
            "#,
        );
        assert_eq!(config.timeout, Duration::from_millis(1000));
        assert_eq!(config.workers, 4);
        assert_eq!(config.coverage.min, Some(80.0));
        assert_eq!(
            config.coverage.include.as_deref(),
            Some(&["src/**".to_string()][..])
        );
        assert_eq!(config.coverage.exclude, vec!["Packages/**"]);
    }

    /// Absent `include` must stay absent rather than defaulting to something
    /// broad: `None` is what tells the coverage filter not to narrow at all.
    #[test]
    fn include_is_unset_by_default() {
        let config = parse("[coverage]\nmin = 80\n");
        assert_eq!(config.coverage.include, None);
        assert_eq!(config.coverage.exclude, DEFAULT_COVERAGE_EXCLUDE);
    }

    /// `include = []` reads as "cover nothing" and as "cover everything"
    /// equally well; rejecting it keeps a config from silently meaning the
    /// opposite of what it says. `exclude = []` stays legal — an empty
    /// exclusion list has one obvious reading.
    #[test]
    fn empty_coverage_include_is_rejected() {
        let err = resolve_raw(toml::from_str("[coverage]\ninclude = []\n").unwrap()).unwrap_err();
        assert!(err.0.contains("`[coverage] include`"), "{}", err.0);
    }

    /// DEPRECATION(0.5): exercises deprecated spellings; rewrite or delete
    /// with the fallbacks.
    #[test]
    fn place_file_inherits_top_level_and_suite_override_wins() {
        let config = parse(
            r#"
            [suites.engine]
            include = ["tests/engine/**"]
            backend = "cloud"

            [suites.other]
            include = ["tests/other/**"]
            backend = "cloud"

            [suites.other.cloud]
            place_file = "other-place.rbxl"

            [cloud]
            universe_id = 1
            place_id = 2
            place_file = "test-place.rbxl"
            "#,
        );
        let engine = config.suites.iter().find(|s| s.name == "engine").unwrap();
        assert_eq!(engine.place.file.as_deref(), Some("test-place.rbxl"));
        let other = config.suites.iter().find(|s| s.name == "other").unwrap();
        assert_eq!(other.place.file.as_deref(), Some("other-place.rbxl"));
    }

    /// `[settings] rojo` was warned about while it was accepted-but-unconsumed;
    /// now that the cloud backend consumes it, setting it must be silent.
    /// DEPRECATION(0.5): exercises deprecated spellings; rewrite or delete
    /// with the fallbacks.
    #[test]
    fn a_deprecated_rojo_key_is_honored_and_warned() {
        // DEPRECATION(0.5): delete this test with the fallback it covers.
        let text = "[settings]\nrojo = \"default.project.json\"\n";
        let raw: RawConfig = toml::from_str(text).unwrap();
        let warnings = config_warnings(text, &raw, Path::new("lest.toml"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("[settings] rojo"));
        assert!(warnings[0].contains("[place] rojo"));
        assert_eq!(
            parse(text).suites[0].place.rojo.as_deref(),
            Some("default.project.json"),
            "the deprecated spelling must still be honored in 0.4"
        );
    }

    #[test]
    fn suite_place_overrides_everything_field_by_field() {
        let config = parse(
            r#"
            [suites.engine]
            include = ["tests/engine/**"]

            [suites.engine.place]
            place_id = 9
            rojo = "engine.project.json"

            [place]
            universe_id = 1
            place_id = 2
            file = "top.rbxl"
            rojo = "top.project.json"
        "#,
        );
        let place = &config.suites[0].place;
        // Set per-suite: wins. Unset per-suite: inherits the top place.
        assert_eq!(place.place_id.as_deref(), Some("9"));
        assert_eq!(place.rojo.as_deref(), Some("engine.project.json"));
        assert_eq!(place.universe_id.as_deref(), Some("1"));
        assert_eq!(place.file.as_deref(), Some("top.rbxl"));
    }

    /// DEPRECATION(0.5): the suite.cloud half of this test dies with the
    /// fallbacks; keep the suite.place half.
    #[test]
    fn suite_place_beats_suite_cloud_which_beats_top() {
        let config = parse(
            r#"
            [suites.engine]
            include = ["tests/engine/**"]

            [suites.engine.place]
            universe_id = 100

            [suites.engine.cloud]
            universe_id = 200
            place_id = 201

            [place]
            universe_id = 300
            place_id = 301
            file = "top.rbxl"
        "#,
        );
        let place = &config.suites[0].place;
        // place > cloud at the suite level; cloud still backfills what
        // place leaves unset; top backfills the rest.
        assert_eq!(place.universe_id.as_deref(), Some("100"));
        assert_eq!(place.place_id.as_deref(), Some("201"));
        assert_eq!(place.file.as_deref(), Some("top.rbxl"));
    }

    #[test]
    fn place_ids_beat_cloud_ids_and_place_rojo_beats_settings_rojo() {
        // DEPRECATION(0.5): delete this test with the fallbacks it covers.
        let config = parse(
            r#"
            [suites.engine]
            include = ["tests/engine/**"]

            [cloud]
            universe_id = 1
            place_id = 2

            [place]
            universe_id = 11
            place_id = 22
            rojo = "new.project.json"

            [settings]
            rojo = "old.project.json"
        "#,
        );
        let place = &config.suites[0].place;
        assert_eq!(place.universe_id.as_deref(), Some("11"));
        assert_eq!(place.place_id.as_deref(), Some("22"));
        assert_eq!(place.rojo.as_deref(), Some("new.project.json"));
    }

    #[test]
    fn the_new_place_table_earns_no_warnings_and_wins_over_old_spellings() {
        let text = r#"
            [suites.engine]
            include = ["tests/engine/**"]

            [place]
            universe_id = 1
            place_id = 2
            file = "new-place.rbxl"
            rojo = "new.project.json"
        "#;
        let raw: RawConfig = toml::from_str(text).unwrap();
        assert!(config_warnings(text, &raw, Path::new("lest.toml")).is_empty());
        let config = parse(text);
        let place = &config.suites[0].place;
        assert_eq!(place.universe_id.as_deref(), Some("1"));
        assert_eq!(place.place_id.as_deref(), Some("2"));
        assert_eq!(place.file.as_deref(), Some("new-place.rbxl"));
        assert_eq!(place.rojo.as_deref(), Some("new.project.json"));

        // DEPRECATION(0.5): the second half of this test dies with the
        // fallbacks. Old and new present together: new wins, old warns.
        let both = r#"
            [suites.engine]
            include = ["tests/engine/**"]

            [cloud]
            place_file = "old-place.rbxl"

            [place]
            file = "new-place.rbxl"
        "#;
        let raw: RawConfig = toml::from_str(both).unwrap();
        let warnings = config_warnings(both, &raw, Path::new("lest.toml"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("[cloud] place_file"));
        assert_eq!(
            parse(both).suites[0].place.file.as_deref(),
            Some("new-place.rbxl")
        );
    }

    #[test]
    fn deprecated_spellings_each_warn_with_their_new_home() {
        // DEPRECATION(0.5): delete this test with the fallbacks it covers.
        let text = r#"
            backend = "lune"

            [suites.engine]
            include = ["tests/engine/**"]

            [suites.engine.cloud]
            universe_id = 1

            [cloud]
            universe_id = 1
            place_id = 2
            place_file = "p.rbxl"

            [settings]
            rojo = "default.project.json"
        "#;
        let raw: RawConfig = toml::from_str(text).unwrap();
        let warnings = config_warnings(text, &raw, Path::new("lest.toml"));
        let all = warnings.join("\n");
        assert!(all.contains("`backend` was renamed"));
        assert!(all.contains("[settings] backend"));
        assert!(all.contains("[place] universe_id"));
        assert!(all.contains("[place] place_id"));
        assert!(all.contains("[place] file"));
        assert!(all.contains("[place] rojo"));
        assert!(all.contains("[suites.engine.place]"));
        // Honored: the old spellings still resolve.
        let config = parse(text);
        assert_eq!(config.suites[0].backend, BackendKind::Lune);
        assert_eq!(config.suites[0].place.file.as_deref(), Some("p.rbxl"));
    }

    #[test]
    fn settings_backend_is_the_default_and_outranks_the_top_level_spelling() {
        let config = parse(
            r#"
            backend = "lune"

            [suites.unit]
            include = ["src/**/*.spec.luau"]

            [settings]
            backend = "lute"
            "#,
        );
        assert_eq!(config.suites[0].backend, BackendKind::Lute);
    }

    #[test]
    fn studio_executable_is_parsed_and_defaults_to_none() {
        let config = parse(
            r#"
            [suites.unit]
            include = ["src/**/*.spec.luau"]

            [studio]
            executable = "C:/Tools/RobloxStudioBeta.exe"
            "#,
        );
        assert_eq!(
            config.studio_executable.as_deref(),
            Some("C:/Tools/RobloxStudioBeta.exe")
        );
        let config = parse(
            r#"
            [suites.unit]
            include = ["src/**/*.spec.luau"]
            "#,
        );
        assert_eq!(config.studio_executable, None);
    }

    #[test]
    fn studio_table_keys_are_checked() {
        let found = unknown_keys(
            r#"
            [studio]
            excutable = "x"
            "#,
        );
        assert_eq!(found, vec!["studio.excutable".to_string()]);
    }

    /// A typo'd key parses fine and does nothing, which is the failure mode
    /// worth naming: `bakcend` runs the suite on native, `deafult` leaves a
    /// cloud suite enabled, and neither looks wrong.
    #[test]
    fn unknown_keys_are_tolerated_but_named() {
        let found = unknown_keys(
            r#"
            bakcend = "lune"

            [suites.engine]
            include = ["tests/engine/**"]
            deafult = false

            [suites.engine.cloud]
            univese_id = 1

            [settings]
            timeout_ms = 1000
            wrokers = 4

            [coverage]
            mim = 80
            "#,
        );
        assert_eq!(
            found,
            [
                "bakcend",
                "settings.wrokers",
                "coverage.mim",
                "suites.engine.deafult",
                "suites.engine.cloud.univese_id",
            ]
        );
        // The warning body is a lowercase fragment — `render_warning`
        // capitalizes it and pluralizes never; the count decides the noun.
        assert_eq!(
            unknown_keys_message(&found, Path::new("lest.toml")),
            "ignoring unrecognized keys in lest.toml: bakcend, settings.wrokers, coverage.mim, \
             suites.engine.deafult, suites.engine.cloud.univese_id"
        );
        assert_eq!(
            unknown_keys_message(&["bakcend".to_string()], Path::new("lest.toml")),
            "ignoring unrecognized key in lest.toml: bakcend"
        );
        // Everything lest documents is recognized, so a correct config is quiet.
        assert!(unknown_keys(
            r#"
            backend = "native"

            [suites.unit]
            include = ["src/**"]
            backend = "lune"
            default = false

            [suites.unit.cloud]
            universe_id = 1
            place_id = 2

            [cloud]
            universe_id = 1
            place_id = 2

            [settings]
            timeout_ms = 1
            workers = 0
            rojo = "default.project.json"
            core = "luau/core"

            [coverage]
            include = ["src/**"]
            exclude = []
            min = 0
            "#
        )
        .is_empty());
    }

    #[test]
    fn cloud_ids_inherit_top_level_and_suite_overrides_win() {
        let config = parse(
            r#"
            [cloud]
            universe_id = 10469641725
            place_id = "102831964562199"

            [suites.engine]
            include = ["tests/engine/**"]
            backend = "cloud"
            default = false

            [suites.other]
            include = ["tests/other/**"]
            backend = "cloud"

            [suites.other.cloud]
            place_id = "999"
            "#,
        );
        let engine = config.suites.iter().find(|s| s.name == "engine").unwrap();
        assert_eq!(engine.place.universe_id.as_deref(), Some("10469641725"));
        assert_eq!(engine.place.place_id.as_deref(), Some("102831964562199"));

        // Per-suite `place_id` overrides the top-level; `universe_id` still
        // inherits.
        let other = config.suites.iter().find(|s| s.name == "other").unwrap();
        assert_eq!(other.place.universe_id.as_deref(), Some("10469641725"));
        assert_eq!(other.place.place_id.as_deref(), Some("999"));
    }

    #[test]
    fn empty_include_is_rejected() {
        let raw: RawConfig = toml::from_str(
            r#"
            [suites.unit]
            include = []
            "#,
        )
        .unwrap();
        assert!(resolve_raw(raw).is_err());
    }
}
