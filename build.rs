//! Compiles lest/core's Luau sources into the CLI binary.
//!
//! The framework and the runner ship as one artifact, so they can never be
//! different versions of each other. `luau/core` stays the source of
//! truth; this walks it at build time and emits an `EMBEDDED` table of
//! (relative path, source) pairs plus a digest of the whole set.
//!
//! Globbing rather than a hand-written `include_str!` list is deliberate. This
//! repo dogfoods its own working copy through `[settings] core`, so its test
//! suite never touches the embedded snapshot — a module added to core and
//! forgotten here would break only the shipped binary, and only for users.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let core = manifest.join("luau").join("core");
    let core = core
        .canonicalize()
        .unwrap_or_else(|e| panic!("cannot find lest/core sources at {}: {e}", core.display()));

    // Rebuild when a module is added or removed, not just edited: `include_str!`
    // registers each file it reads, but nothing would notice a new sibling.
    println!("cargo:rerun-if-changed={}", core.display());

    let mut modules = Vec::new();
    collect(&core, &core, &mut modules);
    modules.sort();
    assert!(
        modules.iter().any(|(name, _)| name == "init.luau"),
        "lest/core has no init.luau in {}",
        core.display()
    );

    let mut out = String::new();
    out.push_str("/// lest/core's modules as (path relative to core's root, source).\n");
    out.push_str("pub static EMBEDDED: &[(&str, &str)] = &[\n");
    let mut digest = FNV_OFFSET;
    for (name, path) in &modules {
        println!("cargo:rerun-if-changed={}", path.display());
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        hash(&mut digest, name.as_bytes());
        hash(&mut digest, source.as_bytes());
        writeln!(
            out,
            "    ({:?}, include_str!({:?})),",
            name,
            path.display().to_string()
        )
        .expect("writing to a String cannot fail");
    }
    out.push_str("];\n\n");
    out.push_str("/// Digest of every embedded module's path and contents. Paired with the\n");
    out.push_str("/// crate version in the on-disk stamp so an edited core forces a rewrite\n");
    out.push_str("/// even when the version has not moved.\n");
    writeln!(out, "pub const EMBED_DIGEST: u64 = {digest:#x};")
        .expect("writing to a String cannot fail");

    let dest =
        PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("core_embed.rs");
    std::fs::write(&dest, out).unwrap_or_else(|e| panic!("cannot write {}: {e}", dest.display()));
}

/// Collects `.luau`/`.lua` modules under `dir`, keyed by their forward-slashed
/// path relative to `root` so the generated table is platform-independent.
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }
        let is_luau = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("luau") || ext.eq_ignore_ascii_case("lua"));
        if !is_luau {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .expect("collected under root")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push((rel, path));
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn hash(state: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *state ^= byte as u64;
        *state = state.wrapping_mul(FNV_PRIME);
    }
    // Length-delimit each chunk so ("ab", "c") and ("a", "bc") differ.
    *state ^= bytes.len() as u64;
    *state = state.wrapping_mul(FNV_PRIME);
}
