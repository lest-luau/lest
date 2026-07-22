# Getting started

## Install

### With rokit (recommended)

[rokit](https://github.com/rojo-rbx/rokit) is a toolchain manager for the Luau
ecosystem. It pins Lest's version per project — recorded in `rokit.toml` and
shared with everyone who clones the repo — and puts it on your `PATH`:

```console
$ rokit add lest-luau/lest
```

This is the same mechanism Lest uses to pin `lune` and `lute` for its own
spawned-runtime backends, so a project that already uses rokit gains nothing new
to install.

### From source

With [Rust](https://rustup.rs):

```console
$ git clone https://github.com/lest-luau/lest
$ cd lest
$ cargo build --release
$ ./target/release/lest self install
```

`lest self install` copies the binary into `~/.lest/bin` and adds that directory
to your `PATH`. `lest self uninstall` reverses both. If you'd rather manage the
binary yourself, skip it and put `target/release/lest` wherever you like.

Building from source is also how you get an unreleased build — the `main`
branch, ahead of the latest tagged release.

### Verify

```console
$ lest --version
```

## Scaffold a project

```console
$ lest init
```

`lest init` detects what it can before asking anything — whether a rojo project
file is present, whether `lune` or `lute` are on your `PATH`, whether you
already have spec files — then asks only about what's left: the default backend,
the main suite's name, whether to add a runtime-scripts suite and an opt-in
cloud engine suite, the main suite's include glob, whether to add a `lest` alias
to your `.luaurc` so specs can `require('@lest')` from anywhere, whether to
write an example spec, and whether to add `/.lest` to your `.gitignore`.

The extra suites come *before* the main glob on purpose: if you add one (or a
rojo project was detected), the main include is pre-filled as
`src/**/*.spec.luau` so it stays out of `tests/scripts/` and `tests/engine/`,
and a glob that would still reach an extra suite's directory is rejected with an
explanation — a spec matched by two suites runs in both. Globs use `/` as the
separator; backslashes are rejected.

Add `--yes` (or `-y`) to accept every default without prompting, which is what
you want in a script or a container; `--no-color` disables colored prompts.

It's safe to re-run. If a `lest.toml` already exists, init asks
`A lest.toml already exists. Overwrite it?` — answering yes replaces the file
wholesale, answering no leaves it untouched. With `--yes` it refuses to
overwrite and exits with code 2.

Four things land in your project:

```
lest.toml            your suites and settings
example.spec.luau    a working spec, if you asked for one
.luaurc              a `lest` alias, if you accepted it
.lest/               generated; add it to .gitignore
```

### About `.lest/`

`.lest/` is Lest's scratch directory. It holds `core` — the framework itself,
written out of the binary on demand — plus the generated harness scripts the
Lune and Lute backends run. It is entirely reproducible: delete it and the next
`lest` run puts it back. Don't commit it, and don't edit anything in it.

## Write a spec

A spec is any file matched by a suite's `include` globs — by convention,
`*.spec.luau` beside the code it tests.

```luau
--!strict
local Lest = require('@lest')
local describe, it, expect = Lest.describe, Lest.it, Lest.expect

local cart = require('./cart')

describe('cart', function ()
	it('sums line items', function ()
		expect(cart.total({ 3, 4 })).toBe(7)
	end)
end)

return nil
```

Three things to notice:

- **`require('@lest')`** works because `lest init` wrote that alias into
  `.luaurc`. If you declined it, require the framework by path instead —
  `require('../.lest/core')`, adjusted for where the spec sits.
- **The destructuring line** (`local describe, it, expect = ...`) is
  deliberate. Lest has no ambient globals: your specs require real `.luau`
  files, so Luau-LSP infers types from the implementation and selene needs no
  configuration beyond `std = "luau"`.
- **`return nil`** at the end. Spec files are modules, and Luau modules return
  a value. Lest collects tests through registration, not through what the file
  returns.

## Run it

```console
$ lest
```

Bare `lest` runs every default suite. From there:

```console
$ lest run unit          # a single suite by name
$ lest --watch           # re-run affected specs on save
$ lest -t 'sums'         # only tests whose full name contains "sums"
$ lest --coverage        # add a line-coverage report
$ lest -u                # update snapshots that differ
```

The full surface is in the [CLI reference](cli.md).

## Where to go next

- [Writing tests](writing-tests.md) — lifecycle hooks, nesting, skipping
- [Matchers](matchers.md) — the full assertion vocabulary
- [Backends](backends.md) — testing Lune, Lute, and real engine code
- [Configuration](configuration.md) — every `lest.toml` key
