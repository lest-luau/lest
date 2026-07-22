# CLI reference

```
lest [OPTIONS] [SUITE]...
lest run [OPTIONS] [SUITE]...
lest init [-y|--yes] [--no-color]
lest self install | uninstall
```

A bare `lest` is exactly `lest run` — every flag below works either way.

## `lest run`

Runs test suites. With no `SUITE` arguments, every suite whose `default` isn't
`false` runs.

```console
$ lest                       # every default suite
$ lest run unit              # one suite
$ lest run unit scripts      # several
$ lest run engine            # a default = false suite, by name
```

Naming a suite is the only way to run one configured with `default = false`
outside CI. An unknown suite name is an error listing the ones that exist, and
naming a suite twice (`lest run unit unit`) runs it once — repeats are deduped,
preserving order.

### Selection

#### `-t, --filter <TEXT>`

Run only tests whose **full name** contains `TEXT` — a plain substring, not a
pattern. The full name is the describe path joined with the test's own name,
**separated by single spaces**.

```console
$ lest -t 'cart'
$ lest -t 'parser errors'
```

The pretty reporter shows describe levels as indented nested headers, and the
` › ` that appears in its "Slowest Tests:" block (and in the JUnit `classname`)
is display decoration only — neither indentation nor the glyph is part of the
filterable name, so filtering on `›` matches nothing.

A filter that selects no tests is a tool error (exit 2), not a green run — a
typo'd `-t` in CI must not look like a passing suite.

#### `--changed <REF>`

Run only the specs affected by files that changed since a git ref. Lest diffs
against the ref, then walks the inverted require graph to find every spec whose
transitive requires touched a changed file.

```console
$ lest --changed origin/main
$ lest --changed HEAD~1
```

Needs a git repository and a valid ref; otherwise it's a tool error. When
nothing is affected, that's an answer, not a mistake: the run exits 0 with a
note (and any coverage minimum is explicitly skipped).

`--changed` conflicts with `--watch` — watch mode already selects by change,
from the file system rather than from git, so combining them is rejected rather
than accepted and ignored.

#### `--backend <BACKEND>`

Force every selected suite onto one backend for this run: `native`, `lune`,
`lute`, or `cloud`. A debugging override — see
[Backends](backends.md#overriding-a-backend).

### Watch mode

#### `--watch`

Re-run affected suites when files they depend on change.

```console
$ lest --watch
$ lest run unit --watch
```

Watch mode is graph-driven, not glob-driven. Saving a file invalidates its
content hash; the inverted require graph then yields exactly the specs whose
transitive requires touched it, and only those re-run. Watching is debounced,
hidden files are ignored (except `.luaurc`, which affects resolution), and
`cloud` suites are always excluded — the fast loop never waits on the network.

At startup and after every pass, a dim banner marks the loop as alive:

```console
watching /path/to/project — save a file to re-run (ctrl+c to quit)
```

Saving `lest.toml` reloads the configuration. If the fresh config fails to load
or select, the reload is rejected with a `Warning:` and the previous
configuration stays live — the loop never dies to a half-typed config. If the
file watcher itself stops, that's a tool error (exit 2) rather than a silent
exit that looks like a clean quit.

Pairing `--watch` with `run <suite>` and `-t` is the tight inner loop:

```console
$ lest run unit -t 'parser' --watch
```

### Output

#### `--reporter <REPORTER>`

| Reporter | Output |
| --- | --- |
| `pretty` | **Default.** Nested suites, inline diffs, slowest tests, summary |
| `json` | The event log, one JSON object per line |
| `junit` | JUnit XML, for CI annotations |

All reporters consume the same merged stream regardless of which backend
produced it, tagging each suite's section with the environment it ran in.

Alongside the report itself, the CLI speaks in exactly two voices on stderr:
**diagnostics** — a colored bold label followed by a capitalized sentence with
no trailing period (`Error:` and `Failure:` in bold red, `Warning:` in bold
yellow, e.g. `Error: This is the error`) — and **notes**, dim lowercase
fragments. There is no `lest:` prefix anywhere.

#### `--no-color`

Disable ANSI color. Color is already disabled automatically when the output
stream isn't a terminal, or when the `NO_COLOR` environment variable is set.

### Snapshots

#### `-u, --update`

Overwrite snapshots that differ instead of failing on the difference. See
[Snapshots](snapshots.md).

### Coverage

#### `--coverage`

Measure line coverage (native suites only) and print a coverage report.

#### `--coverage-format <FORMAT>`

`table` (default) prints a box-drawn terminal table (pretty reporter only —
it's omitted under `--reporter json|junit`); `lcov` writes an lcov tracefile to
stdout for Codecov and editor gutters, moving the human test report to stderr
so `> lcov.info` captures a clean tracefile.

#### `--min <PCT>`

Fail with exit code 1 when overall coverage is below `PCT`. Implies
`--coverage`, and overrides `[coverage] min` in config. If nothing was
instrumented — no native suite in the selection — the gate is a tool error
(exit 2), except when `--changed` selected no specs, which exits 0 and skips
the gate with a note. See [Coverage](coverage.md#gating-on-a-minimum).

### Config

#### `--config <PATH>`

Path to the config file. Defaults to `./lest.toml` when present; the config
file's directory becomes the project root.

## `lest init`

Creates `lest.toml` by asking a few questions, after detecting whatever it can
infer — a rojo project file, `lune`/`lute` on `PATH`, existing spec files.

```console
$ lest init
$ lest init --yes         # accept every default, no prompts (also -y)
$ lest init --no-color    # plain prompts
```

Re-running is safe: an existing `lest.toml` prompts
`A lest.toml already exists. Overwrite it?` (yes replaces it, no leaves it
untouched; with `--yes` init refuses and exits 2), and an alias already bound
to `lest` in `.luaurc` is left alone. `.luaurc` is only rewritten when it
parses as plain JSON with no comments; otherwise init prints the snippet for
you to paste. Key order is preserved.

See [Getting started](getting-started.md#scaffold-a-project).

## `lest self`

```console
$ lest self install      # copy into ~/.lest/bin and add it to PATH
$ lest self uninstall    # remove it from PATH and delete ~/.lest/bin
```

On Windows the user `PATH` is edited in the registry with its value kind
preserved, so `%VAR%`-style (`REG_EXPAND_SZ`) entries keep expanding instead of
being baked into whatever they expanded to that day.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Everything passed |
| `1` | Test failures — including timeouts, spec load errors, and a `--min` shortfall |
| `2` | Tool error — bad config, a backend that couldn't start, an undecodable event stream, a filter or coverage gate over nothing |

These are never conflated, which is the whole point. A test that times out or a
spec that fails to load is a *test* failure (1). A backend that can't start, a
protocol line that won't decode, or a suite that produces no outcomes at all is
a *tool* error (2) — the run didn't happen, so calling it a test failure would
be a lie.

## Environment variables

| Variable | Effect |
| --- | --- |
| `ROBLOX_API_KEY` | Open Cloud API key for `cloud` suites |
| `LEST_API_KEY` | Alternative name for the same key |
| `CI` | When set (and not empty, `0`, or `false`), suites with `default = false` run automatically |
| `NO_COLOR` | When set, disables ANSI color everywhere, same as `--no-color` |

A `.env` file at the project root is loaded automatically.
