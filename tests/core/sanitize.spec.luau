--!strict
-- The cloud sanitizer is pure Luau, so the native suite exercises it directly —
-- no Open Cloud round-trip required (the seam scheduler.luau advertises: the
-- runtime plumbing stays unit-testable under plain-Luau tooling).
local Lest = require('../../luau/core')
local sanitize = require('../../luau/runtime/cloud/sanitize')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

local REPLACEMENT = utf8.char(0xFFFD)

describe('sanitize.value', function ()
	it('passes scalars through', function ()
		expect(sanitize.value('hi')).toBe('hi')
		expect(sanitize.value(true)).toBe(true)
		expect(sanitize.value(false)).toBe(false)
		expect(sanitize.value(42)).toBe(42)
		expect(sanitize.value(-1.5)).toBe(-1.5)
	end)

	it('coerces non-finite numbers to 0', function ()
		-- `0` on both wire paths (here and the harness), so backends agree on
		-- the shape of the same pathological event.
		expect(sanitize.value(0 / 0)).toBe(0)
		expect(sanitize.value(math.huge)).toBe(0)
		expect(sanitize.value(-math.huge)).toBe(0)
	end)

	it('coerces non-finite values inside tables', function ()
		expect(sanitize.value({ nan = 0 / 0, up = math.huge, down = -math.huge, ok = 1 })).toEqual({
			nan = 0,
			up = 0,
			down = 0,
			ok = 1,
		})
	end)

	it('keeps a contiguous array an array', function ()
		-- Protocol `path` fields depend on arrays surviving as arrays.
		expect(sanitize.value({ 1, 2, 3 })).toEqual({ 1, 2, 3 })
		expect(sanitize.value({ 'a', 'b' })).toEqual({ 'a', 'b' })
	end)

	it('copies deep nesting without truncation', function ()
		local deep = { a = { b = { c = { d = { e = { f = { g = 'leaf' } } } } } } }
		expect(sanitize.value(deep)).toEqual(deep)
	end)

	it('collapses beyond the depth ceiling to a string rather than recursing forever', function ()
		local root: { [string]: any } = {}
		local node = root
		for _ = 1, 20 do
			local child: { [string]: any } = {}
			node.child = child
			node = child
		end
		node.leaf = 1

		-- MAX_DEPTH is 16: the copy stays structural down to depth 15 and the
		-- table at depth 16 becomes a string.
		local cursor: any = sanitize.value(root)
		for _ = 1, 16 do
			expect(cursor).toBeA('table')
			cursor = cursor.child
		end
		expect(cursor).toBeA('string')
	end)

	it('collapses cycles to a marker instead of failing the encode', function ()
		local t: { [string]: any } = { label = 'node' }
		t.self = t
		expect(sanitize.value(t)).toEqual({ label = 'node', self = '<cycle>' })
	end)

	it('copies a shared (non-cyclic) subtable in both positions', function ()
		local shared = { x = 1 }
		expect(sanitize.value({ left = shared, right = shared })).toEqual({
			left = { x = 1 },
			right = { x = 1 },
		})
	end)

	it('stringifies every key of a mixed table', function ()
		-- A JSON object holds only string keys; keeping the array part's keys
		-- as numbers would hand the platform encoder a table it cannot
		-- represent as one object.
		local mixed: { [any]: any } = { 1, 2 }
		mixed.foo = 'bar'
		expect(sanitize.value(mixed)).toEqual({ ['1'] = 1, ['2'] = 2, foo = 'bar' })
	end)

	it('stringifies the keys of a sparse or non-contiguous array', function ()
		local sparse: { [any]: any } = {}
		sparse[1] = 'a'
		sparse[3] = 'c'
		expect(sanitize.value(sparse)).toEqual({ ['1'] = 'a', ['3'] = 'c' })
	end)

	it('stringifies non-finite number keys', function ()
		-- A NaN key cannot exist in a Luau table (indexing with NaN raises), so
		-- the infinities are the pathological number keys left to guard.
		local edges: { [any]: any } = {}
		edges[math.huge] = 'up'
		edges[-math.huge] = 'down'
		expect(sanitize.value(edges)).toEqual({ inf = 'up', ['-inf'] = 'down' })
	end)

	it('suffixes stringified keys that collide instead of overwriting', function ()
		local colliding: { [any]: any } = {}
		colliding[1] = 'number key'
		colliding['1'] = 'string key'
		local out = sanitize.value(colliding) :: { [string]: string }

		-- pairs order decides which key stringifies first, so assert the pair
		-- of slots rather than which value landed in which.
		local values = { out['1'], out['1 #2'] }
		table.sort(values)
		expect(values).toEqual({ 'number key', 'string key' })
	end)

	it('replaces invalid UTF-8 bytes with U+FFFD instead of losing the run', function ()
		expect(sanitize.value(string.char(0xFF))).toBe(REPLACEMENT)
		-- A truncated two-byte sequence: the bad lead byte degrades alone and
		-- the valid bytes around it survive untouched.
		expect(sanitize.value('ok' .. string.char(0xC3) .. '(')).toBe('ok' .. REPLACEMENT .. '(')
		-- A run of invalid bytes degrades byte by byte.
		expect(sanitize.value(string.char(0xFF, 0xFE))).toBe(REPLACEMENT .. REPLACEMENT)
	end)

	it('passes valid multi-byte UTF-8 through untouched', function ()
		local text = 'héllo ' .. utf8.char(0x1F600)
		expect(sanitize.value(text)).toBe(text)
	end)

	it('repairs string keys as well as string values', function ()
		local bad: { [any]: any } = {}
		bad['k' .. string.char(0xFF)] = 1
		bad.other = 2
		local out = sanitize.value(bad) :: { [string]: number }
		expect(out['k' .. REPLACEMENT]).toBe(1)
		expect(out.other).toBe(2)
	end)

	it('replaces functions and threads with strings instead of dropping them', function ()
		local out = sanitize.value({
			fn = print,
			thread = coroutine.create(function () end),
		}) :: { [string]: any }
		expect(out.fn).toBeA('string')
		expect(out.thread).toBeA('string')
	end)
end)

return nil
