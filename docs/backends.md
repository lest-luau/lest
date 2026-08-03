# Backends

A **backend** is where a suite's specs actually execute. It's a property of the
tests, not something you type at the command line: you declare it once per suite
in `lest.toml`, and everything downstream — reporters, snapshots, name filters,
CI output — neither knows nor cares where a test ran.

| Backend | Runs in | Coverage | Watch mode |
| --- | --- | :---: | :---: |
| [`native`](#native) | An embedded Luau VM inside the CLI | ✅ | ✅ |
| [`lune`](#lune--lute) | A spawned `lune run` process | — | ✅ |
| [`lute`](#lune--lute) | A spawned `lute run` process | — | ✅ |
| [`cloud`](#cloud) | A real Roblox place via Open Cloud | — | — |
| [`studio`](#studio) | A launched Roblox Studio | — | — |
| [`gargantuan`](#gargantuan-experimental) | A spawned headless Gargantuan engine | — | — |

**No backend fakes an environment.** Nothing mocks Instances, and nothing
reimplements a runtime's standard library. If a test needs an environment, Lest
runs it in that environment — partial mocks produce confident wrong tests.

## native

The default, and the fast one. The CLI embeds a Luau VM and runs specs on a
worker pool — one worker per CPU by default, one fresh VM per spec file. Nothing
is shared between them, so a spec can never see module state left behind by
another.

```toml
[suites.unit]
include = ["src/**/*.spec.luau"]
# backend = "native" is the default
```

It's the only backend with **line coverage** (the VM exposes the hooks) and the
only one fast enough to sit under watch mode comfortably. Per-test timeouts come
from the VM's interrupt callback, so a runaway loop is caught rather than
hanging the run.

It has no `@lune/*`, no `@lute/*`, and no engine APIs. A spec that requires one
fails with a message naming the backend that has it, rather than a confusing
resolution error.

## lune / lute

One abstraction, two runtimes. Because the framework is pure Luau, it runs
unmodified in both.

```toml
[suites.scripts]
include = ["tools/**/*.spec.luau"]
backend = "lune"

[suites.transforms]
include = ["tests/lute/**/*.spec.luau"]
backend = "lute"
```

The CLI generates a harness script into `.lest/`, spawns `lune run` or
`lute run` on it, and decodes the protocol events the harness prints back into
the same results bus every other backend feeds. Your tests get the *real*
runtime APIs because they genuinely run in that runtime — there is no shim to
drift out of date as Lune and Lute evolve.

Events travel on stdout as sentinel-prefixed JSON lines, so test code that
prints can't corrupt the stream; unprefixed output passes through as ordinary
output. The sentinel is a framing device, not a security boundary — stdout is
shared, so a test that deliberately printed the prefix could emit an event. That
is a non-goal: the code being framed is your own test suite.

The costs are real and worth knowing:

- **No coverage.** The VM hooks aren't available across a process boundary.
- **Process-level isolation.** The whole suite runs in one process, so the
  timeout is a whole-suite budget enforced by killing it.
- **The runtime must be installed.** Lest checks `PATH` and, if it's missing,
  prints the exact install command rather than a spawn error. Pin the version
  with [LPM](https://luaupm.com):

  ```console
  $ lpm tool add lune-org/lune
  $ lpm tool add luau-lang/lute
  $ lpm i
  ```

  (or with [rokit](https://github.com/rojo-rbx/rokit): `rokit add
  lune-org/lune`, `rokit add luau-lang/lute`).

Lute's AST and filesystem APIs make a `lute` suite the natural home for testing
code transforms and tooling scripts.

## cloud

For code that touches real engine APIs — Instances, services, the DataModel —
there is no faking it. The `cloud` backend bundles your specs and the framework
into one self-contained script, submits it as an Open Cloud **Luau execution
task** against a real place, polls to completion, and decodes the collected
events back into the same report.

Each task boots a fresh Roblox **game server** for your place and runs the
bundle as an ordinary server script: `RunService:IsServer()` is true,
`IsClient()` and `IsStudio()` are false, and server-only services behave as
they do in production. The studio backend runs the same suite in a different
context — [Execution context](studio.md#execution-context) has the
comparison.

```toml
[suites.engine]
include = ["tests/engine/**/*.spec.luau"]
backend = "cloud"
default = false          # opt in locally; auto-enabled when $CI is set

[place]
universe_id = 1234567890
place_id = 9876543210
```

### Setup

1. **A published place to run against.** The task executes inside it; a small
   dedicated test place is the usual arrangement.
2. **The universe and place ids.** Both appear in the Creator Dashboard URL for
   your experience and place. They aren't secret, so they belong in
   `lest.toml` — under `[place]`, or per-suite as `[suites.<name>.place]`.
3. **An API key** with the universe-places Luau-execution scope, created at
   [create.roblox.com/dashboard/credentials](https://create.roblox.com/dashboard/credentials).
   Add the universe-places **write** scope too if you use `[place] file` below.

The key **is** secret and is read from the environment only — never from
`lest.toml`, and never printed:

```console
$ export ROBLOX_API_KEY=…      # or LEST_API_KEY
```

A `.env` file at the project root is loaded automatically, which is convenient
locally. Don't commit it.

### Behavior

Cloud runs **one task per spec file**, so each spec's events arrive already
isolated and snapshots attribute correctly — at the cost of one round trip per
spec. Each task's in-engine deadline is derived from `timeout_ms` for that one
spec file, so it doesn't grow with the size of the suite, and transient Open
Cloud responses (rate limits, server errors) are retried automatically,
honoring the server's `Retry-After`. It is still slow by physics, needs the
network, and is therefore:

- **opt-in locally** — give the suite `default = false` and run it by name
- **auto-enabled in CI** — a suite with `default = false` runs when `$CI` is set
- **always ignored by watch mode** — the fast loop never waits on the network

Nothing needs to be installed in the project for engine tests: the in-engine
collector and task scheduler are compiled into the CLI and inlined into the
bundle.

### Keeping the place current

Lest runs against a place, and with `[place] file` it also *puts one
there*: name a built `.rbxl`/`.rbxlx` and every cloud run uploads it as a new
saved version first — skipped when the file's content hash hasn't changed —
and pins every task to exactly that version. Build with rojo, point Lest at
the output, and the "someone forgot to publish after a fixture change" run
against a stale place stops being possible:

```toml
[place]
universe_id = 1234567890
place_id = 9876543210
file = "test-place.rbxl"           # e.g. from `rojo build -o test-place.rbxl`
```

### Requiring place modules

The bundle is self-contained, so an empty place works — but the place doesn't
have to be empty. If yours is populated (a rojo-built place with fixtures as
real ModuleScripts, say), there are two ways a spec reaches those modules.

**With `[place] rojo` set** (the good way): point Lest at your rojo project
file, and a plain string require of a mapped module is *delegated* to the
place. The bundler sees that `../fixtures/recorder` maps to
`ServerStorage.Fixtures.recorder`, skips bundling it, and the generated
require resolves the live instance and hands it to the engine's `require`:

```toml
[place]
rojo = "default.project.json"
```

```luau
local Recorder = require('../fixtures/recorder')   -- the place's copy, fully typed
```

Because the file is required by its real path, luau-lsp infers full types from
the implementation — no `:: typeof(...)` casts — and because the engine's
cache owns the module, the spec and in-place code share one table.

**Without it**, a require whose argument is a ModuleScript Instance is handed
to the engine's own `require`:

```luau
local fixture = game:GetService('ServerStorage').Fixtures.recorder
local Recorder = require(fixture)
```

Delegated requires — both kinds — go through the engine's native module cache,
so a spec and in-place code requiring the same ModuleScript get the same
table: shared state and module identity survive, which no bundled copy of the
module could guarantee. Beware the un-mapped middle ground: a string require
of a module that also lives in the place, *without* `[place] rojo`, bundles a
private copy with its own state.

Two rules keep the boundary sharp:

- **String requires belong to the bundler.** They must resolve on disk at
  bundle time — into the bundle, or through the project file into the place —
  and an unresolved one is a loud error, never a silent fallback. That
  includes *dynamic* string requires — a variable holding a path can't be
  resolved from the CLI and isn't supported on this backend.
- **Everything else belongs to the engine.** Instances (and legacy asset ids)
  pass through untouched, and the engine's own errors surface unchanged.

Snapshots work on cloud exactly as they do everywhere else — comparison,
writing, and `-u` updates all happen CLI-side, so the backend makes no
difference. See [Snapshots](snapshots.md#across-backends).

## studio

Engine suites via a **launched Roblox Studio** — the local, zero-click
complement to cloud, using Studio's official command-line interface.

```toml
[suites.engine]
include = ["tests/engine/**/*.spec.luau"]
backend = "cloud"           # CI stays on cloud
default = false
```

```console
$ lest run engine --backend studio     # the same suite, in a launched Studio
```

No setup beyond a place to run against: Lest bundles the suite, launches
Studio on your `[place] file` (or published place), and decodes the
results from Studio's output file when it quits. What carries over from
cloud: the same bundling, the same `[place] rojo` delegation, the same
CLI-side snapshots, the same `[place]` configuration. What differs:
everything runs locally, every run pays a Studio boot (~15-45s), and the
**execution context** — cloud runs specs on a real game server, studio runs
them in an edit-mode session at the command bar's permission level, which
matters the moment a spec asks `IsServer()` or touches DataStores
([Execution context](studio.md#execution-context)). The studio backend
refuses to run under `$CI`, and watch mode does not include it. Details and
troubleshooting: **[Studio](studio.md)**.

## gargantuan (experimental)

[Gargantuan](https://github.com/teamfireworks/gargantuan) is an independent,
Roblox-shaped game engine scripted with Luau — a `game` DataModel,
Instances, Signals, a `task` library. The gargantuan backend runs specs
inside the real engine, headless: Lest bundles the suite (the same bundler
as cloud and studio), spawns `gargantuan --script <bundle> --headless`, and
decodes sentinel-framed events from its stdout with the lune/lute decoder (in bursts rather than live — the engine never flushes `print`).
No emulation, per the usual rule — specs get the engine's actual Instances
because they genuinely run in it.

```toml
[suites.engine-gg]
include = ["tests/gargantuan/**/*.spec.luau"]
backend = "gargantuan"
default = false

[gargantuan]
binary = "vendor/gargantuan/build/gargantuan"
```

**Experimental, stated plainly.** The engine is pre-release: it has no
tagged releases (build it from source and point `[gargantuan] binary` at
the result — with no `binary` configured, Lest looks for `gargantuan` on
`PATH`), and an API surface that is still filling in, so specs will find
`not yet implemented` edges. Those are engine facts, not test failures.
Excluded from watch mode and from `$CI` auto-enable; run it by naming the
suite explicitly.

How a run ends depends on the engine build: on engines with
`ProcessService`, Lest's generated entrypoint exits the engine cleanly
(`ExitAsync(0)`) once the suite completes; on builds that predate the
service (or whose `ExitAsync` fails), Lest kills the engine a few
seconds after the suite's completion marker arrives — deliberate, not an
error.

## Overriding a backend

`--backend` forces every selected suite onto one backend for a single run:

```console
$ lest run unit --backend lune
```

It's a debugging tool — for checking that a suite behaves the same in another
environment — not a substitute for declaring the right backend in config.
Precedence is **CLI flag > suite setting > top-level default**.
