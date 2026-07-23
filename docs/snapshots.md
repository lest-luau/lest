# Snapshots

A snapshot test records what a value serialized to the first time it ran, then
fails if that ever changes. It's the right tool when asserting on a large
structure field by field would be tedious and unreadable — rendered output,
config resolution, a parse tree.

```luau
it('renders the summary', function ()
	expect(render(report)).toMatchSnapshot()
end)
```

## The lifecycle

| State | What happens |
| --- | --- |
| No stored value | Written, and the test **passes** |
| Stored value matches | **Passes** |
| Stored value differs | **Fails**, with a diff |
| `lest -u` and it differs | Overwritten, and the test **passes** |

A mismatch fails the test that produced it, like any other failure — the diff
renders in the test's failure block, labeled with the snapshot's key (one test
can hold several snapshots):

```
  ✗ renders the report (0.2ms)

    Snapshot "renders the report 1" did not match:
      - { status = "ok" }
      + { status = "down" }
```

The first run is meant to pass — that's how the snapshot gets created. Which
means **you have to review the file**: a snapshot committed without being read
records whatever the code did that day, bug included.

```console
$ lest              # fails on any difference
$ lest -u           # accept the new output
```

## Where they live

Beside the spec, in a `__snapshots__` directory:

```
src/
├── report.luau
├── report.spec.luau
└── __snapshots__/
    └── report.spec.luau.snap
```

Commit them. They're part of the test.

## Multiple snapshots in one test

Each `toMatchSnapshot()` call in a test gets a numbered key, so several calls
stay distinct. Passing a hint replaces that number with your label — more
readable in the file, and stable if you reorder the calls:

```luau
it('renders both modes', function ()
	expect(render(doc)).toMatchSnapshot('default')
	expect(render(doc, { compact = true })).toMatchSnapshot('compact')
end)
```

## What can be snapshotted

Serialization is **lossless**: there is no depth cap, so a change anywhere in a
deeply nested value fails the snapshot. (Matcher failure *messages* do truncate
deep values — that's message formatting only, and never affects what a snapshot
stores or compares.)

Two shapes can't serialize deterministically, and snapshotting them is an
assertion failure rather than a degraded snapshot:

- a **cyclic** value — there is no finite lossless rendering of one;
- a value containing a **table, function, userdata, or other non-scalar table
  key** — `tostring` on those embeds a memory address, which changes across
  runs and backends.

Non-scalar reference *values* (functions, threads, userdata) are fine: they
serialize as their type name, e.g. `<function>`.

## Obsolete snapshots

When a stored key no longer corresponds to any test that ran — you renamed a
test or deleted it — Lest reports it as **obsolete** in the run summary rather
than silently keeping it. `lest -u` prunes them.

Obsolete detection is skipped entirely on a **filtered** run — a `-t` name
filter, `--changed`, or watch mode's spec restriction — because under a filter,
"stored but not produced" only means "that test didn't run this time". Pruning
in that state would delete live snapshots. Running a single suite by name
(`lest run unit`) is *not* a filtered run: every test in the suite executes, so
detection works normally within it.

One narrowing to know about: detection compares against the `.snap` files of
spec files that produced **at least one snapshot this run**. A spec file whose
tests all stopped calling `toMatchSnapshot()` never opens its `.snap`, so keys
stranded there aren't flagged.

## The file format

`.snap` files are plain text, deliberately readable and diff-friendly:

```text
lest snapshot v1

report > renders the summary 1 3
Summary
  passed: 2
  failed: 0
```

The header line for each entry is the test's full name — the describe path and
test name joined with ` > ` — plus its hint or number, followed by a line count. Values are stored **literally** — never escaped or
quoted — so a snapshot reads as itself and line-oriented tools (git diffs,
editor gutters, review UIs) stay meaningful. Entries are sorted by key, so the
file is deterministic regardless of what order the tests ran in.

Only the key is escaped, and only for the two characters that would break its
header line (`\` and a newline).

## Across backends

Snapshots behave identically on every backend — `native`, `lune`, `lute`, and
`cloud`. The framework only ever *reports* the serialized value as a protocol
event; the comparison, the diff, the first write, and the `-u` update all happen
in the CLI. There is exactly one implementation, so a suite can move between
backends without its snapshots changing.
