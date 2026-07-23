--!strict
-- Runner/scope-machinery regressions, each exercised in isolation with
-- `Lest.capture` so an intentional beforeAll failure never leaks into the
-- aggregate suite.
local Lest = require('@lest')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect
local beforeAll, beforeEach, afterEach, afterAll = Lest.beforeAll, Lest.beforeEach, Lest.afterEach, Lest.afterAll
local xit = Lest.xit

local function countKind (events: { { [string]: any } }, kind: string): number
	local total = 0
	for _, event in events do
		if event.kind == kind then
			total += 1
		end
	end
	return total
end

--- Every event of one kind, in emission order.
local function ofKind (events: { { [string]: any } }, kind: string): { { [string]: any } }
	local found = {}
	for _, event in events do
		if event.kind == kind then
			table.insert(found, event)
		end
	end
	return found
end

--- The `name` of every event of one kind, in emission order.
local function namesOfKind (events: { { [string]: any } }, kind: string): { string }
	local names = {}
	for _, event in ofKind(events, kind) do
		table.insert(names, event.name)
	end
	return names
end

describe('runner scope machinery', function ()
	it('gives same-named sibling describes independent hooks and state', function ()
		-- The *test* names differ ('first'/'second') because a duplicate full
		-- name is now a collection error; the identically-named describes are
		-- what this test is about.
		local log: { string } = {}
		Lest.capture(function ()
			describe('A', function ()
				beforeAll(function ()
					table.insert(log, 'A1-all')
				end)
				beforeEach(function ()
					table.insert(log, 'A1-each')
				end)
				it('first', function ()
					table.insert(log, 'A1-test')
				end)
			end)
			describe('A', function ()
				beforeAll(function ()
					table.insert(log, 'A2-all')
				end)
				beforeEach(function ()
					table.insert(log, 'A2-each')
				end)
				it('second', function ()
					table.insert(log, 'A2-test')
				end)
			end)
		end)

		-- Each block's beforeAll runs exactly once and its each-hook fires only
		-- for its own test — no bleed across the identically-named siblings.
		expect(log).toEqual({
			'A1-all',
			'A1-each',
			'A1-test',
			'A2-all',
			'A2-each',
			'A2-test',
		})
	end)

	it('skips beforeAll for a describe whose tests are all skipped', function ()
		local log: { string } = {}
		Lest.capture(function ()
			describe('all skipped', function ()
				beforeAll(function ()
					table.insert(log, 'ran')
				end)
				xit('a', function () end)
				xit('b', function () end)
			end)
		end)

		expect(log).toEqual({})
	end)

	it('still runs beforeAll once when a describe has any executable test', function ()
		local log: { string } = {}
		Lest.capture(function ()
			describe('mixed', function ()
				beforeAll(function ()
					table.insert(log, 'ran')
				end)
				xit('skipped', function () end)
				it('runs', function ()
					table.insert(log, 'test')
				end)
			end)
		end)

		expect(log).toEqual({ 'ran', 'test' })
	end)

	it('does not run a child beforeAll after an ancestor beforeAll failed', function ()
		local log: { string } = {}
		local events = Lest.capture(function ()
			describe('outer', function ()
				beforeAll(function ()
					error('outer setup boom')
				end)
				beforeEach(function ()
					table.insert(log, 'outer-each')
				end)
				describe('inner', function ()
					beforeAll(function ()
						table.insert(log, 'inner-all')
					end)
					it('t', function ()
						table.insert(log, 'inner-test')
					end)
				end)
			end)
		end)

		-- The child beforeAll, the each-hook, and the test body all stay
		-- unexecuted; the test is still reported, failed with the ancestor's
		-- error.
		expect(log).toEqual({})
		expect(countKind(events, 'test_fail')).toBe(1)
	end)
end)

-- Collection-time guards: mistakes in how a spec *registers* tests fail loudly
-- at load, never silently at run time.
describe('runner registration guards', function ()
	it('rejects a duplicate test name in the same file at collection time', function ()
		expect(function ()
			Lest.capture(function ()
				describe('dupes', function ()
					it('same', function () end)
					it('same', function () end)
				end)
			end)
		end).toThrow("duplicate test name: 'dupes same'")
	end)

	it('rejects duplicates even across same-named sibling describes', function ()
		-- Snapshot keys and `-t` filtering see only the human-readable full
		-- name, so two same-named describes cannot both hold a test with one
		-- name — scope ids keep their hooks apart, not their identities.
		expect(function ()
			Lest.capture(function ()
				describe('A', function ()
					it('t', function () end)
				end)
				describe('A', function ()
					it('t', function () end)
				end)
			end)
		end).toThrow("duplicate test name: 'A t'")
	end)

	it('counts skipped tests when checking for duplicates', function ()
		expect(function ()
			Lest.capture(function ()
				xit('same name', function () end)
				it('same name', function () end)
			end)
		end).toThrow("duplicate test name: 'same name'")
	end)

	it('reports the real assertion message from an expect() at describe scope', function ()
		-- The collection error used to `tostring` the structured assertion
		-- table, yielding `table: 0x...`; it must unwrap the message instead.
		expect(function ()
			Lest.capture(function ()
				describe('collect', function ()
					expect(1).toBe(2)
				end)
			end)
		end).toThrow("error while collecting describe('collect'): expected 1 to be 2")
	end)

	it('errors when it() is called from inside a running test', function ()
		local events = Lest.capture(function ()
			it('registers mid-run', function ()
				it('too late', function () end)
			end)
		end)

		local failures = ofKind(events, 'test_fail')
		expect(failures).toHaveLength(1)
		expect(failures[1].failure.message).toContain('while the run is in progress')
	end)

	it('errors when a hook is registered from inside a running test', function ()
		-- A mid-run beforeEach used to silently attach at root and fire for
		-- every later test; now the registering test fails and the later test
		-- runs untouched.
		local events = Lest.capture(function ()
			it('attaches a hook mid-run', function ()
				beforeEach(function ()
					error('must never attach')
				end)
			end)
			it('later test', function () end)
		end)

		local failures = ofKind(events, 'test_fail')
		expect(failures).toHaveLength(1)
		expect(failures[1].name).toBe('attaches a hook mid-run')
		expect(failures[1].failure.message).toContain('while the run is in progress')
		expect(countKind(events, 'test_pass')).toBe(1)
	end)

	it('errors when describe() is called from inside a running test', function ()
		local events = Lest.capture(function ()
			it('nests mid-run', function ()
				describe('too late', function () end)
			end)
		end)

		local failures = ofKind(events, 'test_fail')
		expect(failures).toHaveLength(1)
		expect(failures[1].failure.message).toContain('describe(...) cannot be called while the run is in progress')
	end)
end)

describe('runner skip reporting', function ()
	it('carries an optional xit reason on the test_skip event', function ()
		local events = Lest.capture(function ()
			xit('not yet', nil, 'blocked on backend support')
			xit('no reason given', function () end)
		end)

		local skips = ofKind(events, 'test_skip')
		expect(skips).toHaveLength(2)
		expect(skips[1].name).toBe('not yet')
		expect(skips[1].reason).toBe('blocked on backend support')
		expect(skips[2].reason).toBeNil()
	end)
end)

-- Teardown is the part of the runner most easily broken without any existing
-- spec noticing: an inverted guard here turns failing tests green.
describe('runner teardown', function ()
	it('runs afterEach even when the test body threw', function ()
		local log: { string } = {}
		local events = Lest.capture(function ()
			describe('teardown', function ()
				afterEach(function ()
					table.insert(log, 'afterEach')
				end)
				it('boom', function ()
					error('body boom')
				end)
			end)
		end)

		expect(log).toEqual({ 'afterEach' })
		local failures = ofKind(events, 'test_fail')
		expect(failures).toHaveLength(1)
		expect(failures[1].failure.message).toContain('body boom')
	end)

	it('keeps the body failure when afterEach fails too', function ()
		local events = Lest.capture(function ()
			describe('both fail', function ()
				afterEach(function ()
					error('teardown boom')
				end)
				it('boom', function ()
					error('body boom')
				end)
			end)
		end)

		-- One failure, and it is the body's — the teardown error is suppressed
		-- so the report names the cause rather than the consequence.
		local failures = ofKind(events, 'test_fail')
		expect(failures).toHaveLength(1)
		expect(failures[1].failure.message).toContain('body boom')
		expect(countKind(events, 'test_pass')).toBe(0)
	end)

	it('surfaces an afterEach failure when the test itself passed', function ()
		local events = Lest.capture(function ()
			describe('teardown only', function ()
				afterEach(function ()
					error('teardown boom')
				end)
				it('passes', function () end)
			end)
		end)

		local failures = ofKind(events, 'test_fail')
		expect(failures).toHaveLength(1)
		expect(failures[1].name).toBe('passes')
		expect(failures[1].failure.message).toContain('teardown boom')
		expect(countKind(events, 'test_pass')).toBe(0)
	end)

	it('fails a test whose thrown value cannot even be described', function ()
		-- `toFailure` calls `tostring` on the thrown value from inside the
		-- xpcall handler, so an erroring `__tostring` takes the handler down
		-- with it. The outcome has to come from xpcall's success flag; reading
		-- it off "did the handler record a failure" would score this a pass.
		local undescribable = setmetatable({}, {
			__tostring = function ()
				error('tostring boom')
			end,
		})
		local events = Lest.capture(function ()
			it('throws an undescribable value', function ()
				error(undescribable)
			end)
		end)

		expect(countKind(events, 'test_fail')).toBe(1)
		expect(countKind(events, 'test_pass')).toBe(0)
	end)

	it('reports a failing afterAll as a synthetic (afterAll) test', function ()
		local events = Lest.capture(function ()
			describe('closing', function ()
				afterAll(function ()
					error('afterAll boom')
				end)
				it('t', function () end)
			end)
		end)

		expect(countKind(events, 'test_pass')).toBe(1)
		local failures = ofKind(events, 'test_fail')
		expect(failures).toHaveLength(1)
		expect(failures[1].name).toBe('(afterAll)')
		expect(failures[1].path).toEqual({ 'closing' })
		expect(failures[1].failure.message).toContain('afterAll boom')
	end)

	it('reports a failing top-level afterAll against the root path', function ()
		-- The root-scope branch: hooks registered outside any describe wrap the
		-- whole run and are never visited by named-scope entry.
		local events = Lest.capture(function ()
			afterAll(function ()
				error('root afterAll boom')
			end)
			it('t', function () end)
		end)

		expect(countKind(events, 'test_pass')).toBe(1)
		local failures = ofKind(events, 'test_fail')
		expect(failures).toHaveLength(1)
		expect(failures[1].name).toBe('(afterAll)')
		expect(failures[1].path).toEqual({})
		expect(failures[1].failure.message).toContain('root afterAll boom')
	end)
end)

-- `nameFilter` is what `-t` compiles down to. The full name is the describe
-- path joined with the test name by single spaces — no reporter decoration.
describe('runner name filter', function ()
	local function register ()
		describe('greek', function ()
			it('alpha', function () end)
			it('beta', function () end)
			it('gamma beta', function () end)
		end)
		it('rootless', function () end)
	end

	it('keeps only tests whose full name contains the filter', function ()
		local events = Lest.capture(register, { nameFilter = 'beta' })
		expect(namesOfKind(events, 'test_start')).toEqual({ 'beta', 'gamma beta' })
	end)

	it('matches against the describe path joined with spaces', function ()
		local events = Lest.capture(register, { nameFilter = 'greek alpha' })
		expect(namesOfKind(events, 'test_start')).toEqual({ 'alpha' })
	end)

	it('treats the filter as a plain substring, not a pattern', function ()
		local events = Lest.capture(register, { nameFilter = 'g.mma' })
		expect(namesOfKind(events, 'test_start')).toEqual({})
	end)

	it('runs everything when no filter is given', function ()
		local events = Lest.capture(register)
		expect(namesOfKind(events, 'test_start')).toEqual({ 'alpha', 'beta', 'gamma beta', 'rootless' })
	end)
end)

return nil
