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
backend = "native"          # default for suites that don't say otherwise

[suites.unit]
include = ["src/**/*.spec.luau"]

[suites.scripts]
include = ["tools/**/*.spec.luau"]
backend = "lute"

[suites.engine]
include = ["tests/engine/**/*.spec.luau"]
backend = "cloud"
default = false             # opt in locally; auto-enabled when $CI is set

[cloud]
universe_id = 1234567890
place_id = 9876543210

[settings]
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

## Top level

### `backend`

The default backend for suites that don't declare one.

- **Type:** `"native"` · `"lune"` · `"lute"` · `"cloud"`
- **Default:** `"native"`

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

- **Type:** `"native"` · `"lune"` · `"lute"` · `"cloud"`
- **Default:** the top-level `backend`

### `default`

Whether the suite runs when you type a bare `lest`.

- **Type:** boolean
- **Default:** `true`

`default = false` means the suite runs only when **named explicitly**
(`lest run engine`) or when **`$CI` is set** to anything other than empty, `0`,
or `false`. That combination is what makes a slow cloud suite bearable: it stays
out of your local loop and still gates every pull request.

### `[suites.<name>.cloud]`

Per-suite Open Cloud ids, overriding the top-level `[cloud]` block. Only
consulted for `cloud` suites.

```toml
[suites.engine.cloud]
universe_id = 1234567890
place_id = 9876543210
```

## `[cloud]`

Open Cloud target for cloud suites. These identifiers appear in the Creator
Dashboard URL for your experience and place; they are **not** secret.

| Key | Type | Notes |
| --- | --- | --- |
| `universe_id` | integer or string | The experience |
| `place_id` | integer or string | The place the task runs in |

The API key is deliberately **not** configurable here. It's read from
`ROBLOX_API_KEY` or `LEST_API_KEY` in the environment, or from a `.env` file at
the project root. See [Backends → cloud](backends.md#cloud).

## `[settings]`

### `timeout_ms`

Per-test budget in milliseconds.

- **Type:** integer
- **Default:** `5000`

How it's enforced depends on the backend: `native` uses the VM's interrupt
callback per test, the spawned runtimes scale it into a whole-process budget and
kill the process, and `cloud` turns it into a per-spec deadline inside the
engine.

### `workers`

Native-backend worker threads.

- **Type:** integer
- **Default:** `0` — one per CPU

### `rojo`

Path to the rojo project file.

- **Type:** string
- **Default:** unset

Accepted but not yet consumed — the value is read from the file and otherwise
ignored, including whether the path exists. It lands with the rojo build/publish
path for the cloud backend. Setting it today changes nothing.

### `core`

Path to a copy of the framework on disk, relative to the project root.

- **Type:** string
- **Default:** unset — use the copy embedded in the binary

Leave this alone. The framework ships inside the `lest` binary and is written to
`.lest/core` on demand, which is what guarantees the runner and the framework
can never be different versions. Setting `core` opts out of that — it exists so
the Lest repository can dogfood its own working copy of the framework.

## `[coverage]`

Native suites only. See [Coverage](coverage.md).

### `exclude`

Globs excluded from coverage reporting, matched against root-relative,
forward-slashed paths.

- **Type:** array of strings
- **Default:** `["**/*.spec.luau", "**/*.spec.lua", "Packages/**"]`

Setting this **replaces** the defaults rather than adding to them, so include
the spec-file patterns yourself if you still want them excluded.

### `min`

Fail the run (exit code 1) when overall coverage falls below this percentage.

- **Type:** number
- **Default:** unset — no gate

Setting it turns coverage measurement on for every run, just as `--min` implies
`--coverage` — a gate can't compare against a percentage that was never
measured. `--min` overrides it for a single run. If the gate is set but no
native suite was instrumented, that's a tool error (exit 2) — see
[Coverage](coverage.md#gating-on-a-minimum).

## Precedence

**CLI flag > suite setting > top-level default.**

```console
$ lest run unit --backend lune     # ignores the suite's declared backend
$ lest --min 90                    # overrides [coverage] min
```
