//! `lest self` — manage the lest installation.
//!
//! `install` copies the running binary into a managed directory (`~/.lest/bin`)
//! and adds that directory to the user's PATH; `uninstall` reverses both. The
//! managed location is stable so a future `self update` can swap the binary in
//! place. PATH edits are always user-scoped — never system-wide, never needing
//! admin: on Windows through the `HKCU\Environment` value (edited via
//! PowerShell so the change is broadcast to new shells); on Unix through a
//! marked `export` line in the usual shell rc files.

use std::path::{Path, PathBuf};

use crate::error::ToolError;

/// Managed install directory: `~/.lest/bin`.
fn install_dir() -> Result<PathBuf, ToolError> {
    Ok(home_dir()?.join(".lest").join("bin"))
}

/// The managed binary path: `~/.lest/bin/lest[.exe]`.
fn managed_binary() -> Result<PathBuf, ToolError> {
    Ok(install_dir()?.join(format!("lest{}", std::env::consts::EXE_SUFFIX)))
}

/// `pub(crate)`: `lest studio` keeps its stamp under the same `~/.lest`
/// directory this module manages, resolved by the same rule.
pub(crate) fn home_dir() -> Result<PathBuf, ToolError> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            ToolError(format!(
                "cannot locate your home directory (${var} is not set)"
            ))
        })
}

/// `lest self install`: copy the binary into `~/.lest/bin` and add it to PATH.
pub fn install() -> Result<(), ToolError> {
    let current = std::env::current_exe()
        .map_err(|e| ToolError(format!("cannot locate the running lest executable: {e}")))?;
    let dir = install_dir()?;
    let dest = managed_binary()?;

    std::fs::create_dir_all(&dir)
        .map_err(|e| ToolError(format!("cannot create {}: {e}", dir.display())))?;

    if same_file(&current, &dest) {
        // Phrased so the sentence does not *start* with the brand: `lest` is
        // deliberately lowercase everywhere, and status lines are capitalized
        // sentences — the two rules meet by rewording, not by capitalizing.
        println!("Already installed at {}.", dest.display());
    } else {
        std::fs::copy(&current, &dest).map_err(|e| {
            ToolError(format!(
                "cannot copy {} to {}: {e}",
                current.display(),
                dest.display()
            ))
        })?;
        #[cfg(unix)]
        make_executable(&dest)?;
        println!("Installed lest to {}.", dest.display());
    }

    if add_to_path(&dir)? {
        println!("Added {} to PATH.", dir.display());
        println!("Restart your shell (or open a new terminal) for `lest` to be found.");
    } else {
        println!("{} is already on your PATH.", dir.display());
    }
    Ok(())
}

/// `lest self uninstall`: remove `~/.lest/bin` from PATH and delete it.
pub fn uninstall() -> Result<(), ToolError> {
    let dir = install_dir()?;
    let removed_from_path = remove_from_path(&dir)?;
    let existed = dir.exists();

    if !removed_from_path && !existed {
        // Reworded so the capitalized sentence does not start with the
        // deliberately lowercase brand name.
        println!("Nothing to remove — lest is not installed.");
        return Ok(());
    }

    if removed_from_path {
        println!("Removed {} from PATH.", dir.display());
    }
    if existed {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => println!("Deleted {}.", dir.display()),
            // On Windows a running binary can't delete itself; PATH is already
            // updated, so leave the folder for the user to remove afterwards.
            Err(e) => println!(
                "Left {} in place ({e}) — delete it manually once lest isn't running from there.",
                dir.display()
            ),
        }
    }
    if removed_from_path {
        println!("Restart your shell for the PATH change to take effect.");
    }
    Ok(())
}

/// True when both paths resolve to the same on-disk file (so re-installing from
/// the managed copy skips a needless self-copy).
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| {
            ToolError(format!(
                "cannot read permissions of {}: {e}",
                path.display()
            ))
        })?
        .permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)
        .map_err(|e| ToolError(format!("cannot mark {} executable: {e}", path.display())))
}

// ---------------------------------------------------------------------------
// Windows: edit the user's `HKCU\Environment` PATH via PowerShell. The value
// is read *raw* (`%VAR%` references unexpanded) and written back with its
// registry kind preserved: `[Environment]::GetEnvironmentVariable` returns the
// expanded value, and `SetEnvironmentVariable` stores REG_SZ — the round trip
// would permanently bake every `%VAR%` entry into its expansion of the moment.
// Raw registry writes do not broadcast WM_SETTINGCHANGE, so the write follows
// up with a `SetEnvironmentVariable` no-op purely for its broadcast, keeping
// freshly opened shells aware of the change.
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn add_to_path(dir: &Path) -> Result<bool, ToolError> {
    let (current, kind) = windows_user_path()?;
    match with_dir_added(&current, dir) {
        Some(new) => {
            windows_set_user_path(&new, kind)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(windows)]
fn remove_from_path(dir: &Path) -> Result<bool, ToolError> {
    let (current, kind) = windows_user_path()?;
    match with_dir_removed(&current, dir) {
        Some(new) => {
            windows_set_user_path(&new, kind)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// New PATH string with `dir` appended, or `None` when it's already present.
#[cfg(windows)]
fn with_dir_added(current: &str, dir: &Path) -> Option<String> {
    let mut entries = split_path(current);
    if entries.iter().any(|e| path_eq(e, dir)) {
        return None;
    }
    entries.push(dir.to_string_lossy().into_owned());
    Some(entries.join(";"))
}

/// New PATH string with every occurrence of `dir` removed, or `None` when it
/// wasn't present.
#[cfg(windows)]
fn with_dir_removed(current: &str, dir: &Path) -> Option<String> {
    let entries = split_path(current);
    let kept: Vec<String> = entries
        .iter()
        .filter(|e| !path_eq(e, dir))
        .cloned()
        .collect();
    if kept.len() == entries.len() {
        return None;
    }
    Some(kept.join(";"))
}

#[cfg(windows)]
fn split_path(value: &str) -> Vec<String> {
    value
        .split(';')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Windows paths are case-insensitive; also tolerate a trailing separator.
#[cfg(windows)]
fn path_eq(entry: &str, dir: &Path) -> bool {
    let norm = |s: &str| s.trim_end_matches(['\\', '/']).to_ascii_lowercase();
    norm(entry) == norm(&dir.to_string_lossy())
}

/// The registry kind of the user PATH value, carried from the read to the
/// write so the rewrite preserves it. Flattening `REG_EXPAND_SZ` to `REG_SZ`
/// would stop every `%VAR%` entry on the PATH from expanding.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    /// `REG_SZ` — a literal string.
    Literal,
    /// `REG_EXPAND_SZ` — `%VAR%` references expand at read time. Also the
    /// kind chosen when no PATH value exists yet: it is what Windows itself
    /// creates, and literal paths are unaffected by it.
    Expand,
}

/// Reads the user PATH. Whatever comes back is written straight back with the
/// managed directory added or removed, so this is the destructive half of the
/// operation and the decoding has to be exact — in two ways.
///
/// First, the value is read raw from the registry with
/// `DoNotExpandEnvironmentNames`, alongside its kind:
/// `[Environment]::GetEnvironmentVariable` returns the *expanded* value, and
/// writing that back would silently replace an entry like `%JAVA_HOME%\bin`
/// with whatever it expanded to today, forever.
///
/// Second, Windows PowerShell 5.1 encodes *redirected* stdout in the console's
/// OEM codepage, not UTF-8, so a PATH holding `C:\Users\Müller\...` decodes to
/// U+FFFD under `from_utf8_lossy` — and writing that back would permanently
/// break every unrelated tool on that path. `[Console]::OutputEncoding` is set
/// to BOM-less UTF-8 to prevent it, and the result is refused outright if a
/// replacement character survives anyway: a `lest self install` that stops and
/// explains costs a minute, a corrupted PATH costs an afternoon.
#[cfg(windows)]
fn windows_user_path() -> Result<(String, PathKind), ToolError> {
    // The kind name goes on the first output line, the raw value on the rest.
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); \
             $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment'); \
             if ($k -ne $null -and @($k.GetValueNames()) -contains 'Path') { \
                 Write-Output $k.GetValueKind('Path').ToString(); \
                 Write-Output $k.GetValue('Path', '', \
                     [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) \
             } else { Write-Output 'Absent' }",
        ])
        .output()
        .map_err(|e| ToolError(format!("cannot read the user PATH via PowerShell: {e}")))?;
    if !out.status.success() {
        return Err(ToolError(format!(
            "cannot read the user PATH — PowerShell reported: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let decoded = String::from_utf8_lossy(&out.stdout);
    // A BOM would become the first character of the kind line.
    let text = decoded.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    let kind = match lines.next().map(str::trim).unwrap_or_default() {
        "String" => PathKind::Literal,
        "ExpandString" => PathKind::Expand,
        "Absent" => return Ok((String::new(), PathKind::Expand)),
        // A PATH stored as REG_BINARY or REG_MULTI_SZ is not something lest
        // can round-trip; refusing beats rewriting it into a kind it was not.
        other => {
            return Err(ToolError(format!(
                "cannot update the user PATH — its registry value has the unexpected type \
                 \"{other}\"; add the install directory to your PATH by hand (Settings → System → \
                 About → Advanced system settings → Environment Variables)"
            )))
        }
    };
    // The value is everything after the kind line (a PATH cannot meaningfully
    // contain newlines, but joining preserves one if it somehow does).
    let value = lines.collect::<Vec<_>>().join("\n");
    let value = value.trim_end_matches(['\r', '\n']);
    if value.contains('\u{fffd}') {
        return Err(ToolError(
            "cannot read your PATH as text — PowerShell returned bytes lest cannot decode, and \
             writing them back would corrupt it; add the install directory to your PATH by hand \
             (Settings → System → About → Advanced system settings → Environment Variables)"
                .to_string(),
        ));
    }
    Ok((value.to_string(), kind))
}

#[cfg(windows)]
fn windows_set_user_path(value: &str, kind: PathKind) -> Result<(), ToolError> {
    // Pass the value through an env var so no PATH content is interpolated into
    // the command string (paths can contain quotes, spaces, `$`, etc.). The
    // kind is matched against fixed strings on the PowerShell side, so nothing
    // dynamic reaches the command text there either. `SetValue` writes the raw
    // registry value but does not broadcast WM_SETTINGCHANGE the way
    // `SetEnvironmentVariable` does — the trailing delete of a throwaway
    // variable exists solely for that broadcast (deleting an absent variable
    // still broadcasts, and changes nothing else).
    let kind_name = match kind {
        PathKind::Literal => "String",
        PathKind::Expand => "ExpandString",
    };
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$kind = if ($env:LEST_PATH_KIND -eq 'ExpandString') \
                 { [Microsoft.Win32.RegistryValueKind]::ExpandString } \
                 else { [Microsoft.Win32.RegistryValueKind]::String }; \
             $k = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment'); \
             $k.SetValue('Path', $env:LEST_NEW_PATH, $kind); \
             $k.Close(); \
             [Environment]::SetEnvironmentVariable('LEST_PATH_BROADCAST', $null, 'User')",
        ])
        .env("LEST_NEW_PATH", value)
        .env("LEST_PATH_KIND", kind_name)
        .output()
        .map_err(|e| ToolError(format!("cannot update the user PATH via PowerShell: {e}")))?;
    if !out.status.success() {
        return Err(ToolError(format!(
            "cannot update the user PATH — PowerShell reported: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unix: manage a single marked `export` line in the common shell rc files.
// ---------------------------------------------------------------------------

#[cfg(unix)]
const MARKER: &str = "# added by lest (self install)";

#[cfg(unix)]
fn rc_targets() -> Vec<PathBuf> {
    let Ok(home) = home_dir() else {
        return Vec::new();
    };
    [".profile", ".bashrc", ".zshrc"]
        .iter()
        .map(|f| home.join(f))
        .collect()
}

/// `dir` is deliberately unused: the rc line is written with a literal `$HOME`
/// rather than the expanded path [`install_dir`] just produced. The two always
/// name the same directory — `install_dir` *is* `$HOME/.lest/bin` — but the
/// unexpanded form survives a home directory that later moves or is mounted
/// somewhere else, and an rc file is read for years after it is written.
#[cfg(unix)]
fn add_to_path(_dir: &Path) -> Result<bool, ToolError> {
    let line = format!("export PATH=\"$HOME/.lest/bin:$PATH\" {MARKER}\n");
    let mut changed = false;
    let mut touched_existing = false;
    for rc in rc_targets() {
        if !rc.exists() {
            continue;
        }
        touched_existing = true;
        let content = read(&rc)?;
        if content.contains(MARKER) {
            continue;
        }
        let mut new = content;
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(&line);
        write(&rc, &new)?;
        changed = true;
    }
    // No shell rc files at all — establish PATH through a fresh ~/.profile.
    if !touched_existing {
        let profile = home_dir()?.join(".profile");
        write(&profile, &line)?;
        changed = true;
    }
    Ok(changed)
}

/// `dir` is unused for the same reason `add_to_path` ignores it: the line is
/// found by its marker comment, not by the path it exports.
#[cfg(unix)]
fn remove_from_path(_dir: &Path) -> Result<bool, ToolError> {
    let mut changed = false;
    for rc in rc_targets() {
        if !rc.exists() {
            continue;
        }
        let content = read(&rc)?;
        if !content.contains(MARKER) {
            continue;
        }
        write(&rc, &without_marked_lines(&content))?;
        changed = true;
    }
    Ok(changed)
}

#[cfg(unix)]
fn without_marked_lines(content: &str) -> String {
    content
        .lines()
        .filter(|l| !l.contains(MARKER))
        .map(|l| format!("{l}\n"))
        .collect()
}

#[cfg(unix)]
fn read(path: &Path) -> Result<String, ToolError> {
    std::fs::read_to_string(path)
        .map_err(|e| ToolError(format!("cannot read {}: {e}", path.display())))
}

#[cfg(unix)]
fn write(path: &Path, content: &str) -> Result<(), ToolError> {
    std::fs::write(path, content)
        .map_err(|e| ToolError(format!("cannot write {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::*;
    #[cfg(windows)]
    use std::path::PathBuf;

    #[cfg(windows)]
    #[test]
    fn adds_dir_only_when_absent() {
        let dir = PathBuf::from("C:\\Users\\x\\.lest\\bin");
        assert_eq!(
            with_dir_added("C:\\a;C:\\b", &dir).as_deref(),
            Some("C:\\a;C:\\b;C:\\Users\\x\\.lest\\bin")
        );
        // Already present (case-insensitive, trailing slash) → no change.
        assert_eq!(
            with_dir_added("C:\\A;C:\\USERS\\X\\.LEST\\BIN\\", &dir),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn removes_every_occurrence() {
        let dir = PathBuf::from("C:\\Users\\x\\.lest\\bin");
        assert_eq!(
            with_dir_removed("C:\\a;C:\\Users\\x\\.lest\\bin;C:\\b", &dir).as_deref(),
            Some("C:\\a;C:\\b")
        );
        assert_eq!(with_dir_removed("C:\\a;C:\\b", &dir), None);
    }

    #[cfg(unix)]
    #[test]
    fn strips_only_marked_lines() {
        let content = "export A=1\nexport PATH=\"$HOME/.lest/bin:$PATH\" # added by lest (self install)\nexport B=2\n";
        assert_eq!(
            super::without_marked_lines(content),
            "export A=1\nexport B=2\n"
        );
    }
}
