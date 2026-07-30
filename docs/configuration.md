# Configuration

Lest reads `lest.toml` from the working directory, or from `--config <PATH>`.
The config's directory is the **project root** — every glob and relative path
below resolves against it.

Without a `lest.toml`, Lest runs one `native` suite over `**/*.spec.luau`. For
many projects that is genuinely enough.

Every key is optional — even `[suites.*]`. A real config for a real project:

```toml
[suites.unit]
include = ["src/**/*.spec.luau"]
```

## A full example

```toml
[suites.unit]
include = ["src/**/*.spec.luau"]

[suites.scripts]
include = ["tools/**/*.spec.luau"]
backend = "lute"

[suites.engine]
include = ["tests/engine/**/*.spec.luau"]
backend = "cloud"
default = false             # opt in locally; auto-enabled when $CI is set

[place]                     # the Roblox place engine suites run in
universe_id = 1234567890
place_id = 9876543210
file = "test-place.rbxl"    # uploaded by cloud, launched by studio
rojo = "default.project.json"

[settings]
backend = "native"          # default for suites that don't say otherwise
timeout_ms = 5000
workers = 0                 # 0 = one per CPU

[coverage]
exclude = ["**/*.spec.luau", "Packages/**"]
min = 80
```

Named suite tables keep the file scannable: someone new reads it and knows the
project's whole testing story.

Unrecognized keys are tolerated — a config written for a newer Lest still
parses — but not silently: each one is named in a warning on stderr
(`Warning: Ignoring unrecognized key in lest.toml: bakcend`), because a typo'd
key otherwise looks exactly like a working config.

Every key lives in a table. (A bare top-level `backend` is the 0.3
spelling of `[settings] backend`; it still works in 0.4 with a warning and
is removed in 0.5 — see [Migrating from 0.3](#migrating-from-03).)

## `[suites.<name>]`

The table name is the suite's name, which is what you pass to `lest run <name>`
and what the reporter labels its section with.

Suites are optional: when a config declares none — an empty `lest.toml` is
valid — Lest synthesizes a suite named `specs` over `**/*.spec.luau` on the
default backend, exactly as if there were no config file at all. Declaring any
suite replaces that synthesized one.

### `include`

Glob patterns selecting the suite's spec files, relative to the project root.

- **Type:** array of strings
- **Required**

```toml
include = ["src/**/*.spec.luau", "lib/**/*.spec.luau"]
```

Hidden entries are never discovered: dot-directories (`.git`, `.lest`) and
dot-files (`.foo.spec.luau`) are skipped even when a glob matches them. The
watcher ignores hidden files too, so a hidden spec would run once and then
never re-run on save — not running it at all is the consistent reading.

### `backend`

Where this suite's specs run, overriding the top-level default. See
[Backends](backends.md).

- **Type:** `"native"` · `"lune"` · `"lute"` · `"cloud"` · `"studio"`
- **Default:** `[settings] backend`

### `default`

Whether the suite runs when you type a bare `lest`.

- **Type:** boolean
- **Default:** `true`

`default = false` means the suite runs only when **named explicitly**
(`lest run engine`) or when **`$CI` is set** to anything other than empty, `0`,
or `false`. That combination is what makes a slow cloud suite bearable: it stays
out of your local loop and still gates every pull request.

### `[suites.<name>.place]`

A per-suite place, overriding the top-level `[place]` field by field. For
projects whose engine suites target different places.

```toml
[suites.engine.place]
universe_id = 1234567890
place_id = 9876543210
```

## `[place]`

The Roblox place engine suites run in — backend-agnostic: the **cloud**
backend targets it through Open Cloud, and the **studio** backend launches
it. The identifiers appear in the Creator Dashboard URL for your experience
and place; they are **not** secret.

| Key | Type | Notes |
| --- | --- | --- |
| `universe_id` | integer or string | The experience |
| `place_id` | integer or string | The place |
| `file` | string | A built `.rbxl`/`.rbxlx`, root-relative |
| `rojo` | string | Rojo project file mapping the filesystem into the place |

When `file` is set, every **cloud** run makes sure the place holds exactly
that file: it is uploaded as a new **saved** version — skipped when its
content hash matches the last upload, recorded in
`.lest/place-versions.json` — and every task is **pinned** to that version.
Without it, tasks run against whatever the place currently holds, which is
fine for an empty place and a foot-gun for a populated one: forget to
publish after a fixture change and the suite quietly tests last week's
place. The upload needs the **universe-places write** scope on the API key,
alongside the Luau-execution scope. The **studio** backend launches `file`
directly (or the published place when only the ids are set).

With `rojo` set, a string require whose target the project file maps to a
ModuleScript in the place is **delegated**: instead of bundling a private
copy of the module, the generated require walks to the live instance and
hands it to the engine's own `require`. The spec and the place's own code
then share one module through the engine's cache — a plain
`require('../packages/thing/src')` reaches the same singleton the place's
scripts see, with full static types in your editor. Details worth knowing:

- Only targets mapped to a **ModuleScript** delegate; anything else (a
  mapped `Script`, a folder, an unmapped file) bundles exactly as before.
- `lest/core` never delegates, even if your project file maps it — the
  framework must be the copy your CLI shipped.
- If the mapped instance is missing at run time, the test fails with the
  mapped path and a pointer at the likely cause (a stale place) — pair
  `rojo` with `file` and that failure mode disappears.
- The native/lune/lute backends ignore this key; requires there resolve on
  disk as always.

The API key is deliberately **not** configurable here. It's read from
`ROBLOX_API_KEY` or `LEST_API_KEY` in the environment, or from a `.env`
file at the project root. See [Backends → cloud](backends.md#cloud).

## `[settings]`

### `backend`

The default backend for suites that don't declare one.

- **Type:** `"native"` · `"lune"` · `"lute"` · `"cloud"` · `"studio"`
- **Default:** `"native"`

Precedence for where a suite runs: the `--backend` CLI flag, then the
suite's own `backend`, then this key.

### `timeout_ms`

Per-test budget in milliseconds.

- **Type:** integer
- **Default:** `5000`

How it's enforced depends on the backend: `native` uses the VM's interrupt
callback per test, the spawned runtimes scale it into a whole-process budget and
kill the process, and `cloud` and `studio` turn it into a per-spec deadline
inside the engine.

### `workers`

Native-backend worker threads.

- **Type:** integer
- **Default:** `0` — one per CPU

### `core`

Path to a copy of the framework on disk, relative to the project root.

- **Type:** string
- **Default:** unset — use the copy embedded in the binary

Leave this alone. The framework ships inside the `lest` binary and is written to
`.lest/core` on demand, which is what guarantees the runner and the framework
can never be different versions. Setting `core` opts out of that — it exists so
the Lest repository can dogfood its own working copy of the framework.

## `[studio]`

Settings for the studio backend (see **[Studio](studio.md)**). The place it
launches comes from the shared `[place]` block (`file`, or
`place_id` + `universe_id`).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `executable` | string | platform install location | Path to the Roblox Studio binary, for non-standard installs. |

## `[coverage]`

Native suites only. See [Coverage](coverage.md).

### `include`

Globs limiting coverage reporting to the files you consider yours, matched
against root-relative, forward-slashed paths.

- **Type:** array of strings
- **Default:** unset — no narrowing, leaving `exclude` to decide alone

`exclude` applies on top, so a file matching both is excluded, and `exclude`
keeps its own defaults whether or not `include` is set. An empty list is a
config error (exit 2): remove the key to report everything. Lest's own framework
is never reported regardless.

### `exclude`

Globs excluded from coverage reporting, matched against root-relative,
forward-slashed paths.

- **Type:** array of strings
- **Default:** `["**/*.spec.luau", "**/*.spec.lua", "Packages/**"]`

Setting this **replaces** the defaults rather than adding to them, so include
the spec-file patterns yourself if you still want them excluded. Setting
`include` does not change this default.

Both keys are matched case-sensitively, and neither supports gitignore-style
negation: a leading `!` is a literal character, not an operator. `*` stops at a
directory boundary, as in a suite's `include`, so `**` is what covers a whole
subtree. See [Coverage](coverage.md#glob-syntax).

### `min`

Fail the run (exit code 1) when overall coverage falls below this percentage.

- **Type:** number
- **Default:** unset — no gate

Setting it turns coverage measurement on for every run, just as `--min` implies
`--coverage` — a gate can't compare against a percentage that was never
measured. `--min` overrides it for a single run. If the gate is set but nothing
reached the table — no native suite ran, or `include` matched nothing — that's a
tool error (exit 2) — see [Coverage](coverage.md#gating-on-a-minimum).

## Precedence

**CLI flag > suite setting > top-level default.**

```console
$ lest run unit --backend lune     # ignores the suite's declared backend
$ lest --min 90                    # overrides [coverage] min
```

## Migrating from 0.4

**`*` no longer crosses `/` in `[coverage]` globs.** Through 0.4 a single `*` in
`include`/`exclude` matched across directory separators, which made `src/*` and
`src/**` identical and meant `exclude = ["vendor/*"]` removed the entire
`vendor/` tree. From 0.5 `*` stops at a boundary and `**` spans depth, matching
how a suite's `include` has always behaved.

This is a silent change where it matters: an exclude that used to take a whole
subtree now takes one level of it, those files reappear in the table, and a
`--min` gate that passed can start failing without the config being touched. To
keep the old meaning, replace any pattern component that is a bare `*` with
`**`:

| 0.4 | 0.5 |
| --- | --- |
| `vendor/*` | `vendor/**` |
| `src/*/generated.luau` | `src/**/generated.luau` |

Lest warns on startup for each pattern that needs this, naming the rewrite. The
shipped defaults (`**/*.spec.luau`, `Packages/**`) are unaffected, as is any
pattern whose `*` sits inside a filename, like `src/*.gen.luau`.

## Migrating from 0.3

0.4 renamed the place-related keys; the old spellings still work in 0.4
(each with a warning naming its new home) and are **removed in 0.5**:

| 0.3 | 0.4 |
| --- | --- |
| `backend` (top level) | `[settings] backend` |
| `[cloud] universe_id` | `[place] universe_id` |
| `[cloud] place_id` | `[place] place_id` |
| `[cloud] place_file` | `[place] file` |
| `[settings] rojo` | `[place] rojo` |
| `[suites.<name>.cloud]` | `[suites.<name>.place]` |

When both spellings are present, the new one wins.
