//! `lest studio` — the Studio companion plugin: install, uninstall, status.
//!
//! The plugin ships inside this binary (the same rule as everything else lest
//! owns: `luau/studio/` is embedded at build time) and `install` writes it to
//! the user-level Roblox Plugins folder as `lest.rbxmx`. Alongside it, a stamp
//! file under `~/.lest/` records what was written — version, port, the
//! install-time session secret, and a digest of the plugin file — so repair,
//! uninstall, and the future bridge all know whether the file on disk is ours.
//! The digest check is the same philosophy as `.lest/core`'s stamp: compared,
//! not trusted; anything unrecognized is refused rather than overwritten.
//!
//! The secret is the bridge's mutual-auth token: baked into the plugin source
//! at install time and mirrored into the stamp, it lets the CLI (which reads
//! the stamp) and the plugin (which carries the baked copy) recognize each
//! other on 127.0.0.1 without either side trusting the port number alone. It
//! is a loopback-only guard against confusion, not cryptography — the threat
//! is a stale plugin or another local tool on the port, not an attacker.

pub(crate) mod bridge;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ToolError;

/// The plugin sources, embedded like the runtime harness and cloud collector.
const MAIN_TEMPLATE: &str = include_str!("../luau/studio/main.luau");
const BRIDGE_SOURCE: &str = include_str!("../luau/studio/bridge.luau");

/// The bridge's default loopback port. Chosen away from rojo's 34872 and
/// run-in-roblox's 50312 so the tools coexist; overridable per project with
/// `[studio] port` and per install with `--port`.
pub const DEFAULT_PORT: u16 = 28806;

/// The installed plugin's filename. Stable — the persistent-session design
/// means one install serves every project and every run (the port lives
/// *inside* the file, not in its name).
const PLUGIN_FILE: &str = "lest.rbxmx";

/// How long `status` waits for a live plugin to poll the probe. The plugin's
/// absent-server backoff ceiling is 2 seconds and absent polls shed any
/// longer suspect-episode memory (see luau/studio/bridge.luau), so an idle
/// connected Studio polls within ~2s of the port opening; the margin covers
/// a poll mid-flight when the probe bound, and one suspect-widened gap.
const PROBE_WAIT: std::time::Duration = std::time::Duration::from_secs(4);

/// What `install` recorded, mirrored so later commands can recognize their own
/// work. Lives at `~/.lest/studio.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stamp {
    version: String,
    port: u16,
    secret: String,
    /// FNV digest of the installed `lest.rbxmx` bytes, `{:016x}`-formatted.
    digest: String,
}

/// How an install call left the world, separated from printing so tests can
/// assert on outcomes without capturing stdout.
#[derive(Debug, PartialEq, Eq)]
enum InstallOutcome {
    /// Fresh install — no prior plugin file.
    Installed,
    /// An existing plugin file was replaced: a lest install whose version or
    /// port changed, or a foreign file overwritten under `--force`.
    Updated,
    /// The installed file already matches this binary, port, and secret.
    Current,
}

#[derive(Debug, PartialEq, Eq)]
enum UninstallOutcome {
    Removed,
    NotInstalled,
}

/// The status report, gathered as data so the printing stays one honest
/// rendering of it (and so tests can cover the states without stdout).
#[derive(Debug, PartialEq, Eq)]
enum PluginState {
    /// File present and byte-identical to what the stamp recorded.
    Current,
    /// Stamp present but the file is gone.
    FileMissing,
    /// Stamp present but the file's bytes are not what it recorded.
    FileChanged,
    /// No stamp — lest has never installed here (whatever file may exist).
    NeverInstalled,
}

// ---------------------------------------------------------------------------
// Platform locations
// ---------------------------------------------------------------------------

/// The Studio local Plugins folder. Studio only exists on Windows and macOS;
/// everywhere else the studio family of commands is a clear tool error rather
/// than a guess.
fn plugins_dir() -> Result<PathBuf, ToolError> {
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                ToolError(
                    "cannot locate the Roblox Plugins folder ($LOCALAPPDATA is not set)".into(),
                )
            })?;
        Ok(base.join("Roblox").join("Plugins"))
    } else if cfg!(target_os = "macos") {
        Ok(crate::self_cmd::home_dir()?
            .join("Documents")
            .join("Roblox")
            .join("Plugins"))
    } else {
        Err(ToolError(
            "Roblox Studio does not run on this platform — `lest studio` needs Windows or macOS \
             (engine suites can still run anywhere through the cloud backend)"
                .into(),
        ))
    }
}

/// The stamp file, in the same user-level directory `lest self` manages.
fn stamp_file() -> Result<PathBuf, ToolError> {
    Ok(crate::self_cmd::home_dir()?
        .join(".lest")
        .join("studio.json"))
}

// ---------------------------------------------------------------------------
// Plugin construction
// ---------------------------------------------------------------------------

/// Replaces each marked placeholder line of the entry template — the runtime
/// harness convention: a binding's marker is a trailing `-- __LEST_*__`
/// comment and the whole line carrying it is swapped, so the template on disk
/// stays ordinary, checkable Luau. A marker no line carries is a hard error:
/// a silently dropped marker would ship a plugin polling the default port
/// with an unconfigured secret, which the bridge would then refuse — a
/// confusing failure the loud one here prevents.
fn substitute_plugin(template: &str, bindings: &[(&str, String)]) -> Result<String, ToolError> {
    let mut out = String::new();
    let mut applied = vec![false; bindings.len()];
    let mut lines = template.lines().peekable();

    // Directives must stay in the leading comment block to bind; the
    // generated-file banner goes after them, exactly as the harness does it.
    while let Some(directive) = lines.next_if(|line| line.starts_with("--!")) {
        out.push_str(directive);
        out.push('\n');
    }
    out.push_str(
        "-- Generated by `lest studio install` from luau/studio/main.luau — do not edit; \
         rewritten on every install.\n",
    );

    for line in lines {
        match bindings
            .iter()
            .position(|(marker, _)| line.contains(marker))
        {
            Some(index) => {
                // A marker appearing twice is the same class of template
                // corruption as a missing one — refuse rather than bake two
                // (possibly disagreeing) copies of a constant into the plugin.
                if applied[index] {
                    return Err(ToolError(format!(
                        "the studio plugin template carries the marker comment {} more than once — \
                         each `-- __LEST_*__` line in luau/studio/main.luau must be unique",
                        bindings[index].0
                    )));
                }
                applied[index] = true;
                out.push_str(&bindings[index].1);
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }

    let missing: Vec<&str> = bindings
        .iter()
        .zip(&applied)
        .filter(|(_, &done)| !done)
        .map(|((marker, _), _)| *marker)
        .collect();
    if !missing.is_empty() {
        return Err(ToolError(format!(
            "the studio plugin template is missing the marker comment(s) {} — every `-- __LEST_*__` \
             line in luau/studio/main.luau must survive editing",
            missing.join(", ")
        )));
    }
    Ok(out)
}

/// The entry script with this install's identity baked in.
fn build_entry_source(version: &str, port: u16, secret: &str) -> Result<String, ToolError> {
    substitute_plugin(
        MAIN_TEMPLATE,
        &[
            (
                "__LEST_STUDIO_VERSION__",
                format!("local VERSION = '{version}'"),
            ),
            ("__LEST_STUDIO_PORT__", format!("local PORT = {port}")),
            (
                "__LEST_STUDIO_SECRET__",
                format!("local SECRET = '{secret}'"),
            ),
        ],
    )
}

/// Escapes text for an XML text node. The three that matter in element
/// content; quotes stay literal because sources never land in attributes.
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The `.rbxmx` document: a plugin `Script` (the entry) with the bridge as a
/// child `ModuleScript` — the shape `require(script.bridge)` in the entry
/// expects. Hand-templated rather than pulled in through a serializer crate:
/// the format is stable XML, this is the only place lest writes it, and the
/// header matches what Studio's own exporter produces.
fn build_rbxmx(entry_source: &str, bridge_source: &str) -> String {
    format!(
        r#"<roblox xmlns:xmime="http://www.w3.org/2005/05/xmlmime" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="http://www.roblox.com/roblox.xsd" version="4">
	<Item class="Script" referent="RBX0">
		<Properties>
			<string name="Name">lest</string>
			<ProtectedString name="Source">{}</ProtectedString>
		</Properties>
		<Item class="ModuleScript" referent="RBX1">
			<Properties>
				<string name="Name">bridge</string>
				<ProtectedString name="Source">{}</ProtectedString>
			</Properties>
		</Item>
	</Item>
</roblox>
"#,
        xml_escape(entry_source),
        xml_escape(bridge_source)
    )
}

fn digest_of(bytes: &[u8]) -> String {
    format!("{:016x}", crate::resolve::hash_bytes(bytes))
}

/// A fresh session secret: 32 hex chars from two FNV rounds over process
/// entropy (time, pid, a stack address). Loopback-confusion guard, not
/// cryptography — see the module docs; upgrading this means upgrading the
/// stated threat model first.
fn generate_secret() -> String {
    // The counter guarantees two in-process calls differ even where the OS
    // timer is coarse and the stack lands the probe at the same address.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stack_probe = 0u8;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix = format!(
        "{nanos}-{}-{:p}-{}",
        std::process::id(),
        std::ptr::addr_of!(stack_probe),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let a = crate::resolve::hash_bytes(mix.as_bytes());
    let b = crate::resolve::hash_bytes(format!("{mix}-{a}").as_bytes());
    format!("{a:016x}{b:016x}")
}

// ---------------------------------------------------------------------------
// Core operations (path-parameterized so tests run against temp dirs)
// ---------------------------------------------------------------------------

fn read_stamp(stamp_path: &Path) -> Option<Stamp> {
    let text = std::fs::read_to_string(stamp_path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_stamp(stamp_path: &Path, stamp: &Stamp) -> Result<(), ToolError> {
    if let Some(parent) = stamp_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolError(format!("cannot create {}: {e}", parent.display())))?;
    }
    let text = serde_json::to_string_pretty(stamp)
        .map_err(|e| ToolError(format!("cannot serialize the studio stamp: {e}")))?;
    std::fs::write(stamp_path, text)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", stamp_path.display())))
}

fn install_at(
    plugins: &Path,
    stamp_path: &Path,
    requested_port: Option<u16>,
    force: bool,
) -> Result<InstallOutcome, ToolError> {
    let prior = read_stamp(stamp_path);
    let plugin_path = plugins.join(PLUGIN_FILE);

    // Precedence for the port: an explicit request (flag, then config —
    // merged by the caller) wins; otherwise a reinstall keeps whatever the
    // existing install used, so upgrading lest never silently moves the
    // bridge; the default is last.
    let port = requested_port
        .or(prior.as_ref().map(|s| s.port))
        .unwrap_or(DEFAULT_PORT);
    // Port 0 means "any free port" to an OS but nothing to a plugin that must
    // dial a fixed address — baking it in would install a bridge that can
    // never connect.
    if port == 0 {
        return Err(ToolError(format!(
            "port 0 is not a usable bridge port — pick a real port (the default is {DEFAULT_PORT})"
        )));
    }
    // The secret survives reinstalls for the same reason the port does: a
    // version upgrade should not orphan a running Studio session's auth.
    let secret = prior
        .as_ref()
        .map(|s| s.secret.clone())
        .unwrap_or_else(generate_secret);

    let existing = match std::fs::read(&plugin_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(ToolError(format!(
                "cannot read {}: {e}",
                plugin_path.display()
            )))
        }
    };

    // Refuse to overwrite a file lest cannot recognize as its own — the
    // `.lest/core` rule. No stamp, or bytes that do not match the stamp,
    // means someone (or something) else owns that file.
    if let Some(bytes) = &existing {
        let ours = prior.as_ref().is_some_and(|s| s.digest == digest_of(bytes));
        if !ours && !force {
            return Err(ToolError(format!(
                "{} already exists and was not written by this lest install — move it aside, or \
                 re-run with --force to replace it",
                plugin_path.display()
            )));
        }
    }

    let entry = build_entry_source(env!("CARGO_PKG_VERSION"), port, &secret)?;
    let contents = build_rbxmx(&entry, BRIDGE_SOURCE);

    if existing.as_deref() == Some(contents.as_bytes()) {
        // Identical bytes imply identical version/port/secret — nothing to do.
        // The stamp is still rewritten below in case *it* was the stale half.
        write_stamp(
            stamp_path,
            &Stamp {
                version: env!("CARGO_PKG_VERSION").into(),
                port,
                secret,
                digest: digest_of(contents.as_bytes()),
            },
        )?;
        return Ok(InstallOutcome::Current);
    }

    std::fs::create_dir_all(plugins)
        .map_err(|e| ToolError(format!("cannot create {}: {e}", plugins.display())))?;
    std::fs::write(&plugin_path, &contents)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", plugin_path.display())))?;
    write_stamp(
        stamp_path,
        &Stamp {
            version: env!("CARGO_PKG_VERSION").into(),
            port,
            secret,
            digest: digest_of(contents.as_bytes()),
        },
    )?;

    Ok(if existing.is_some() {
        InstallOutcome::Updated
    } else {
        InstallOutcome::Installed
    })
}

fn uninstall_at(plugins: &Path, stamp_path: &Path) -> Result<UninstallOutcome, ToolError> {
    let plugin_path = plugins.join(PLUGIN_FILE);
    let stamp = read_stamp(stamp_path);

    let file = match std::fs::read(&plugin_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(ToolError(format!(
                "cannot read {}: {e}",
                plugin_path.display()
            )))
        }
    };

    match (file, stamp) {
        (None, stamp) => {
            // Nothing installed; clear a stale stamp so status stays honest.
            if stamp.is_some() {
                let _ = std::fs::remove_file(stamp_path);
            }
            Ok(UninstallOutcome::NotInstalled)
        }
        (Some(bytes), Some(stamp)) if stamp.digest == digest_of(&bytes) => {
            std::fs::remove_file(&plugin_path)
                .map_err(|e| ToolError(format!("cannot remove {}: {e}", plugin_path.display())))?;
            let _ = std::fs::remove_file(stamp_path);
            Ok(UninstallOutcome::Removed)
        }
        (Some(_), _) => Err(ToolError(format!(
            "{} was not written by this lest install (or was modified since) — refusing to \
             delete it; remove it by hand if it should go",
            plugin_path.display()
        ))),
    }
}

fn status_at(plugins: &Path, stamp_path: &Path) -> (Option<Stamp>, PluginState) {
    let stamp = read_stamp(stamp_path);
    let plugin_path = plugins.join(PLUGIN_FILE);
    let state = match &stamp {
        None => PluginState::NeverInstalled,
        Some(stamp) => match std::fs::read(&plugin_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PluginState::FileMissing,
            // Unreadable counts as changed: lest can no longer vouch for the
            // file's contents, which is what "changed" means here.
            Err(_) => PluginState::FileChanged,
            Ok(bytes) if digest_of(&bytes) == stamp.digest => PluginState::Current,
            Ok(_) => PluginState::FileChanged,
        },
    };
    (stamp, state)
}

// ---------------------------------------------------------------------------
// CLI entry points
// ---------------------------------------------------------------------------

/// `lest studio install`. `requested_port` is flag-over-config, merged by the
/// caller; `None` keeps an existing install's port or falls to the default.
pub fn install(requested_port: Option<u16>, force: bool) -> Result<(), ToolError> {
    let plugins = plugins_dir()?;
    let stamp_path = stamp_file()?;
    // Whether lest had ever installed here, read before installing: it is
    // what decides if the permission walkthrough is news. A forced replace of
    // a foreign file is a *first* lest install from Studio's point of view.
    let first_install = read_stamp(&stamp_path).is_none();
    let outcome = install_at(&plugins, &stamp_path, requested_port, force)?;
    let path = plugins.join(PLUGIN_FILE);
    let (_, state) = status_at(&plugins, &stamp_path);
    let port = read_stamp(&stamp_path)
        .map(|s| s.port)
        .unwrap_or(DEFAULT_PORT);
    debug_assert_eq!(state, PluginState::Current);

    match outcome {
        InstallOutcome::Current => {
            println!(
                "The lest Studio plugin is already installed and current at {}.",
                path.display()
            );
        }
        InstallOutcome::Updated | InstallOutcome::Installed => {
            if outcome == InstallOutcome::Installed {
                println!("Installed the lest Studio plugin to {}.", path.display());
            } else {
                println!("Updated the lest Studio plugin at {}.", path.display());
                println!("Restart Roblox Studio (if open) to load the new version.");
            }
            if first_install {
                println!();
                println!("One-time Studio setup, when prompted on first use:");
                println!("  1. Open (or restart) Roblox Studio with your place.");
                println!(
                    "  2. Allow the lest plugin to make HTTP requests (Plugin Management) — it talks"
                );
                println!("     to the lest CLI on 127.0.0.1:{port} and nowhere else.");
                println!(
                    "  3. When the studio backend lands, allow Script Injection too — that is how a"
                );
                println!("     run's test bundle enters the place.");
            }
        }
    }
    Ok(())
}

/// `lest studio uninstall`.
pub fn uninstall() -> Result<(), ToolError> {
    let plugins = plugins_dir()?;
    let stamp_path = stamp_file()?;
    match uninstall_at(&plugins, &stamp_path)? {
        UninstallOutcome::Removed => {
            println!(
                "Removed the lest Studio plugin from {}.",
                plugins.join(PLUGIN_FILE).display()
            );
        }
        UninstallOutcome::NotInstalled => {
            println!("Nothing to remove — the lest Studio plugin is not installed.");
        }
    }
    Ok(())
}

/// `lest studio status`.
pub fn status() -> Result<(), ToolError> {
    let plugins = plugins_dir()?;
    let stamp_path = stamp_file()?;
    let (stamp, state) = status_at(&plugins, &stamp_path);
    let path = plugins.join(PLUGIN_FILE);

    match (&stamp, &state) {
        (None, _) => {
            println!("The lest Studio plugin is not installed — run `lest studio install`.");
        }
        (Some(stamp), PluginState::Current) => {
            println!(
                "Installed at {} (lest {}, port {}).",
                path.display(),
                stamp.version,
                stamp.port
            );
            if stamp.version != env!("CARGO_PKG_VERSION") {
                println!(
                    "This lest is {} — run `lest studio install` to update the plugin.",
                    env!("CARGO_PKG_VERSION")
                );
            }
        }
        (Some(_), PluginState::FileMissing) => {
            println!(
                "The plugin file is missing from {} — run `lest studio install` to restore it.",
                plugins.display()
            );
        }
        (Some(_), PluginState::FileChanged) => {
            println!(
                "{} was modified by something other than lest — run `lest studio install --force` \
                 to restore it.",
                path.display()
            );
        }
        (Some(_), PluginState::NeverInstalled) => unreachable!("stamp implies installed"),
    }
    // With an install on record there is a port and secret to probe, so ask
    // whether a Studio session is actually connected right now. Probing even
    // when the file on disk changed or vanished is deliberate: a running
    // Studio may still have an older, working plugin loaded.
    if let Some(stamp) = &stamp {
        // Probe failures print and still exit 0: status is informational,
        // and "cannot check" is a status.
        match bridge::probe(stamp.port, &stamp.secret, PROBE_WAIT) {
            Ok(bridge::PingOutcome::Session(session)) => {
                if session.ok {
                    let place = match session.place_id {
                        Some(id) => format!(" (place {id})"),
                        None => String::new(),
                    };
                    println!(
                        "Live session: \"{}\"{place}, plugin {}.",
                        session.place_name, session.plugin_version
                    );
                } else {
                    println!(
                        "A Studio session answered but refused the check: {}.",
                        session.error.as_deref().unwrap_or("(no error given)")
                    );
                }
                if let Some(note) = version_note(&session.plugin_version) {
                    println!("{note}");
                }
            }
            Ok(bridge::PingOutcome::RefusedSecret) => {
                println!(
                    "A plugin polled with a mismatched secret: Studio is running an older \
                     install. Restart Studio to load the refreshed plugin (run `lest studio \
                     install` first if you haven't)."
                );
            }
            Ok(bridge::PingOutcome::Silent) => {
                println!("No live Studio session answered on port {}.", stamp.port);
                // Only suggest the permission checklist when the install
                // itself is healthy; in the missing/changed states the
                // install guidance above is the real lead.
                if state == PluginState::Current {
                    println!("(Is Studio open, with the plugin's HTTP permission granted?)");
                }
            }
            Err(e) => {
                println!("Cannot check for a live session: {e}.");
            }
        }
    }
    Ok(())
}

/// The installed bridge credentials, for the studio backend. Not being
/// installed is a tool error with the fix in it — the backend cannot invent
/// a port and secret the plugin does not share.
pub(crate) fn credentials() -> Result<(u16, String), ToolError> {
    let stamp = read_stamp(&stamp_file()?).ok_or_else(|| {
        ToolError(
            "the studio backend needs the companion plugin — run `lest studio install`, open \
             your place in Studio, and re-run"
                .into(),
        )
    })?;
    Ok((stamp.port, stamp.secret))
}

/// The version-handshake note for a live session: a running plugin that does
/// not match this binary gets explicit re-install and restart instructions,
/// never a silent skew. `None` when versions agree.
fn version_note(plugin_version: &str) -> Option<String> {
    if plugin_version == env!("CARGO_PKG_VERSION") {
        return None;
    }
    Some(format!(
        "The session's plugin is {plugin_version}; this lest is {}. Run `lest studio install`, \
         then restart Studio.",
        env!("CARGO_PKG_VERSION")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugins = dir.path().join("Plugins");
        let stamp = dir.path().join("data").join("studio.json");
        (dir, plugins, stamp)
    }

    #[test]
    fn entry_source_substitutes_all_three_markers() {
        let source = build_entry_source("9.9.9", 12345, "cafebabe").expect("substitute");
        assert!(source.contains("local VERSION = '9.9.9'"));
        assert!(source.contains("local PORT = 12345"));
        assert!(source.contains("local SECRET = 'cafebabe'"));
        assert!(!source.contains("__LEST_STUDIO_"));
        // The banner sits below the strict directive so the directive binds.
        assert!(source.starts_with("--!strict\n-- Generated by `lest studio install`"));
    }

    #[test]
    fn a_dropped_marker_is_a_hard_error() {
        let err = substitute_plugin(
            "--!strict\nlocal x = 1\n",
            &[("__LEST_STUDIO_PORT__", "local PORT = 1".into())],
        )
        .expect_err("must fail");
        assert!(err.to_string().contains("missing the marker"));
    }

    #[test]
    fn rbxmx_escapes_sources_and_names_both_scripts() {
        let doc = build_rbxmx("if a < b and c > d then print('&') end", "-- bridge");
        assert!(doc.contains("if a &lt; b and c &gt; d then print('&amp;') end"));
        assert!(doc.contains(r#"<string name="Name">lest</string>"#));
        assert!(doc.contains(r#"<string name="Name">bridge</string>"#));
        assert!(doc.contains(r#"class="ModuleScript""#));
    }

    #[test]
    fn fresh_install_writes_plugin_and_stamp() {
        let (_dir, plugins, stamp) = temp_setup();
        let outcome = install_at(&plugins, &stamp, None, false).expect("install");
        assert_eq!(outcome, InstallOutcome::Installed);
        let bytes = std::fs::read(plugins.join(PLUGIN_FILE)).expect("plugin written");
        let recorded = read_stamp(&stamp).expect("stamp written");
        assert_eq!(recorded.digest, digest_of(&bytes));
        assert_eq!(recorded.port, DEFAULT_PORT);
        assert_eq!(recorded.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(recorded.secret.len(), 32);
    }

    #[test]
    fn reinstall_is_current_and_preserves_the_secret() {
        let (_dir, plugins, stamp) = temp_setup();
        install_at(&plugins, &stamp, None, false).expect("first");
        let first = read_stamp(&stamp).expect("stamp");
        let outcome = install_at(&plugins, &stamp, None, false).expect("second");
        assert_eq!(outcome, InstallOutcome::Current);
        let second = read_stamp(&stamp).expect("stamp");
        assert_eq!(first.secret, second.secret);
    }

    #[test]
    fn changing_the_port_updates_and_keeps_the_secret() {
        let (_dir, plugins, stamp) = temp_setup();
        install_at(&plugins, &stamp, None, false).expect("first");
        let first = read_stamp(&stamp).expect("stamp");
        let outcome = install_at(&plugins, &stamp, Some(40000), false).expect("second");
        assert_eq!(outcome, InstallOutcome::Updated);
        let second = read_stamp(&stamp).expect("stamp");
        assert_eq!(second.port, 40000);
        assert_eq!(first.secret, second.secret);
        let text = std::fs::read_to_string(plugins.join(PLUGIN_FILE)).expect("read");
        assert!(text.contains("local PORT = 40000"));
    }

    #[test]
    fn reinstall_without_a_port_keeps_the_existing_port() {
        let (_dir, plugins, stamp) = temp_setup();
        install_at(&plugins, &stamp, Some(40000), false).expect("first");
        install_at(&plugins, &stamp, None, false).expect("second");
        assert_eq!(read_stamp(&stamp).expect("stamp").port, 40000);
    }

    #[test]
    fn a_foreign_file_is_refused_without_force() {
        let (_dir, plugins, stamp) = temp_setup();
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join(PLUGIN_FILE), "not ours").unwrap();
        let err = install_at(&plugins, &stamp, None, false).expect_err("refuse");
        assert!(err
            .to_string()
            .contains("was not written by this lest install"));
        let outcome = install_at(&plugins, &stamp, None, true).expect("force");
        assert_eq!(outcome, InstallOutcome::Updated);
    }

    #[test]
    fn uninstall_removes_only_what_lest_wrote() {
        let (_dir, plugins, stamp) = temp_setup();
        install_at(&plugins, &stamp, None, false).expect("install");
        assert_eq!(
            uninstall_at(&plugins, &stamp).expect("uninstall"),
            UninstallOutcome::Removed
        );
        assert!(!plugins.join(PLUGIN_FILE).exists());
        assert!(!stamp.exists());
        assert_eq!(
            uninstall_at(&plugins, &stamp).expect("again"),
            UninstallOutcome::NotInstalled
        );
    }

    #[test]
    fn uninstall_refuses_a_modified_plugin_file() {
        let (_dir, plugins, stamp) = temp_setup();
        install_at(&plugins, &stamp, None, false).expect("install");
        std::fs::write(plugins.join(PLUGIN_FILE), "tampered").unwrap();
        let err = uninstall_at(&plugins, &stamp).expect_err("refuse");
        assert!(err.to_string().contains("refusing to"));
    }

    #[test]
    fn status_reports_the_four_states() {
        let (_dir, plugins, stamp) = temp_setup();
        assert_eq!(status_at(&plugins, &stamp).1, PluginState::NeverInstalled);
        install_at(&plugins, &stamp, None, false).expect("install");
        assert_eq!(status_at(&plugins, &stamp).1, PluginState::Current);
        std::fs::write(plugins.join(PLUGIN_FILE), "tampered").unwrap();
        assert_eq!(status_at(&plugins, &stamp).1, PluginState::FileChanged);
        std::fs::remove_file(plugins.join(PLUGIN_FILE)).unwrap();
        assert_eq!(status_at(&plugins, &stamp).1, PluginState::FileMissing);
    }

    #[test]
    fn a_missing_plugin_file_is_repaired_keeping_port_and_secret() {
        let (_dir, plugins, stamp) = temp_setup();
        install_at(&plugins, &stamp, Some(40000), false).expect("first");
        let first = read_stamp(&stamp).expect("stamp");
        std::fs::remove_file(plugins.join(PLUGIN_FILE)).unwrap();
        let outcome = install_at(&plugins, &stamp, None, false).expect("repair");
        assert_eq!(outcome, InstallOutcome::Installed);
        let second = read_stamp(&stamp).expect("stamp");
        assert_eq!(second.port, 40000);
        assert_eq!(first.secret, second.secret);
        assert!(plugins.join(PLUGIN_FILE).exists());
    }

    #[test]
    fn a_corrupt_stamp_with_an_existing_file_is_the_foreign_path() {
        let (_dir, plugins, stamp) = temp_setup();
        install_at(&plugins, &stamp, None, false).expect("install");
        std::fs::write(&stamp, "not json").unwrap();
        // The file is real but the stamp cannot vouch for it: refuse without
        // --force, exactly like any other unrecognized file.
        let err = install_at(&plugins, &stamp, None, false).expect_err("refuse");
        assert!(err
            .to_string()
            .contains("was not written by this lest install"));
        assert_eq!(
            install_at(&plugins, &stamp, None, true).expect("force"),
            InstallOutcome::Updated
        );
    }

    #[test]
    fn a_forced_replace_of_a_foreign_file_starts_fresh() {
        let (_dir, plugins, stamp) = temp_setup();
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join(PLUGIN_FILE), "not ours").unwrap();
        install_at(&plugins, &stamp, None, true).expect("force");
        let recorded = read_stamp(&stamp).expect("stamp");
        // No prior stamp to inherit from: default port, brand-new secret.
        assert_eq!(recorded.port, DEFAULT_PORT);
        assert_eq!(recorded.secret.len(), 32);
    }

    #[test]
    fn port_zero_is_rejected() {
        let (_dir, plugins, stamp) = temp_setup();
        let err = install_at(&plugins, &stamp, Some(0), false).expect_err("reject");
        assert!(err.to_string().contains("port 0"));
        assert!(!plugins.join(PLUGIN_FILE).exists());
    }

    #[test]
    fn a_duplicated_marker_is_a_hard_error() {
        let template = "--!strict\nlocal a = 1 -- __X__\nlocal b = 2 -- __X__\n";
        let err =
            substitute_plugin(template, &[("__X__", "local a = 9".into())]).expect_err("must fail");
        assert!(err.to_string().contains("more than once"));
    }

    #[test]
    fn version_note_fires_only_on_skew() {
        assert_eq!(version_note(env!("CARGO_PKG_VERSION")), None);
        let note = version_note("0.0.1").expect("a skewed version earns a note");
        assert!(note.contains("0.0.1"));
        assert!(note.contains(env!("CARGO_PKG_VERSION")));
        assert!(note.contains("lest studio install"));
        assert!(note.contains("restart Studio"));
    }

    #[test]
    fn secrets_are_32_hex_chars_and_vary() {
        let a = generate_secret();
        let b = generate_secret();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Two secrets generated in-process must differ (address/time mix).
        assert_ne!(a, b);
    }
}
