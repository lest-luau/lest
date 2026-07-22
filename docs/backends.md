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
  with [rokit](https://github.com/rojo-rbx/rokit):

  ```console
  $ rokit add lune-org/lune
  $ rokit add luau-lang/lute
  ```

Lute's AST and filesystem APIs make a `lute` suite the natural home for testing
code transforms and tooling scripts.

## cloud

For code that touches real engine APIs — Instances, services, the DataModel —
there is no faking it. The `cloud` backend bundles your specs and the framework
into one self-contained script, submits it as an Open Cloud **Luau execution
task** against a real place, polls to completion, and decodes the collected
events back into the same report.

```toml
[suites.engine]
include = ["tests/engine/**/*.spec.luau"]
backend = "cloud"
default = false          # opt in locally; auto-enabled when $CI is set

[cloud]
universe_id = 1234567890
place_id = 9876543210
```

### Setup

1. **A published place to run against.** The task executes inside it; a small
   dedicated test place is the usual arrangement.
2. **The universe and place ids.** Both appear in the Creator Dashboard URL for
   your experience and place. They aren't secret, so they belong in
   `lest.toml` — under `[cloud]`, or per-suite as `[suites.<name>.cloud]`.
3. **An API key** with the universe-places Luau-execution scope, created at
   [create.roblox.com/dashboard/credentials](https://create.roblox.com/dashboard/credentials).

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

### Requiring place modules

The bundle is self-contained, so an empty place works — but the place doesn't
have to be empty. If yours is populated (a rojo-built place with fixtures as
real ModuleScripts, say), a require whose argument is a ModuleScript Instance
is handed to the engine's own `require`:

```luau
local fixture = game:GetService('ServerStorage').Fixtures.recorder
local Recorder = require(fixture)
```

Delegated requires go through the engine's native module cache, so a spec and
in-place code requiring the same ModuleScript get the same table — shared state
and module identity survive, which no bundled copy of the module could
guarantee.

Two rules keep the boundary sharp:

- **String requires belong to the bundler.** They must resolve on disk at
  bundle time, and an unresolved one is a loud error, never a silent fallback.
  That includes *dynamic* string requires — a variable holding a path can't be
  resolved from the CLI and isn't supported on this backend.
- **Everything else belongs to the engine.** Instances (and legacy asset ids)
  pass through untouched, and the engine's own errors surface unchanged.

Snapshots work on cloud exactly as they do everywhere else — comparison,
writing, and `-u` updates all happen CLI-side, so the backend makes no
difference. See [Snapshots](snapshots.md#across-backends).

## Overriding a backend

`--backend` forces every selected suite onto one backend for a single run:

```console
$ lest run unit --backend lune
```

It's a debugging tool — for checking that a suite behaves the same in another
environment — not a substitute for declaring the right backend in config.
Precedence is **CLI flag > suite setting > top-level default**.
