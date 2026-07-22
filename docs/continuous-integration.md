# Continuous integration

Lest is a single static binary with no Node or Python runtime to install
alongside it, so a CI job is mostly just "get the binary, run `lest`".

## Exit codes

Everything starts here, because CI reacts to exit codes and conflating them is
how a broken pipeline reports green.

| Code | Meaning |
| --- | --- |
| `0` | Everything passed |
| `1` | Test failures |
| `2` | Tool error |

The line between `1` and `2` is deliberate. A test that times out, a spec that
fails to load, an assertion that fails, a `--min` coverage shortfall — those are
**test failures** (1). The project ran and didn't meet its own standard.

A backend that can't start, a config that won't parse, a protocol line that
won't decode, a suite that produces no outcomes at all, a coverage minimum with
no native suite instrumented — those are **tool errors** (2). The run didn't
happen (or measured nothing), and calling that a test failure would be a lie
your dashboard then repeats.

If you're writing a script that reacts to failures, that distinction is the
thing worth branching on:

```bash
lest --reporter junit > results.xml
case $? in
  0) echo "green" ;;
  1) echo "tests failed" ;;
  2) echo "lest itself broke — do not report this as a test failure" >&2 ;;
esac
```

## What `$CI` changes

When `CI` is set to anything other than empty, `0`, or `false`, suites
configured with `default = false` run automatically.

That single rule is what makes a slow `cloud` suite practical: it stays out of
your local loop, and still gates every pull request without anyone having to
remember a flag.

Every mainstream CI provider sets `CI=true` for you.

## Reporters

### JUnit

```console
$ lest --reporter junit > results.xml
```

Writes JUnit XML to stdout, which nearly every CI system understands for inline
test annotations on a pull request.

### JSON

```console
$ lest --reporter json > events.jsonl
```

The raw event log, one JSON object per line — for building your own dashboard or
post-processing a run.

## Coverage

```console
$ lest --coverage --coverage-format=lcov > lcov.info
```

Under `--coverage-format=lcov` the tracefile owns stdout and the human test
report moves to stderr, so the redirect above captures a clean `lcov.info` —
upload it to Codecov, Coveralls, or whatever you use. Add `--min` — or
`[coverage] min` in config — to fail the build on a regression. See
[Coverage](coverage.md).

## Running only what changed

```console
$ lest --changed origin/main
```

Runs only the specs affected by files that changed since a ref, using the
inverted require graph rather than a path heuristic. On a large repository with
a slow suite this turns a pull-request check from minutes into seconds. A
change that affects no specs exits 0 with a note — and any coverage minimum is
explicitly skipped, since gating a run that was asked to contain nothing would
fail every no-op change.

Make sure the ref is actually fetched — most CI providers do a shallow clone by
default, which leaves `origin/main` missing:

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0
```

## Engine tests in CI

The `cloud` backend needs an Open Cloud API key, which is a secret. Put it in
your CI provider's secret store and expose it as an environment variable:

```yaml
env:
  ROBLOX_API_KEY: ${{ secrets.ROBLOX_API_KEY }}
```

The universe and place ids aren't secret and belong in `lest.toml`. See
[Backends → cloud](backends.md#cloud).

Because cloud runs are one Open Cloud task per spec file, parallel CI jobs
against the same place can contend. A dedicated test place per pipeline is the
simple answer for now.

## A GitHub Actions workflow

Add `lest-luau/lest` to your project's `rokit.toml` (`rokit add lest-luau/lest`)
and CI needs nothing but [rokit](https://github.com/rojo-rbx/rokit) — no Rust
toolchain, no build step. `rokit install` brings in Lest alongside whatever
`lune`/`lute` versions you've pinned.

```yaml
name: ci

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0     # so --changed can see the base ref

      - uses: CompeyDev/setup-rokit@v0.1.2
      - run: rokit install --no-trust-check

      - name: Test
        run: lest --reporter junit > results.xml
        env:
          ROBLOX_API_KEY: ${{ secrets.ROBLOX_API_KEY }}

      - name: Coverage
        run: lest run unit --coverage --coverage-format=lcov --min 80 > lcov.info

      - uses: codecov/codecov-action@v4
        with:
          files: lcov.info
```

Two `lest` invocations, deliberately: the coverage pass narrows to the native
suites, since those are the only ones instrumented — no point paying for a
cloud round trip twice. (A single combined invocation is possible —
`--coverage-format=lcov` moves the test report to stderr so the tracefile owns
stdout — but two passes keep each redirect obvious.)
