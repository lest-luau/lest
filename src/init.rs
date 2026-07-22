//! `lest init`: interactive scaffolding built on inquire. Detects what it
//! can (runtimes on PATH, a rojo project file, existing specs), asks only
//! what it cannot infer, and accepts every default with `--yes` for scripts
//! and CI. Re-runs are safe: an existing lest.toml is never overwritten
//! without confirmation.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use inquire::ui::{Color, ErrorMessageRenderConfig, RenderConfig, StyleSheet, Styled};
use inquire::validator::Validation;
use inquire::{Confirm, CustomUserError, Select, Text};

use crate::backend::relative_path;
use crate::config::BackendKind;
use crate::embed;
use crate::error::ToolError;

struct Detected {
    lune: bool,
    lute: bool,
    rojo: bool,
    has_specs: bool,
}

struct Answers {
    backend: BackendKind,
    suite_name: String,
    include: String,
    scripts_backend: Option<BackendKind>,
    cloud_suite: bool,
    write_example: bool,
    gitignore: bool,
    /// Add a `lest` alias to `.luaurc` so specs can `require('@lest')` and
    /// editors resolve it. Optional: it edits a file lest does not own, and
    /// declining only costs a relative require.
    luaurc_alias: bool,
}

/// inquire in the lest palette: a yellow `?` while a question is open, the
/// green `✓` and red `✗` the reporters already use for pass and fail. The
/// glyphs are swapped unconditionally; the colors only when `color` is on —
/// decided by the caller with the CLI's standard rule (`--no-color`,
/// `NO_COLOR`, and the terminal state of stderr, where inquire renders).
fn prompt_style(color: bool) -> RenderConfig<'static> {
    let plain = !color;
    let glyph = |styled: Styled<&'static str>, color: Color| {
        if plain {
            styled
        } else {
            styled.with_fg(color)
        }
    };
    let sheet = |color: Color| {
        if plain {
            StyleSheet::empty()
        } else {
            StyleSheet::empty().with_fg(color)
        }
    };

    // DarkYellow/DarkGreen/DarkRed are ANSI 33/32/31 — the exact codes in
    // `report::pretty`, so a prompt and a test result are the same red.
    //
    // Recessive text is DarkGrey rather than the reporters' ANSI-2 dim:
    // inquire's stylesheets carry a foreground color and BOLD/ITALIC only, so
    // the dim *attribute* is not expressible here. DarkGrey reads the same way.
    RenderConfig::default()
        .with_prompt_prefix(glyph(Styled::new("?"), Color::DarkYellow))
        .with_answered_prompt_prefix(glyph(Styled::new("✓"), Color::DarkGreen))
        .with_help_message(sheet(Color::DarkGrey))
        // The suggested value and the `(y/n)` hint are prompts to accept, not
        // answers already given, so they recede with the help line.
        .with_default_value(sheet(Color::DarkGrey))
        // Options recede so the highlighted one is the only line at full
        // strength — the cursor is then legible without color carrying it.
        .with_option(sheet(Color::DarkGrey))
        .with_selected_option(Some(StyleSheet::empty()))
        .with_highlighted_option_prefix(Styled::new(">"))
        .with_error_message(
            ErrorMessageRenderConfig::empty()
                .with_prefix(glyph(Styled::new("✗"), Color::DarkRed))
                .with_message(sheet(Color::DarkRed)),
        )
}

/// `no_color` is the `--no-color` flag, wired through from the CLI; the
/// effective off-switch adds `NO_COLOR` and stderr's terminal state (inquire
/// renders its prompts on stderr). Init's own confirmation lines are plain
/// text on stdout — no styling to switch off there.
pub fn run(yes: bool, no_color: bool) -> Result<(), ToolError> {
    let cwd = std::env::current_dir()
        .map_err(|e| ToolError(format!("cannot determine working directory: {e}")))?;
    let config_path = cwd.join("lest.toml");
    let color =
        !no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal();
    inquire::set_global_render_config(prompt_style(color));

    if config_path.exists() {
        if yes {
            // Starts with an article so the diagnostic renderer's capitalizer
            // never uppercases the deliberately lowercase file name.
            return Err(ToolError(
                "a lest.toml already exists — refusing to overwrite it with --yes".to_string(),
            ));
        }
        // The brand is deliberately lowercase everywhere, so the sentence must
        // not open with it.
        let overwrite = Confirm::new("A lest.toml already exists. Overwrite it?")
            .with_help_message("lest.toml holds your suites and settings; overwriting replaces it")
            .with_default(false)
            .prompt()
            .map_err(prompt_err)?;
        if !overwrite {
            println!("Left the existing lest.toml untouched.");
            return Ok(());
        }
    }

    // Under `--yes` the runtime questions are never asked, so the probes that
    // feed them are skipped; `defaults` still reads `rojo` and `has_specs`.
    let detected = detect(&cwd, !yes);
    let answers = if yes {
        defaults(&detected)
    } else {
        ask(&detected)?
    };

    std::fs::write(&config_path, render_config(&answers))
        .map_err(|e| ToolError(format!("cannot write {}: {e}", config_path.display())))?;
    println!("Wrote the lest.toml manifest.");

    // Written eagerly rather than left to the first run: the example spec
    // below requires it, and an editor should resolve that the moment init
    // finishes.
    embed::write(&cwd)?;
    println!(
        "Wrote {} — the framework, from this copy of lest.",
        embed::CORE_DIR
    );

    if answers.gitignore {
        ensure_gitignore(&cwd)?;
    }

    // Whether `@lest` actually resolves to what was just written — not whether
    // the user asked for it. Every bail-out inside leaves the file alone, and
    // an example spec requiring an alias that is not there fails on its first
    // run, which is the worst possible first impression.
    let alias_active = answers.luaurc_alias && ensure_luaurc_alias(&cwd)?;

    if answers.write_example {
        write_example(&cwd, &answers, alias_active)?;
    }

    println!("\nDone. Run `lest` to run your suites.");
    if answers.gitignore {
        println!(
            "Note: {} is generated and gitignored — a fresh clone gets it back by running `lest`.",
            embed::CORE_DIR
        );
    }
    Ok(())
}

fn prompt_err(err: inquire::InquireError) -> ToolError {
    ToolError(format!(
        "{err} — rerun with `lest init --yes` for non-interactive defaults"
    ))
}

fn detect(cwd: &Path, probe_runtimes: bool) -> Detected {
    Detected {
        // Each probe spawns a `--version` process; skipped when the caller
        // (`--yes`) will never read the answers.
        lune: probe_runtimes && runtime_available("lune"),
        lute: probe_runtimes && runtime_available("lute"),
        rojo: cwd.join("default.project.json").is_file(),
        has_specs: crate::discover::discover(cwd, &["**/*.spec.luau".to_string()])
            .map(|specs| !specs.is_empty())
            .unwrap_or(false),
    }
}

fn runtime_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn defaults(detected: &Detected) -> Answers {
    Answers {
        backend: BackendKind::Native,
        suite_name: "unit".to_string(),
        // `--yes` adds no extra suites, so the zero-config glob cannot overlap.
        include: default_include(detected.rojo, false).to_string(),
        scripts_backend: None,
        cloud_suite: false,
        write_example: !detected.has_specs,
        gitignore: true,
        luaurc_alias: true,
    }
}

/// The pre-filled main include. Scoped to `src/` when a rojo project makes
/// that the obvious home — or when an extra suite was chosen, because the
/// zero-config `**/*.spec.luau` also matches `tests/scripts/**` and
/// `tests/engine/**`, and a spec matched by two suites runs in both, failing
/// in the one whose backend cannot host it.
fn default_include(rojo: bool, extra_suites: bool) -> &'static str {
    if rojo || extra_suites {
        "src/**/*.spec.luau"
    } else {
        "**/*.spec.luau"
    }
}

fn ask(detected: &Detected) -> Result<Answers, ToolError> {
    // The options are bare backend names — what goes in the config, nothing
    // else. What is missing from PATH rides along in the help line instead of
    // ragging the list out.
    let absent: Vec<&str> = [("lune", detected.lune), ("lute", detected.lute)]
        .iter()
        .filter(|(_, found)| !found)
        .map(|(name, _)| *name)
        .collect();
    let backend_help = if absent.is_empty() {
        "Suites that name no backend of their own run on this one".to_string()
    } else {
        format!(
            "Suites that name no backend of their own run on this one; {} not on PATH",
            absent.join(" and ")
        )
    };
    let backend_choice = Select::new(
        "Enter the default backend for suites:",
        vec!["native", "lune", "lute", "cloud"],
    )
    .with_help_message(&backend_help)
    .prompt()
    .map_err(prompt_err)?;
    let backend = match backend_choice {
        "lune" => BackendKind::Lune,
        "lute" => BackendKind::Lute,
        "cloud" => BackendKind::Cloud,
        _ => BackendKind::Native,
    };

    let suite_name = Text::new("Enter the name of the main suite:")
        .with_help_message("Run it on its own with `lest run <name>`")
        .with_default("unit")
        .with_validator(validate_suite_name)
        .prompt()
        .map_err(prompt_err)?;

    let scripts_backend = if backend == BackendKind::Native && (detected.lune || detected.lute) {
        // Both runtimes present means the question cannot name one, so the
        // follow-up asks; otherwise the only candidate goes in the question.
        let both = detected.lune && detected.lute;
        let runtime = if detected.lute { "lute" } else { "lune" };
        let question = if both {
            "Add a scripts suite?".to_string()
        } else {
            format!("Add a scripts suite using {runtime}?")
        };
        let wanted = Confirm::new(&question)
            .with_help_message(
                "Specs in tests/scripts/** run inside the real runtime, with its APIs available",
            )
            .with_default(false)
            .prompt()
            .map_err(prompt_err)?;
        if !wanted {
            None
        } else if both {
            let runtime = Select::new(
                "Enter the runtime for the scripts suite:",
                vec!["lute", "lune"],
            )
            .with_help_message("The suite is spawned with this runtime's own binary")
            .prompt()
            .map_err(prompt_err)?;
            Some(if runtime == "lune" {
                BackendKind::Lune
            } else {
                BackendKind::Lute
            })
        } else if detected.lute {
            Some(BackendKind::Lute)
        } else {
            Some(BackendKind::Lune)
        }
    } else {
        None
    };

    let cloud_suite = if backend == BackendKind::Cloud {
        false
    } else {
        Confirm::new("Add an opt-in cloud engine suite?")
            .with_help_message(
                "Specs in tests/engine/** run in the real engine; opt in by name, or let CI run them",
            )
            .with_default(false)
            .prompt()
            .map_err(prompt_err)?
    };

    // Asked *after* the extra suites so both the pre-fill and the validator
    // can keep the main glob out of their directories — a spec matched by two
    // suites runs in both, and no generated config may overlap.
    let has_scripts = scripts_backend.is_some();
    let include = Text::new("Enter the glob for the main suite:")
        .with_help_message("Relative to the project root; `**/` matches any depth, including none")
        .with_default(default_include(detected.rojo, has_scripts || cloud_suite))
        .with_validator(validate_include)
        .with_validator(move |input: &str| {
            validate_no_suite_overlap(input, has_scripts, cloud_suite)
        })
        .prompt()
        .map_err(prompt_err)?;

    let luaurc_alias = Confirm::new("Add a lest alias to .luaurc?")
        .with_help_message(
            "Lets any spec `require('@lest')` instead of a relative path, and editors follow it",
        )
        .with_default(true)
        .prompt()
        .map_err(prompt_err)?;

    let write_example = Confirm::new("Add an example spec?")
        .with_help_message("Writes a passing example.spec.luau where your include glob points")
        .with_default(!detected.has_specs)
        .prompt()
        .map_err(prompt_err)?;

    let gitignore = Confirm::new("Add /.lest to your .gitignore?")
        .with_help_message("/.lest is generated; a fresh clone gets it back on the next run")
        .with_default(true)
        .prompt()
        .map_err(prompt_err)?;

    Ok(Answers {
        backend,
        suite_name,
        include,
        scripts_backend,
        cloud_suite,
        write_example,
        gitignore,
        luaurc_alias,
    })
}

/// The suite name becomes a TOML bare key in a `[suites.<name>]` header, so
/// anything outside the bare-key charset writes a lest.toml that will not parse
/// on the very next run — `my suite` and `` both break it. Caught at the prompt,
/// where the user can still fix it, rather than an hour later.
fn validate_suite_name(input: &str) -> Result<Validation, CustomUserError> {
    if input.is_empty() {
        return Ok(Validation::Invalid("The suite name cannot be empty".into()));
    }
    let bare = input
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !bare {
        return Ok(Validation::Invalid(
            "The name becomes a TOML key — use letters, digits, `_` or `-`".into(),
        ));
    }
    Ok(Validation::Valid)
}

/// An empty glob yields `include = [""]`, which parses and then matches
/// nothing — a suite that reports no specs on every run, for no visible reason.
/// A backslash-separated glob is just as silent: globset treats `\` as an
/// escape, so `src\**\*.spec.luau` is accepted and then never matches anything.
fn validate_include(input: &str) -> Result<Validation, CustomUserError> {
    if input.trim().is_empty() {
        return Ok(Validation::Invalid(
            "The include glob cannot be empty — try `**/*.spec.luau`".into(),
        ));
    }
    if input.contains('\\') {
        return Ok(Validation::Invalid(
            "Use `/` as the path separator — `\\` is a glob escape, so this pattern would never \
             match a file"
                .into(),
        ));
    }
    Ok(Validation::Valid)
}

/// Directories the optional extra suites claim; their generated globs are
/// `<dir>/**/*.spec.luau`. Shared between the config renderer and the overlap
/// validator so the two cannot drift.
const SCRIPTS_DIR: &str = "tests/scripts";
const ENGINE_DIR: &str = "tests/engine";

/// Rejects a main-suite glob that can reach a chosen extra suite's directory.
/// A spec matched by two suites runs in both — and fails in the one whose
/// backend cannot host it — so a generated config must not overlap by
/// construction.
fn validate_no_suite_overlap(
    input: &str,
    scripts: bool,
    engine: bool,
) -> Result<Validation, CustomUserError> {
    let extras = [
        (scripts, SCRIPTS_DIR, "scripts"),
        (engine, ENGINE_DIR, "engine"),
    ];
    for (chosen, dir, suite) in extras {
        if chosen && glob_reaches_dir(input, dir) {
            return Ok(Validation::Invalid(
                format!(
                    "This glob also matches specs under {dir}/, which belong to the {suite} \
                     suite — a spec matched by two suites runs in both; scope it away, e.g. \
                     `src/**/*.spec.luau`"
                )
                .into(),
            ));
        }
    }
    Ok(Validation::Valid)
}

/// Whether `glob` can match a spec inside `dir`. Probed at a few depths with
/// the same literal-separator semantics `discover` uses, rather than
/// intersecting glob languages: the extra suites' globs are fixed
/// (`<dir>/**/*.spec.luau`), so any realistic overlapping main glob matches
/// one of these probes.
fn glob_reaches_dir(glob: &str, dir: &str) -> bool {
    let Ok(built) = globset::GlobBuilder::new(glob)
        .literal_separator(true)
        .build()
    else {
        // A glob that will not build matches nothing, so it cannot overlap;
        // `discover` reports the build error on the first run.
        return false;
    };
    let matcher = built.compile_matcher();
    [
        format!("{dir}/example.spec.luau"),
        format!("{dir}/a/example.spec.luau"),
        format!("{dir}/a/b/example.spec.luau"),
    ]
    .iter()
    .any(|probe| matcher.is_match(probe))
}

/// Renders `value` as a TOML basic string. The glob is free text the user typed;
/// a `"` or a `\` (which a Windows-shaped path invites) pasted in raw ends the
/// string early and the generated config no longer parses. Written out rather
/// than leaned on the validators alone: the file this produces has to be valid
/// TOML by construction, not by everything upstream having been careful.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn render_config(answers: &Answers) -> String {
    let mut out = String::new();
    out.push_str("# Generated by `lest init`.\n");
    out.push_str("# Suites group spec files and pin them to the backend they need;\n");
    out.push_str("# run everything with `lest`, one suite with `lest run <name>`.\n");
    out.push_str(&format!("backend = \"{}\"\n", answers.backend));

    // The name is a bare key (the prompt validator holds it to that charset);
    // the glob is quoted and escaped, so no answer can produce a broken file.
    out.push_str(&format!(
        "\n[suites.{}]\ninclude = [{}]\n",
        answers.suite_name,
        toml_string(&answers.include)
    ));

    if let Some(scripts_backend) = answers.scripts_backend {
        out.push_str(&format!(
            "\n# Specs that need real runtime APIs run inside a spawned {scripts_backend} process.\n\
             [suites.scripts]\ninclude = [\"{SCRIPTS_DIR}/**/*.spec.luau\"]\nbackend = \"{scripts_backend}\"\n"
        ));
    }

    if answers.cloud_suite {
        out.push_str(&format!(
            "\n# Engine tests run in the real engine via Open Cloud.\n\
             [suites.engine]\ninclude = [\"{ENGINE_DIR}/**/*.spec.luau\"]\nbackend = \"cloud\"\n\
             default = false # opt-in locally; auto-enabled in CI\n"
        ));
    }

    out.push_str(
        "\n[settings]\ntimeout_ms = 5000\n\
         # The framework ships inside the lest binary and is written to .lest/core.\n\
         # Set `core = \"path/to/core/src\"` only to point at your own copy instead.\n",
    );
    out
}

/// Appends `/.lest` to .gitignore, creating the file when absent. Idempotent.
fn ensure_gitignore(cwd: &Path) -> Result<(), ToolError> {
    let path = cwd.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(ToolError(format!("cannot read {}: {err}", path.display()))),
    };
    let already_ignored = existing
        .lines()
        .any(|line| matches!(line.trim(), ".lest" | ".lest/" | "/.lest" | "/.lest/"));
    if already_ignored {
        println!("Your .gitignore already covers /.lest.");
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("/.lest\n");
    std::fs::write(&path, updated)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", path.display())))?;
    println!("Added /.lest to your .gitignore.");
    Ok(())
}

/// Adds `aliases.lest` to the project's `.luaurc`, creating the file when
/// absent. Purely for tooling reach: the alias is what lets Luau-LSP and selene
/// follow `require('@lest')` to the real sources and infer types from them.
///
/// An existing `.luaurc` is the user's file, so this is conservative. It is
/// rewritten only when it parses as plain JSON and carries no comments —
/// serde_json cannot round-trip `//` and `/* */`, which `.luaurc` allows, and
/// silently deleting someone's comments is worse than printing two lines they
/// can paste. Same for anything already bound to `lest`.
///
/// Returns whether `@lest` now resolves to the framework this run wrote. Every
/// other outcome is `false`, including the case where some *other* `lest` alias
/// already exists: `@lest` resolves there, but not to `.lest/core`, so nothing
/// generated afterwards may assume it.
fn ensure_luaurc_alias(cwd: &Path) -> Result<bool, ToolError> {
    let path = cwd.join(".luaurc");
    let manual = || {
        println!(
            "Add this to {} yourself to require('@lest'):\n  \"aliases\": {{ \"lest\": \"{}\" }}",
            path.display(),
            embed::CORE_DIR
        );
    };

    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let fresh = format!(
                "{{\n  \"aliases\": {{\n    \"lest\": \"{}\"\n  }}\n}}\n",
                embed::CORE_DIR
            );
            std::fs::write(&path, fresh)
                .map_err(|e| ToolError(format!("cannot write {}: {e}", path.display())))?;
            println!("Wrote .luaurc with the `lest` alias.");
            return Ok(true);
        }
        Err(err) => return Err(ToolError(format!("cannot read {}: {err}", path.display()))),
    };

    let sanitized = crate::resolve::sanitize_luaurc(&existing);
    if sanitized != existing {
        println!("Left .luaurc alone — it contains comments, which lest will not rewrite.");
        manual();
        return Ok(false);
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&sanitized) else {
        println!("Left .luaurc alone — it is not valid JSON.");
        manual();
        return Ok(false);
    };
    let Some(object) = value.as_object_mut() else {
        println!("Left .luaurc alone — it is not a JSON object.");
        manual();
        return Ok(false);
    };

    let aliases = object
        .entry("aliases")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let Some(aliases) = aliases.as_object_mut() else {
        println!("Left .luaurc alone — its `aliases` field is not an object.");
        manual();
        return Ok(false);
    };
    // Alias names are case-insensitive per the require-by-string RFC, so a
    // differently-cased `lest` already binds `@lest` and must not be shadowed.
    if let Some((name, bound)) = aliases
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("lest"))
    {
        println!("Left .luaurc alone — `{name}` already binds to {bound}.");
        // `@lest` resolves, but to their framework, not the one just written —
        // so anything generated from here on uses the relative path instead.
        return Ok(false);
    }
    aliases.insert(
        "lest".to_string(),
        serde_json::Value::String(embed::CORE_DIR.to_string()),
    );

    let mut rendered = serde_json::to_string_pretty(&value)
        .map_err(|e| ToolError(format!("cannot render .luaurc: {e}")))?;
    rendered.push('\n');
    std::fs::write(&path, rendered)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", path.display())))?;
    println!("Added the `lest` alias to .luaurc.");
    Ok(true)
}

fn write_example(cwd: &Path, answers: &Answers, alias_active: bool) -> Result<(), ToolError> {
    let dir = cwd.join(static_prefix(&answers.include));
    std::fs::create_dir_all(&dir)
        .map_err(|e| ToolError(format!("cannot create {}: {e}", dir.display())))?;
    let path = dir.join("example.spec.luau");
    if path.exists() {
        println!(
            "Skipped the example spec — {} already exists.",
            path.display()
        );
        return Ok(());
    }

    // With the alias, every spec requires the framework the same way wherever
    // it sits. Without it, the require is a relative path into `.lest/core`,
    // which depends on how deep the spec is. Keyed on the alias being *in
    // effect*, not on the answer: `ensure_luaurc_alias` bails out whenever the
    // file is not lest's to rewrite, and the relative path always works.
    let require = if alias_active {
        "@lest".to_string()
    } else {
        let core_dir = crate::resolve::normalize(&embed::core_dir(cwd));
        relative_path(&crate::resolve::normalize(&dir), &core_dir)
            .unwrap_or_else(|| "path/to/lest".to_string())
    };
    let spec = format!(
        "--!strict\nlocal Lest = require('{require}')\nlocal describe, it, expect = Lest.describe, Lest.it, Lest.expect\n\n\
         describe('example', function ()\n\tit('adds', function ()\n\t\texpect(1 + 1).toBe(2)\n\tend)\nend)\n\nreturn nil\n"
    );
    std::fs::write(&path, spec)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", path.display())))?;
    println!("Wrote the example spec to {}.", path.display());
    Ok(())
}

/// The literal directory part of a glob, up to its first wildcard:
/// `src/**/*.spec.luau` → `src`, `**/*.spec.luau` → ``.
fn static_prefix(glob: &str) -> PathBuf {
    let wildcard = glob.find(['*', '?', '[']).unwrap_or(glob.len());
    let prefix = &glob[..wildcard];
    let dir = match prefix.rfind('/') {
        Some(slash) => &prefix[..slash],
        None => "",
    };
    PathBuf::from(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_prefix_stops_at_wildcards() {
        assert_eq!(static_prefix("src/**/*.spec.luau"), PathBuf::from("src"));
        assert_eq!(static_prefix("**/*.spec.luau"), PathBuf::from(""));
        assert_eq!(
            static_prefix("tests/unit/*.spec.luau"),
            PathBuf::from("tests/unit")
        );
    }

    #[test]
    fn config_includes_chosen_suites() {
        let answers = Answers {
            backend: BackendKind::Native,
            suite_name: "unit".into(),
            include: "src/**/*.spec.luau".into(),
            scripts_backend: Some(BackendKind::Lute),
            cloud_suite: true,
            write_example: false,
            gitignore: true,
            luaurc_alias: true,
        };
        let text = render_config(&answers);
        assert!(text.contains("backend = \"native\""));
        assert!(text.contains("[suites.unit]"));
        assert!(text.contains("[suites.scripts]"));
        assert!(text.contains("backend = \"lute\""));
        assert!(text.contains("[suites.engine]"));
        assert!(text.contains("default = false"));
        // The generated config must parse with lest's own loader.
        let parsed: toml::Value = toml::from_str(&text).expect("generated config parses");
        // The framework is embedded now, so the config must not pin a path to
        // it. Checked on the parsed value, not the text: the settings comment
        // mentions the key it is telling you that you do not need.
        assert!(parsed.get("settings").and_then(|s| s.get("core")).is_none());
    }

    fn luaurc(temp: &tempfile::TempDir) -> String {
        std::fs::read_to_string(temp.path().join(".luaurc")).unwrap()
    }

    #[test]
    fn luaurc_alias_is_created_when_absent() {
        let temp = tempfile::tempdir().unwrap();
        assert!(ensure_luaurc_alias(temp.path()).unwrap());
        assert!(luaurc(&temp).contains("\"lest\""));
        assert!(luaurc(&temp).contains(embed::CORE_DIR));
    }

    #[test]
    fn luaurc_alias_merges_into_existing_aliases() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(".luaurc"),
            "{\n  \"languageMode\": \"strict\",\n  \"aliases\": { \"pkg\": \"Packages\" }\n}\n",
        )
        .unwrap();

        assert!(ensure_luaurc_alias(temp.path()).unwrap());
        let text = luaurc(&temp);
        // The user's own settings and alias survive alongside the new one.
        assert!(text.contains("\"languageMode\""));
        assert!(text.contains("\"pkg\""));
        assert!(text.contains("\"lest\""));
        // preserve_order keeps the user's keys where they wrote them.
        assert!(text.find("languageMode").unwrap() < text.find("aliases").unwrap());
    }

    #[test]
    fn luaurc_with_comments_is_left_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let original = "{\n  // our aliases\n  \"aliases\": { \"pkg\": \"Packages\" }\n}\n";
        std::fs::write(temp.path().join(".luaurc"), original).unwrap();

        // False, so the example spec does not go out requiring an alias that
        // was never written.
        assert!(!ensure_luaurc_alias(temp.path()).unwrap());
        assert_eq!(luaurc(&temp), original);
    }

    /// The alias's *outcome* is what the example spec keys on. A `.luaurc` lest
    /// refuses to touch means the generated spec must use a relative require —
    /// `require('@lest')` there fails on the first `lest` run.
    #[test]
    fn example_require_follows_the_alias_outcome_not_the_answer() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".luaurc"), "{ not json").unwrap();
        let answers = Answers {
            backend: BackendKind::Native,
            suite_name: "unit".into(),
            include: "src/**/*.spec.luau".into(),
            scripts_backend: None,
            cloud_suite: false,
            write_example: true,
            gitignore: false,
            // Asked for and not granted: the file is not lest's to rewrite.
            luaurc_alias: true,
        };
        let alias_active = ensure_luaurc_alias(temp.path()).unwrap();
        assert!(!alias_active);

        write_example(temp.path(), &answers, alias_active).unwrap();
        let spec = std::fs::read_to_string(temp.path().join("src/example.spec.luau")).unwrap();
        assert!(!spec.contains("'@lest'"), "{spec}");
        assert!(spec.contains(".lest"), "{spec}");
    }

    #[test]
    fn suite_names_must_be_toml_bare_keys() {
        for bad in ["", "my suite", "unit.core", "quo\"te"] {
            assert!(
                matches!(validate_suite_name(bad).unwrap(), Validation::Invalid(_)),
                "{bad:?} should be rejected"
            );
        }
        for good in ["unit", "unit-2", "my_suite"] {
            assert_eq!(validate_suite_name(good).unwrap(), Validation::Valid);
        }
        assert!(matches!(
            validate_include("  ").unwrap(),
            Validation::Invalid(_)
        ));
        assert_eq!(
            validate_include("**/*.spec.luau").unwrap(),
            Validation::Valid
        );
    }

    /// globset treats `\` as an escape, so a Windows-separated glob is
    /// accepted and then matches nothing forever — caught at the prompt.
    #[test]
    fn backslash_globs_are_rejected() {
        assert!(matches!(
            validate_include("src\\**\\*.spec.luau").unwrap(),
            Validation::Invalid(_)
        ));
        assert_eq!(
            validate_include("src/**/*.spec.luau").unwrap(),
            Validation::Valid
        );
    }

    /// No spec may match two suites in a generated config: the main glob is
    /// rejected when it can reach a chosen extra suite's directory, and the
    /// pre-fill already avoids them.
    #[test]
    fn generated_suites_cannot_overlap() {
        // The zero-config glob reaches both extra-suite directories.
        assert!(matches!(
            validate_no_suite_overlap("**/*.spec.luau", true, true).unwrap(),
            Validation::Invalid(_)
        ));
        assert!(matches!(
            validate_no_suite_overlap("tests/**/*.spec.luau", true, false).unwrap(),
            Validation::Invalid(_)
        ));
        // A nested-only glob still overlaps and is still caught.
        assert!(matches!(
            validate_no_suite_overlap("tests/scripts/a/*.spec.luau", true, false).unwrap(),
            Validation::Invalid(_)
        ));
        // Only *chosen* extra suites are guarded.
        assert_eq!(
            validate_no_suite_overlap("**/*.spec.luau", false, false).unwrap(),
            Validation::Valid
        );
        // A scoped glob passes with every extra chosen.
        assert_eq!(
            validate_no_suite_overlap("src/**/*.spec.luau", true, true).unwrap(),
            Validation::Valid
        );
        // The pre-fill follows the same rule: choosing an extra suite scopes
        // the suggested main glob away from `tests/`.
        assert_eq!(default_include(false, true), "src/**/*.spec.luau");
        assert_eq!(default_include(false, false), "**/*.spec.luau");
        assert_eq!(default_include(true, false), "src/**/*.spec.luau");
    }

    /// Even if a glob reached the renderer unvalidated, the file it writes has
    /// to parse — otherwise `lest init` produces a config that breaks the very
    /// next command.
    #[test]
    fn generated_config_parses_whatever_the_glob_contains() {
        let answers = Answers {
            backend: BackendKind::Native,
            suite_name: "unit".into(),
            include: "src\\**\\a\"b.spec.luau".into(),
            scripts_backend: None,
            cloud_suite: false,
            write_example: false,
            gitignore: false,
            luaurc_alias: false,
        };
        let text = render_config(&answers);
        let parsed: toml::Value = toml::from_str(&text).expect("generated config parses");
        assert_eq!(
            parsed["suites"]["unit"]["include"][0].as_str(),
            Some("src\\**\\a\"b.spec.luau")
        );
    }

    #[test]
    fn luaurc_keeps_an_existing_lest_alias() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(".luaurc"),
            "{\"aliases\":{\"Lest\":\"vendor/lest\"}}",
        )
        .unwrap();

        // False: `@lest` resolves, but to their copy, not the one just written.
        assert!(!ensure_luaurc_alias(temp.path()).unwrap());
        // Alias names are case-insensitive, so `Lest` already binds `@lest`.
        assert!(luaurc(&temp).contains("vendor/lest"));
        assert!(!luaurc(&temp).contains(embed::CORE_DIR));
    }
}
