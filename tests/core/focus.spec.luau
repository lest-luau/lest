--!strict
-- Focus and skip modifiers, each scenario driven through `Lest.capture` so a
-- focused inner run never silences the suite running it.
local Lest = require('@lest')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

--- The `(kind, name)` pairs of every test outcome, in emission order.
local function outcomes (events: { { [string]: any } }): { { string } }
	local found = {}
	for _, event in events do
		local kind = event.kind
		if kind == 'test_pass' or kind == 'test_fail' or kind == 'test_skip' then
			table.insert(found, { kind :: string, event.name :: string })
		end
	end
	return found
end

local function eventNamed (events: { { [string]: any } }, name: string): { [string]: any }?
	for _, event in events do
		if event.name == name and event.kind == 'test_skip' then
			return event
		end
	end
	return nil
end

describe('it.only', function ()
	it('runs only focused tests and reports the rest as skipped', function ()
		local events = Lest.capture(function ()
			it('plain', function () end)
			it.only('focused', function () end)
			it('another plain', function () end)
		end)

		expect(outcomes(events)).toEqual({
			{ 'test_skip', 'plain' },
			{ 'test_pass', 'focused' },
			{ 'test_skip', 'another plain' },
		})
	end)

	it('marks excluded tests with focusExcluded and a reason', function ()
		local events = Lest.capture(function ()
			it('excluded', function () end)
			it.only('focused', function () end)
		end)

		local skip = eventNamed(events, 'excluded')
		expect(skip).toNotBeNil()
		expect((skip :: any).focusExcluded).toBe(true)
		expect((skip :: any).reason).toBe('skipped by .only')
	end)

	it('leaves focusExcluded off tests skipped on their own', function ()
		local events = Lest.capture(function ()
			it.skip('deliberately skipped', function () end)
			it.only('focused', function () end)
		end)

		local skip = eventNamed(events, 'deliberately skipped')
		expect(skip).toNotBeNil()
		-- Absent, not false: a nil field stays off the wire entirely.
		expect((skip :: any).focusExcluded).toBeNil()
	end)

	it('runs every test when nothing is focused', function ()
		local events = Lest.capture(function ()
			it('a', function () end)
			it('b', function () end)
		end)

		expect(outcomes(events)).toEqual({
			{ 'test_pass', 'a' },
			{ 'test_pass', 'b' },
		})
	end)
end)

describe('describe.only', function ()
	it('focuses every test in the block, including nested ones', function ()
		local events = Lest.capture(function ()
			it('outside', function () end)
			describe.only('focused block', function ()
				it('inside', function () end)
				describe('nested', function ()
					it('deep', function () end)
				end)
			end)
		end)

		expect(outcomes(events)).toEqual({
			{ 'test_skip', 'outside' },
			{ 'test_pass', 'inside' },
			{ 'test_pass', 'deep' },
		})
	end)

	it('does not leak focus to tests registered after the block', function ()
		local events = Lest.capture(function ()
			describe.only('focused block', function ()
				it('inside', function () end)
			end)
			it('after', function () end)
		end)

		expect(outcomes(events)).toEqual({
			{ 'test_pass', 'inside' },
			{ 'test_skip', 'after' },
		})
	end)

	it('skips the once-hooks of a fully focused-out block', function ()
		local ran = {}
		local events = Lest.capture(function ()
			describe('unfocused', function ()
				Lest.beforeAll(function ()
					table.insert(ran, 'beforeAll')
				end)
				Lest.afterAll(function ()
					table.insert(ran, 'afterAll')
				end)
				it('never runs', function () end)
			end)
			it.only('focused', function () end)
		end)

		expect(#outcomes(events)).toBe(2)
		expect(ran).toEqual({})
	end)
end)

describe('describe.skip', function ()
	it('skips every test in the block with a describe.skip reason', function ()
		local events = Lest.capture(function ()
			describe.skip('parked', function ()
				it('one', function () end)
			end)
			it('kept', function () end)
		end)

		expect(outcomes(events)).toEqual({
			{ 'test_skip', 'one' },
			{ 'test_pass', 'kept' },
		})
		expect((eventNamed(events, 'one') :: any).reason).toBe('describe.skip')
	end)

	it('keeps a skip inside a focused block skipped', function ()
		-- Skip wins over focus: parking a test must survive someone else
		-- focusing the block it lives in.
		local events = Lest.capture(function ()
			describe.only('focused', function ()
				it.skip('parked', function () end)
				it('runs', function () end)
			end)
		end)

		expect(outcomes(events)).toEqual({
			{ 'test_skip', 'parked' },
			{ 'test_pass', 'runs' },
		})
		expect((eventNamed(events, 'parked') :: any).focusExcluded).toBeNil()
	end)
end)

describe('it.todo', function ()
	it('registers a bodyless test skipped with the todo reason', function ()
		local events = Lest.capture(function ()
			it.todo('write this one')
		end)

		expect(outcomes(events)).toEqual({ { 'test_skip', 'write this one' } })
		expect((eventNamed(events, 'write this one') :: any).reason).toBe('todo')
	end)

	it('rejects a body', function ()
		expect(function ()
			Lest.capture(function ()
				(it.todo :: any)('has a body', function () end)
			end)
		end).toThrow('takes no body')
	end)
end)

describe('focus and the name filter', function ()
	it('filters first, then focuses within what remains', function ()
		local events = Lest.capture(function ()
			it.only('keep me', function () end)
			it('keep me too', function () end)
			it('dropped', function () end)
		end, { nameFilter = 'keep me' })

		expect(outcomes(events)).toEqual({
			{ 'test_pass', 'keep me' },
			{ 'test_skip', 'keep me too' },
		})
	end)

	it('runs the survivors normally when the filter excludes every focused test', function ()
		local events = Lest.capture(function ()
			it.only('focused but filtered out', function () end)
			it('survivor', function () end)
		end, { nameFilter = 'survivor' })

		expect(outcomes(events)).toEqual({ { 'test_pass', 'survivor' } })
	end)
end)

describe('modifier state does not leak', function ()
	it('captures unfocused from inside a focused block', function ()
		-- The wave's recurring bug source: `capture` must clear the enclosing
		-- focus/skip depths, or the inner scenario inherits them and silently
		-- skips its own tests. The nesting is deliberate — the inner capture
		-- runs while collecting a `describe.only` body, which is the only
		-- moment `focusDepth` is positive.
		local inner: { { [string]: any } }? = nil
		Lest.capture(function ()
			describe.only('outer focus', function ()
				inner = Lest.capture(function ()
					it('a', function () end)
					it('b', function () end)
				end)
				it('placeholder', function () end)
			end)
		end)

		expect(inner).toNotBeNil()
		expect(outcomes(inner :: any)).toEqual({
			{ 'test_pass', 'a' },
			{ 'test_pass', 'b' },
		})
	end)

	it('unwinds the depth when a focused block body throws', function ()
		local events = Lest.capture(function ()
			pcall(function ()
				describe.only('explodes', function ()
					error('boom')
				end)
			end)
			-- Registered after the throw: if the counter leaked, this becomes
			-- focused and every unfocused sibling is skipped.
			it('after the throw', function () end)
			it('sibling', function () end)
		end)

		expect(outcomes(events)).toEqual({
			{ 'test_pass', 'after the throw' },
			{ 'test_pass', 'sibling' },
		})
	end)

	it('refuses a duplicate name registered through a modifier', function ()
		expect(function ()
			Lest.capture(function ()
				it('same', function () end)
				it.only('same', function () end)
			end)
		end).toThrow('duplicate test name')
	end)
end)

describe('a focused test that cannot run', function ()
	it('does not silence the file when the focused test is bodyless', function ()
		-- Otherwise `it.only('x')` with the body forgotten reports a green run
		-- over zero executed tests.
		local events = Lest.capture(function ()
			-- Cast: the type deliberately requires a body, and this test
			-- exercises what happens when someone gets it wrong anyway.
			(it.only :: any)('body forgotten')
			it('still runs', function () end)
		end)

		expect(outcomes(events)).toEqual({
			{ 'test_skip', 'body forgotten' },
			{ 'test_pass', 'still runs' },
		})
	end)

	it('does not silence the file when the focused test is also skipped', function ()
		local events = Lest.capture(function ()
			describe.skip('parked', function ()
				it.only('left focused', function () end)
			end)
			it('still runs', function () end)
		end)

		expect(outcomes(events)).toEqual({
			{ 'test_skip', 'left focused' },
			{ 'test_pass', 'still runs' },
		})
	end)
end)

return nil
