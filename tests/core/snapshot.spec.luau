--!strict
local Lest = require('../../luau/core')
local context = require('../../luau/core/context')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

--- Pulls the `snapshot` events out of a captured event stream.
local function snapshots (events: { { [string]: any } }): { { [string]: any } }
	local found = {}
	for _, event in events do
		if event.kind == 'snapshot' then
			table.insert(found, event)
		end
	end
	return found
end

--- Pulls the `test_fail` events out of a captured event stream.
local function failures (events: { { [string]: any } }): { { [string]: any } }
	local found = {}
	for _, event in events do
		if event.kind == 'test_fail' then
			table.insert(found, event)
		end
	end
	return found
end

describe('toMatchSnapshot', function ()
	it('emits a snapshot event carrying the serialized value', function ()
		local events = Lest.capture(function ()
			Lest.describe('outer', function ()
				Lest.describe('inner', function ()
					Lest.it('serializes', function ()
						expect({ b = 2, a = 1 }).toMatchSnapshot()
					end)
				end)
			end)
		end)

		local snaps = snapshots(events)
		expect(snaps).toHaveLength(1)
		expect(snaps[1].kind).toBe('snapshot')
		expect(snaps[1].name).toBe('serializes')
		expect(snaps[1].path).toEqual({ 'outer', 'inner' })
		-- Key = describe path + test name joined with ` > `, then the counter.
		expect(snaps[1].key).toBe('outer > inner > serializes 1')
		-- Received is a stable, sorted-key string from format.value.
		expect(snaps[1].received).toBe('{ a = 1, b = 2 }')
		expect(snaps[1].received).toBeA('string')
	end)

	it('gives multiple calls in one test distinct 1-based keys', function ()
		local events = Lest.capture(function ()
			Lest.describe('counting', function ()
				Lest.it('twice', function ()
					expect('first').toMatchSnapshot()
					expect('second').toMatchSnapshot()
				end)
			end)
		end)

		local snaps = snapshots(events)
		expect(snaps).toHaveLength(2)
		expect(snaps[1].key).toBe('counting > twice 1')
		expect(snaps[2].key).toBe('counting > twice 2')
		expect(snaps[1].received).toBe('"first"')
		expect(snaps[2].received).toBe('"second"')
	end)

	it('lets an explicit hint replace the counter suffix', function ()
		local events = Lest.capture(function ()
			Lest.it('hinted', function ()
				expect(42).toMatchSnapshot('the answer')
			end)
		end)

		local snaps = snapshots(events)
		expect(snaps).toHaveLength(1)
		expect(snaps[1].key).toBe('hinted the answer')
		expect(snaps[1].received).toBe('42')
	end)

	it('serializes deterministically regardless of insertion order', function ()
		local function grab (value: any): string
			local events = Lest.capture(function ()
				Lest.it('t', function ()
					expect(value).toMatchSnapshot()
				end)
			end)
			return snapshots(events)[1].received
		end

		local a: { [string]: any } = {}
		a.z = 1
		a.a = 2
		a.m = 3
		local b: { [string]: any } = {}
		b.a = 2
		b.m = 3
		b.z = 1
		expect(grab(a)).toBe(grab(b))
		expect(grab(a)).toBe('{ a = 2, m = 3, z = 1 }')
	end)

	it('serializes losslessly past the message formatter depth cap', function ()
		-- `format.value` truncates at depth 4 for matcher messages; a snapshot
		-- of the same value must keep every level, or a change below the cap
		-- could never fail the snapshot.
		local events = Lest.capture(function ()
			Lest.it('deep', function ()
				expect({ a = { b = { c = { d = { e = 1 } } } } }).toMatchSnapshot()
			end)
		end)

		local snaps = snapshots(events)
		expect(snaps).toHaveLength(1)
		expect(snaps[1].received).toBe('{ a = { b = { c = { d = { e = 1 } } } } }')
	end)

	it('fails the assertion on a cyclic value instead of snapshotting a marker', function ()
		local cyclic: { [string]: any } = {}
		cyclic.self = cyclic
		local events = Lest.capture(function ()
			Lest.it('cyclic', function ()
				expect(cyclic).toMatchSnapshot()
			end)
		end)

		expect(snapshots(events)).toHaveLength(0)
		local failed = failures(events)
		expect(failed).toHaveLength(1)
		expect(failed[1].failure.type).toBe('assertion')
		expect(failed[1].failure.message).toContain('must not contain cycles')
	end)

	it('fails the assertion on non-scalar table keys', function ()
		local badKeys: { [any]: any } = {}
		badKeys[{}] = 1
		local events = Lest.capture(function ()
			Lest.it('bad key', function ()
				expect(badKeys).toMatchSnapshot()
			end)
		end)

		expect(snapshots(events)).toHaveLength(0)
		local failed = failures(events)
		expect(failed).toHaveLength(1)
		expect(failed[1].failure.type).toBe('assertion')
		expect(failed[1].failure.message).toContain('keys must be strings, numbers, or booleans')
	end)

	it('keeps the enclosing test snapshottable across a nested capture', function ()
		local before = context.current()
		expect(before).toNotBeNil()

		Lest.capture(function ()
			Lest.it('inner', function ()
				expect('nested').toMatchSnapshot()
			end)
		end)

		-- The nested run clears the context after each of its own tests. Without
		-- `capture` restoring it, this test would be left contextless and every
		-- later `toMatchSnapshot` in it would fail as if no test were running.
		-- Asserted by identity rather than by taking a real snapshot here: this
		-- spec should not leave a stored entry behind just to prove a point.
		expect(context.current()).toBe(before)
	end)

	it('reports a failure when called outside a running test', function ()
		-- No test is executing from the matcher's point of view. The context is
		-- put back afterwards: leaving it cleared would strand every test
		-- declared after this one in the file, which is the same hazard
		-- `Lest.capture` had to be fixed for.
		local saved = context.current()
		context.clear()
		expect(function ()
			expect(1).toMatchSnapshot()
		end).toThrow('while a test is running')
		context.restore(saved)
	end)
end)

return nil
