--!strict
-- End-to-end snapshot suite: unlike tests/core/snapshot.spec.luau, which
-- asserts on the *events* the framework emits, these tests take real
-- snapshots and rely on the CLI's store for pass/fail. The committed
-- __snapshots__/snapshot.spec.luau.snap is the other half of every test here —
-- a serializer or store regression shows up as a diff against it.
local Lest = require('@lest')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

describe('snapshot store', function ()
	it('stores scalars', function ()
		expect(42).toMatchSnapshot()
		expect(-1.5).toMatchSnapshot()
		expect(true).toMatchSnapshot()
		expect(nil).toMatchSnapshot()
	end)

	it('stores strings with quoting and escapes', function ()
		expect('plain').toMatchSnapshot()
		expect('has "quotes" and a\nnewline\tand tab').toMatchSnapshot()
		expect('').toMatchSnapshot()
	end)

	it('stores arrays and mixed tables', function ()
		expect({ 1, 2, 3 }).toMatchSnapshot()
		expect({}).toMatchSnapshot()
		expect({ list = { 'a', 'b' }, count = 2, ok = true }).toMatchSnapshot()
	end)

	it('stores keys sorted, not in insertion order', function ()
		local scrambled: { [string]: number } = {}
		scrambled.zebra = 1
		scrambled.apple = 2
		scrambled.mango = 3
		expect(scrambled).toMatchSnapshot()
	end)

	it('stores non-identifier and non-string keys in brackets', function ()
		expect({ ['not an identifier'] = 1, [10] = 'ten', [false] = 'off' }).toMatchSnapshot()
	end)

	it('stores deep nesting losslessly', function ()
		-- Deeper than the depth-4 cap of the matcher-message formatter; the
		-- stored snapshot must keep every level.
		expect({ a = { b = { c = { d = { e = { f = 'bottom' } } } } } }).toMatchSnapshot()
	end)

	it('stores reference values as stable placeholders', function ()
		-- Functions carry a memory address through `tostring`; the snapshot
		-- must hold the fixed `<function>` form or it would differ every run.
		expect({ callback = function () end }).toMatchSnapshot()
	end)

	it('stores a shared table appearing twice without a cycle error', function ()
		local shared = { tag = 'shared' }
		expect({ left = shared, right = shared }).toMatchSnapshot()
	end)

	it('keys repeated snapshots in one test distinctly', function ()
		-- Same value twice: only distinct counter-suffixed keys keep the
		-- second call from comparing against (or clobbering) the first.
		expect('same value').toMatchSnapshot()
		expect('same value').toMatchSnapshot()
	end)

	it('keys hinted snapshots by hint', function ()
		expect({ role = 'named' }).toMatchSnapshot('the named one')
	end)
end)

describe('snapshot store > nested suite path', function ()
	describe('inner', function ()
		it('joins the describe path into the key', function ()
			-- The stored key embeds the full `describe` chain, so moving this
			-- test between suites is a snapshot change, not a silent re-match.
			expect('keyed by path').toMatchSnapshot()
		end)
	end)
end)

return nil
