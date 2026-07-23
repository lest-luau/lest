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
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            BackendKind::Native => "native",
            BackendKind::Lune => "lune",
            BackendKind::Lute => "lute",
            BackendKind::Cloud => "cloud",
        };
        f.write_str(name)
    }
}

/// `lest.toml` as written by the user. Unknown keys are tolerated so configs
/// written for later versions still parse — but tolerated is not the same as
/// unmentioned, so [`unknown_keys`] names them back to the reader.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    backend: Option<BackendKind>,
    #[serde(default)]
    suites: IndexMap<String, RawSuite>,
    #[serde(default)]
    settings: RawSettings,
    #[serde(default)]
    coverage: RawCoverage,
    #[serde(default)]
    cloud: RawCloud,
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
    exclude: Option<Vec<String>>,
    min: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawSuite {
    include: Vec<String>,
    backend: Option<BackendKind>,
    default: Option<bool>,
    /// Per-suite Open Cloud target, overriding the top-level `[cloud]` block.
    #[serde(default)]
    cloud: RawCloud,
}

#[derive(Debug, Default, Deserialize)]
struct RawSettings {
    timeout_ms: Option<u64>,
    workers: Option<usize>,
    /// Rojo project file (root-relative) describing how the filesystem maps
    /// into the place. Consumed by the cloud backend: string requires whose
    /// targets it maps to place ModuleScripts delegate to the engine's
    /// `require` instead of bundling a private copy.
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
    /// Per-suite Open Cloud overrides; falls back to the top-level `[cloud]`
    /// block when a field is unset. Only consulted for cloud-backend suites.
    pub cloud: CloudTarget,
}

/// Open Cloud identifiers resolved for a suite (per-suite overriding
/// top-level). Either id may still be `None` when nothing supplied it; the
/// cloud backend turns a missing id into a clear tool error at run time. Never
/// holds the API key — that is environment-only.
#[derive(Debug, Clone, Default)]
pub struct CloudTarget {
    pub universe_id: Option<String>,
    pub place_id: Option<String>,
    /// Root-relative path to a place file to upload (hash-skipped) and pin
    /// tasks to. `None` means run against the place's latest version.
    pub place_file: Option<String>,
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
    /// Root-relative rojo project file (`[settings] rojo`), consumed by the
    /// cloud backend for place mapping.
    pub rojo: Option<String>,
    /// Coverage settings (native suites only).
    pub coverage: Coverage,
    /// The `lest.toml` this config was read from, or `None` in zero-config
    /// mode. Carried so callers can point at the real file (watch mode watches
    /// it by identity; the empty-discovery message only mentions a config file
    /// when one exists).
    pub file: Option<PathBuf>,
}

/// Line-coverage configuration. `exclude` globs are matched against the
/// root-relative, forward-slashed spec/source path; `min` gates CI when set.
#[derive(Debug, Clone)]
pub struct Coverage {
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
    // `raw` is unused today but stays: warnings about parsed-but-unconsumed
    // keys read it, and the signature is the seam they return through.
    let _ = raw;
    let mut warnings = Vec::new();
    let unknown = unknown_keys(text);
    if !unknown.is_empty() {
        warnings.push(unknown_keys_message(&unknown, path));
    }
    warnings
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
    const TOP: &[&str] = &["backend", "suites", "settings", "coverage", "cloud"];
    const SUITE: &[&str] = &["include", "backend", "default", "cloud"];
    const SETTINGS: &[&str] = &["timeout_ms", "workers", "rojo", "core"];
    const COVERAGE: &[&str] = &["exclude", "min"];
    const CLOUD: &[&str] = &["universe_id", "place_id", "place_file"];

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
    if let Some(suites) = table(root, "suites") {
        for (name, suite) in suites {
            let Some(suite) = suite.as_table() else {
                continue;
            };
            collect(&format!("suites.{name}."), suite, SUITE, &mut out);
            if let Some(cloud) = table(suite, "cloud") {
                collect(&format!("suites.{name}.cloud."), cloud, CLOUD, &mut out);
            }
        }
    }
    out
}

fn resolve_raw(raw: RawConfig) -> Result<Config, ToolError> {
    let default_backend = raw.backend.unwrap_or(BackendKind::Native);
    let top_universe = raw.cloud.universe_id.clone().map(CloudId::into_string);
    let top_place = raw.cloud.place_id.clone().map(CloudId::into_string);
    let top_place_file = raw.cloud.place_file.clone();

    let mut suites: Vec<Suite> = raw
        .suites
        .into_iter()
        .map(|(name, suite)| {
            // Per-suite value wins; otherwise inherit the top-level `[cloud]`.
            let universe_id = suite
                .cloud
                .universe_id
                .map(CloudId::into_string)
                .or_else(|| top_universe.clone());
            let place_id = suite
                .cloud
                .place_id
                .map(CloudId::into_string)
                .or_else(|| top_place.clone());
            let place_file = suite.cloud.place_file.or_else(|| top_place_file.clone());
            Suite {
                name,
                include: suite.include,
                backend: suite.backend.unwrap_or(default_backend),
                default_enabled: suite.default.unwrap_or(true),
                cloud: CloudTarget {
                    universe_id,
                    place_id,
                    place_file,
                },
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
            cloud: CloudTarget {
                universe_id: top_universe,
                place_id: top_place,
                place_file: top_place_file,
            },
        });
    }

    let coverage = Coverage {
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
        rojo: raw.settings.rojo,
        coverage,
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
            exclude = ["Packages/**"]
            min = 80
            "#,
        );
        assert_eq!(config.timeout, Duration::from_millis(1000));
        assert_eq!(config.workers, 4);
        assert_eq!(config.coverage.min, Some(80.0));
        assert_eq!(config.coverage.exclude, vec!["Packages/**"]);
    }

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
        assert_eq!(engine.cloud.place_file.as_deref(), Some("test-place.rbxl"));
        let other = config.suites.iter().find(|s| s.name == "other").unwrap();
        assert_eq!(other.cloud.place_file.as_deref(), Some("other-place.rbxl"));
    }

    /// `[settings] rojo` was warned about while it was accepted-but-unconsumed;
    /// now that the cloud backend consumes it, setting it must be silent.
    #[test]
    fn a_set_rojo_key_is_consumed_and_earns_no_warning() {
        let text = "[settings]\nrojo = \"default.project.json\"\n";
        let raw: RawConfig = toml::from_str(text).unwrap();
        assert!(config_warnings(text, &raw, Path::new("lest.toml")).is_empty());
        assert_eq!(
            parse(text).rojo.as_deref(),
            Some("default.project.json"),
            "the key must land in the resolved config"
        );
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
        assert_eq!(engine.cloud.universe_id.as_deref(), Some("10469641725"));
        assert_eq!(engine.cloud.place_id.as_deref(), Some("102831964562199"));

        // Per-suite `place_id` overrides the top-level; `universe_id` still
        // inherits.
        let other = config.suites.iter().find(|s| s.name == "other").unwrap();
        assert_eq!(other.cloud.universe_id.as_deref(), Some("10469641725"));
        assert_eq!(other.cloud.place_id.as_deref(), Some("999"));
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
