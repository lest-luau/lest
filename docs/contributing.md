# Contributing

Issues and pull requests are welcome. Lest is [MIT licensed](../LICENSE), and
contributions are accepted under that same license.

## Getting set up

You'll need [Rust](https://rustup.rs) and [rokit](https://github.com/rojo-rbx/rokit).

```console
$ git clone https://github.com/lest-luau/lest
$ cd lest
$ rokit install          # lune, lute, stylua, and selene, at the pinned versions
$ cargo build
```

Rust builds the CLI; rokit provides the two runtimes the spawned-runtime suites
need plus the two Luau linters, `stylua` and `selene`, all pinned in
`rokit.toml`.

## Repository layout

One crate, one binary, and one language per directory.

```
lest
├── Cargo.toml        the single package; src/main.rs → lest
├── build.rs          walks luau/core, emits the embedded framework
├── src/              Rust only
│   ├── main.rs       CLI surface, orchestration, exit policy
│   ├── backend/      native · runtime (lune/lute) · cloud/
│   ├── resolve/      require resolver, dependency graph, rojo mapping
│   └── report/       protocol schema, reporters, snapshot format, coverage
├── luau/             Luau only — everything lest ships inside the binary
│   ├── core/         the framework: spec API, matchers, hooks, emitter
│   └── runtime/      in-runtime plumbing: the harness template, cloud collector
├── tests/            every Luau spec, one root, one directory per suite
├── docs/             this documentation
└── lest.toml         lest testing lest
```

Rust tests are inline `#[cfg(test)]` modules beside the code they cover, so
`tests/` never mixes languages.

`src/resolve/` deliberately imports nothing else from the crate — it's a
self-contained unit that could become its own crate with a directory move and a
manifest. Please keep it that way.

## Lest tests itself

The repository dogfoods its own binary. `lest.toml` defines five suites across
all four backends, and the framework under test is this working copy — not the
copy embedded in the binary — via `[settings] core`.

```console
$ cargo test              # the Rust side
$ cargo run -- run        # the Luau side, through lest itself
$ cargo run -- run core   # just the framework's own specs
```

The engine suite is `default = false`, so it stays out of the default run. It
needs an Open Cloud key and a place — see [Backends](backends.md#cloud).

## Before you open a pull request

```console
$ cargo fmt --all --check
$ cargo clippy --all-targets --locked -- -D warnings   # must be clean, not merely compiling
$ cargo test --locked
$ cargo run --locked -- run core coverage lune-runtime lute-runtime
$ stylua --check luau tests
$ selene luau tests
```

These are the exact invocations CI runs (see `.github/workflows/ci.yml`), so a
green local pass is a green CI pass. The lest run names its suites explicitly
rather than running a bare `lest`: the engine suite is `default = false` and
would auto-enable under `$CI`, firing an Open Cloud run per platform — it stays
a manual `lest run engine`. `stylua --check` reports rather than rewrites; drop
the `--check` when you want it to reformat in place.

## Conventions

**Rust.** Comments explain *why*, especially where they guard a subtle failure
mode. Clippy clean at `--all-targets`.

**Luau.** Tabs, single quotes, a space before the paren in function definitions,
`--!strict` at the top of every file. The stylua and selene configs are checked
in; run them rather than matching by eye.

**Naming.** The embedded-VM backend is called **native**, never "vm". The
framework module is **`Lest`** (PascalCase) when required. Matchers are
dot-calls, and negations are spelled `toNot*` — `not` is a Luau keyword, so
Jest's `.not.toBe` isn't available.

## Two things that will bite you

**Path identity is an invariant.** Any code that *generates* a require string
must normalize **both** sides before computing a relative path. Mixing a
normalized path with an unnormalized one diverges right after the Windows drive
letter, producing a require that climbs to the filesystem root and walks back
down. That path reads the right file — but Lune and Lute key their module caches
on it, so a spec reaching the framework by any other spelling loads a *second
copy*, registers its tests into a registry nobody runs, and the suite reports
zero tests while failing nothing. A long `../../../..` run in a generated
require is the symptom.

**The harness template is real Luau, and must stay that way.**
`luau/runtime/harness.luau` is substituted line-by-line, keyed on trailing
`-- __LEST_*__` marker comments, and every marked line holds a working default.
That's what lets it type-check and be formatted like any other file. Keep each
marker at the end of the line it belongs to, and never spell a complete marker
in prose — matching is by substring.

## Architecture

The [documentation](getting-started.md) covers Lest from a user's side; for the
internals, the code is organized to be read. Most modules open with a doc comment
explaining what they own and why, and the trickier invariants (path identity in
`src/resolve/`, the harness template in `src/backend/runtime.rs`) are commented
where they live. Start from `src/main.rs` — its `execute_run` reads
top-to-bottom as config load → suite selection → discovery → per-backend
execution → reporting → exit policy — and follow it into whichever component a
change touches.
