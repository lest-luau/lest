# lest/core

The Lest spec API and runner in pure, strict Luau with zero dependencies. It
runs unmodified anywhere a Luau VM exists: the CLI's embedded VM, Lune, Lute,
Open Cloud, and Studio.

The framework never prints — `Lest.run(emit)` streams protocol events (plain
JSON-safe tables) to whatever emitter the host supplies. Snapshot comparison
and every other decision live in the host; this package only reports facts.

```luau
--!strict
local Lest = require('path/to/lest')
local describe, it, expect = Lest.describe, Lest.it, Lest.expect

describe('math', function ()
	Lest.beforeEach(function ()
		-- runs before every test in this block
	end)

	it('adds', function ()
		expect(1 + 1).toBe(2)
	end)

	it('rejects impossible sums', function ()
		expect(function ()
			error('overflow')
		end).toThrow('overflow')
	end)
end)

return nil
```

Matchers are plain dot-call functions closing over the expected value:
`toBe` / `toNotBe`, `toEqual` / `toNotEqual`, `toBeTruthy` / `toBeFalsy`,
`toBeNil` / `toNotBeNil`, `toBeA`, `toContain`, `toHaveLength`,
`toBeGreaterThan[OrEqual]`, `toBeLessThan[OrEqual]`, `toBeCloseTo`, `toMatch`,
`toThrow` / `toNotThrow`, `toMatchSnapshot`. Lifecycle hooks: `beforeAll`,
`beforeEach`, `afterEach`, `afterAll` — plus `xit`, which registers a test that
is reported but never executed.

This is **not** a package you install. `build.rs` compiles these sources into
the `lest` binary, which writes them to `.lest/core` on demand — so the runner
and the framework are always the same version. A module slot is reserved for the
property engine, post-1.0.

User-facing documentation lives in [`docs/`](../../docs); see
[Writing tests](../../docs/writing-tests.md) and [Matchers](../../docs/matchers.md).
