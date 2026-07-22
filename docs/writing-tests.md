# Writing tests

Every spec file starts the same way: require the framework, pull the API into
locals, declare tests, return.

```luau
--!strict
local Lest = require('@lest')
local describe, it, expect = Lest.describe, Lest.it, Lest.expect

describe('parser', function ()
	it('reads an empty document', function ()
		expect(parse('')).toEqual({})
	end)
end)

return nil
```

## `describe` and `it`

`it(name, fn)` registers one test. `describe(name, body)` groups tests under a
label; the body runs immediately, at *collection* time, and its only job is to
register things.

Blocks nest as deeply as you like. A test's full name is its describe path
joined with its own name, separated by single spaces — that's what `-t` filters
against:

```luau
describe('parser', function ()
	describe('errors', function ()
		it('reports the line number', function ()   -- "parser errors reports the line number"
			-- ...
		end)
	end)
end)
```

The pretty reporter shows those levels as indented nested headers (the ` › `
glyph appears only in its "Slowest Tests:" block and the JUnit `classname`) —
either way, the filterable name is the space-joined one above.

Two tests may never share a full name within a spec file — including tests
under same-named sibling `describe` blocks, since the blocks' labels join
identically. A duplicate is an error at collection time, because two tests
with one name would collide on snapshot keys and be indistinguishable to
reports and `-t`.

Because a `describe` body runs during collection, an error thrown there fails
collection — Lest reports it against the block by name so you can see which one
broke, and the spec counts as failed (exit code 1, like any other failing
test). Keep bodies to registration and hooks; put real work inside `it` or a
lifecycle hook. Calling `describe`, `it`, or a hook *after* the run has started
— from inside a test body — is a hard error rather than a registration that
silently vanishes.

## Lifecycle hooks

Four hooks, each scoped to the `describe` block that registers it (or to the
whole file at the top level):

| Hook | When |
| --- | --- |
| `Lest.beforeAll(fn)` | Once, before the first test in the block |
| `Lest.beforeEach(fn)` | Before every test in the block, outermost first |
| `Lest.afterEach(fn)` | After every test in the block, innermost first |
| `Lest.afterAll(fn)` | Once, after the last test in the block |

```luau
local Lest = require('@lest')
local describe, it, expect = Lest.describe, Lest.it, Lest.expect
local beforeEach, afterAll = Lest.beforeEach, Lest.afterAll

describe('database', function ()
	local db

	beforeEach(function ()
		db = openInMemory()
	end)

	afterAll(function ()
		db:close()
	end)

	it('starts empty', function ()
		expect(db:count()).toBe(0)
	end)
end)

return nil
```

`beforeEach` runs outermost-first and `afterEach` innermost-first, so setup
nests and teardown unwinds in the order you'd expect. Within one block the same
symmetry holds for the `All` hooks: multiple `beforeAll` hooks run in
registration order, and multiple `afterAll` hooks run in **reverse**
registration order — teardown is LIFO, mirroring setup.

A failing `beforeAll` fails every test in its block — there is no point running
them against setup that didn't happen — and Lest says so rather than reporting a
cascade of unrelated assertion failures. Descendant blocks of a failed
`beforeAll` don't run their own `beforeAll` either.

Same-named sibling `describe` blocks get independent hooks and state; blocks are
identified by position, not by label.

## Skipping a test

`Lest.xit` registers a test that is reported but never executed. The body is
optional, so you can park an intention before you've written one, and an
optional third argument gives the skip a reason that reporters surface:

```luau
local xit = Lest.xit

xit('handles unicode escapes')

xit('handles surrogate pairs', function ()
	-- kept for when the encoder lands
end, 'waiting on the encoder')
```

Skipped tests appear in the summary as `skipped` — with the reason, when one
was given (`○ handles surrogate pairs (skipped: waiting on the encoder)`) —
and never affect the exit code.

## Assertions

`expect(value)` returns an object of matcher functions. Matchers are plain
dot-calls, and negations are spelled `toNot*`:

```luau
expect(total).toBe(7)
expect(total).toNotBe(0)
expect(items).toContain('apple')
expect(function () parse('{') end).toThrow('unexpected end')
```

There is no `.not.toBe`: `not` is a Luau keyword, so Jest's spelling isn't
available. Every matcher returns the expectation, so they chain:

```luau
expect(name).toBeA('string').toMatch('^user_')
```

The full list is in **[Matchers](matchers.md)**.

## Async and yielding

Test bodies are called synchronously, so what "async" means depends entirely on
whether the backend running them has a scheduler.

The `native` backend has none — the embedded VM is a bare Luau VM, with no task
library and nothing to yield to. Per-test timeouts come from the VM's interrupt
callback, which is what stops a runaway loop.

Under `lune` and `lute`, specs run inside the real runtime, so its scheduler is
the one driving your test — `task.wait` and friends behave exactly as they do in
production. Timeouts are enforced by killing the process, since the harness runs
the whole suite in one.

The `cloud` backend runs each spec under the engine's real task scheduler with a
per-spec deadline derived from `timeout_ms`; a spec that exceeds it is reported
as a timeout failure rather than hanging the run.

Full task-library semantics exist only where a task library does. If a spec
needs one, give its suite a backend that has one — see [Backends](backends.md).

## File conventions

- Name specs `*.spec.luau` beside the code they test, or wherever your suite's
  `include` globs point.
- End with `return nil`. Spec files are modules; Lest collects tests through
  registration, not the return value.
- Use `--!strict`. Your specs require real `.luau` files, so the type checker
  and Luau-LSP have everything they need.
- The test registry is reset before each spec file runs on every backend, so no
  spec ever sees another's tests. How far isolation goes beyond that depends on
  the backend: `native` gives each spec file its own VM and `cloud` its own
  task, so module state is fresh per file. Under `lune` and `lute` the whole
  suite runs in one process over one module cache, so a helper module's state
  does persist across spec files — see [Backends](backends.md#lune--lute). Don't
  rely on a shared module resetting itself between specs.
