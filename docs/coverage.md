# Coverage

```console
$ lest --coverage
```

Lest measures **line coverage** on `native` suites by compiling with Luau's
coverage level, reading each loaded function's recorded hit counts after the
run, and aggregating per-file line hits across every worker.

```console
Coverage:
┌─────────────────┬─────────┬─────────┐
│ File            │   Lines │ Covered │
├─────────────────┼─────────┼─────────┤
│ src/cart.luau   │   34/36 │   94.4% │
│ src/parser.luau │   81/97 │   83.5% │
│ src/fetch.luau  │       — │       — │
├─────────────────┼─────────┼─────────┤
│ All files       │ 115/133 │   86.5% │
└─────────────────┴─────────┴─────────┘
```

## Native suites only — by design

The VM hooks that produce hit counts don't exist across a process boundary, so
`lune`, `lute`, and `cloud` suites can't be instrumented.

Files those suites covered show a dimmed `—` in the table rather than being
counted as zero (`src/fetch.luau` above), and they're left out of the lcov
output entirely (an lcov consumer treats "absent" and "not instrumented" the
same way). Honest numbers or none — reporting a Lune-only module as 0% would be
worse than saying nothing, because it would make a well-tested file look like a
gap.

If coverage matters for a module, give it a native suite.

## What's excluded

By default:

```toml
["**/*.spec.luau", "**/*.spec.lua", "Packages/**"]
```

Spec files aren't the code under test, and vendored packages aren't yours.
Override in config:

```toml
[coverage]
exclude = ["**/*.spec.luau", "vendor/**", "src/generated/**"]
```

Setting `exclude` **replaces** the defaults rather than adding to them — list
the spec patterns yourself if you still want them out.

Globs match against root-relative, forward-slashed paths, so one pattern works
the same on every platform.

## Output formats

### `table` (default)

The box-drawn terminal table above: per-file covered/total lines and a
percentage, ruled off from an `All files` row. It belongs to the pretty report,
so it isn't printed under `--reporter json` or `--reporter junit` — splicing a
table into those streams would corrupt the document they promise.

### `lcov`

```console
$ lest --coverage --coverage-format=lcov > lcov.info
```

Writes a standard lcov tracefile to **stdout**, which is what Codecov,
Coveralls, and editor gutter extensions consume. Under this format the lcov
document owns stdout and the human test report moves to **stderr**, so
redirecting stdout as above captures a clean tracefile and nothing else.

## Gating on a minimum

```console
$ lest --min 80
```

Fails the run with **exit code 1** when overall coverage is below the given
percentage, printing what it got and what it needed. `--min` implies
`--coverage`, so you don't need both.

The same gate can live in config, which is usually where you want it — the
number is a property of the project, not of one invocation:

```toml
[coverage]
min = 80
```

`--min` overrides `[coverage] min` for a single run, which is handy for checking
where you'd land before committing to a higher bar. Setting `[coverage] min`
also turns coverage measurement on by itself, for the same reason `--min`
implies `--coverage`.

A coverage shortfall is a **test failure** (exit 1), not a tool error (exit 2) —
the run happened and the project didn't meet its own standard. See
[Continuous integration](continuous-integration.md).

A gate over *nothing* is different: when a minimum is set but no native suite
was instrumented — say every selected suite runs on `lune` — there is no
percentage to compare, and exiting 0 would green-light CI while measuring
nothing. That's a **tool error** (exit 2). The one exception is `--changed`
selecting no affected specs: the empty run was requested, so it exits 0 and the
gate is explicitly skipped with a note
(`coverage minimum not enforced — --changed selected no specs`).
