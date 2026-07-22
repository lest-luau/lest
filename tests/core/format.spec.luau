--!strict
local Lest = require('../../luau/core')
local format = require('../../luau/core/format')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

describe('format.value', function ()
	it('quotes strings', function ()
		expect(format.value('hi')).toBe('"hi"')
	end)

	it('renders scalars with tostring', function ()
		expect(format.value(42)).toBe('42')
		expect(format.value(true)).toBe('true')
		expect(format.value(nil)).toBe('nil')
	end)

	it('renders arrays in order', function ()
		expect(format.value({ 3, 1, 2 })).toBe('{ 3, 1, 2 }')
	end)

	it('renders maps with sorted keys', function ()
		expect(format.value({ b = 2, a = 1 })).toBe('{ a = 1, b = 2 }')
	end)

	it('brackets non-identifier keys', function ()
		expect(format.value({ ['not an id'] = 1 })).toBe('{ ["not an id"] = 1 }')
	end)

	it('marks cycles instead of recursing forever', function ()
		local t: { [string]: any } = {}
		t.self = t
		expect(format.value(t)).toBe('{ self = <cycle> }')
	end)

	it('caps depth', function ()
		local deep = { a = { b = { c = { d = { e = 1 } } } } }
		expect(format.value(deep)).toBe('{ a = { b = { c = { d = {...} } } } }')
	end)
end)

-- The snapshot serializer intentionally diverges from `format.value`: matcher
-- messages truncate for readability, snapshots must be lossless — a depth cap
-- there would mean a change below it could never fail a snapshot.
describe('format.snapshot', function ()
	it('serializes with no depth cap', function ()
		local deep = { a = { b = { c = { d = { e = { f = 1 } } } } } }
		expect(format.snapshot(deep)).toBe('{ a = { b = { c = { d = { e = { f = 1 } } } } } }')
	end)

	it('sorts keys deterministically regardless of insertion order', function ()
		local a: { [string]: number } = {}
		a.z = 1
		a.a = 2
		a.m = 3
		expect(format.snapshot(a)).toBe('{ a = 2, m = 3, z = 1 }')
	end)

	it('orders mixed scalar key types deterministically', function ()
		-- Keys sort by type name first (boolean < number < string), then value.
		local t: { [any]: number } = {}
		t.alpha = 3
		t[2.5] = 2
		t[true] = 1
		expect(format.snapshot(t)).toBe('{ [true] = 1, [2.5] = 2, alpha = 3 }')
	end)

	it('reports a cycle as an error instead of truncating it away', function ()
		local t: { [string]: any } = {}
		t.self = t
		local received, problem = format.snapshot(t)
		expect(received).toBeNil()
		expect(problem).toContain('must not contain cycles')
	end)

	it('reports non-scalar keys as an error instead of sorting by address', function ()
		local t: { [any]: any } = {}
		t[{}] = 1
		local received, problem = format.snapshot(t)
		expect(received).toBeNil()
		expect(problem).toContain('keys must be strings, numbers, or booleans')
	end)

	it('serializes a shared (non-cyclic) subtable in both positions', function ()
		local shared = { x = 1 }
		expect(format.snapshot({ left = shared, right = shared })).toBe('{ left = { x = 1 }, right = { x = 1 } }')
	end)

	it('renders reference values as stable placeholders, never addresses', function ()
		-- `tostring(print)` embeds a memory address, which would change every
		-- run; the serializer substitutes a stable type placeholder.
		expect(format.snapshot({ fn = print })).toBe('{ fn = <function> }')
	end)
end)

describe('format.deepEqual', function ()
	it('compares primitives', function ()
		expect(format.deepEqual(1, 1)).toBe(true)
		expect(format.deepEqual('a', 'b')).toBe(false)
		expect(format.deepEqual(nil, false)).toBe(false)
	end)

	it('compares nested tables structurally', function ()
		expect(format.deepEqual({ a = { 1, 2 } }, { a = { 1, 2 } })).toBe(true)
		expect(format.deepEqual({ a = { 1, 2 } }, { a = { 1, 3 } })).toBe(false)
	end)

	it('rejects missing and extra keys in either direction', function ()
		expect(format.deepEqual({ a = 1 }, { a = 1, b = 2 })).toBe(false)
		expect(format.deepEqual({ a = 1, b = 2 }, { a = 1 })).toBe(false)
	end)

	it('treats NaN as equal to NaN, but never to anything else', function ()
		expect(format.deepEqual(0 / 0, 0 / 0)).toBe(true)
		expect(format.deepEqual({ n = 0 / 0 }, { n = 0 / 0 })).toBe(true)
		expect(format.deepEqual(0 / 0, 1)).toBe(false)
		expect(format.deepEqual(1, 0 / 0)).toBe(false)
		expect(format.deepEqual(0 / 0, 'nan')).toBe(false)
	end)

	-- Without a seen-pair guard these overflow the stack, and the failure
	-- surfaces as an unrelated traceback even though `format.value` prints the
	-- same values happily.
	it('compares self-referential tables instead of recursing forever', function ()
		local a: { [string]: any } = { name = 'node' }
		a.self = a
		local b: { [string]: any } = { name = 'node' }
		b.self = b
		expect(format.deepEqual(a, b)).toBe(true)
	end)

	it('compares mutually referential tables', function ()
		local parentA: { [string]: any } = { label = 'parent' }
		local childA: { [string]: any } = { label = 'child', parent = parentA }
		parentA.child = childA

		local parentB: { [string]: any } = { label = 'parent' }
		local childB: { [string]: any } = { label = 'child', parent = parentB }
		parentB.child = childB

		expect(format.deepEqual(parentA, parentB)).toBe(true)
	end)

	it('still reports a difference reachable through a cycle', function ()
		local a: { [string]: any } = { label = 'a' }
		a.self = a
		local b: { [string]: any } = { label = 'b' }
		b.self = b
		expect(format.deepEqual(a, b)).toBe(false)
	end)
end)

return nil
