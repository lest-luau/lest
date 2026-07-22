--!strict
-- Every block here runs its scenario inside `Lest.capture`, so no assertion
-- depends on a sibling `describe` having run first. Asserting on module-level
-- state left behind by an earlier block would make any filtered run
-- (`lest -t ...`, `--changed`, a single suite) fail spuriously — lest has to
-- survive being pointed at its own tests.
local Lest = require('../../luau/core')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect
local beforeAll, beforeEach, afterEach, afterAll = Lest.beforeAll, Lest.beforeEach, Lest.afterEach, Lest.afterAll

describe('lifecycle hooks', function ()
	describe('observed order', function ()
		it('ran beforeAll once, each-hooks around every test, afterAll on exit', function ()
			local log: { string } = {}
			Lest.capture(function ()
				describe('ordered block', function ()
					beforeAll(function ()
						table.insert(log, 'beforeAll')
					end)
					beforeEach(function ()
						table.insert(log, 'outer beforeEach')
					end)
					afterEach(function ()
						table.insert(log, 'outer afterEach')
					end)
					-- Fires when the runner leaves the block, which happens
					-- before `capture` returns.
					afterAll(function ()
						table.insert(log, 'afterAll')
					end)

					it('first test', function ()
						table.insert(log, 'test one')
					end)

					describe('inner', function ()
						beforeEach(function ()
							table.insert(log, 'inner beforeEach')
						end)
						afterEach(function ()
							table.insert(log, 'inner afterEach')
						end)

						it('second test', function ()
							table.insert(log, 'test two')
						end)
					end)
				end)
			end)

			expect(log).toEqual({
				'beforeAll',
				'outer beforeEach',
				'test one',
				'outer afterEach',
				'outer beforeEach',
				'inner beforeEach',
				'test two',
				'inner afterEach',
				'outer afterEach',
				'afterAll',
			})
		end)
	end)

	describe('state isolation', function ()
		it('runs beforeEach afresh for every test in the block', function ()
			local observed: { number } = {}
			local counter = 0
			Lest.capture(function ()
				describe('counting', function ()
					beforeEach(function ()
						counter += 1
					end)

					it('sees the hook-prepared state', function ()
						table.insert(observed, counter)
					end)

					it('gets a fresh increment per test', function ()
						table.insert(observed, counter)
					end)
				end)
			end)

			expect(observed).toEqual({ 1, 2 })
		end)
	end)

	-- Within one scope, setup runs in registration order and teardown runs in
	-- *reverse* registration order — LIFO, for afterEach and afterAll alike.
	-- The two teardown kinds agreeing is the point: they used to diverge.
	describe('same-scope ordering', function ()
		it('runs setup forward and teardown in reverse registration order', function ()
			local log: { string } = {}
			Lest.capture(function ()
				describe('lifo', function ()
					beforeAll(function ()
						table.insert(log, 'beforeAll 1')
					end)
					beforeAll(function ()
						table.insert(log, 'beforeAll 2')
					end)
					beforeEach(function ()
						table.insert(log, 'beforeEach 1')
					end)
					beforeEach(function ()
						table.insert(log, 'beforeEach 2')
					end)
					afterEach(function ()
						table.insert(log, 'afterEach 1')
					end)
					afterEach(function ()
						table.insert(log, 'afterEach 2')
					end)
					afterAll(function ()
						table.insert(log, 'afterAll 1')
					end)
					afterAll(function ()
						table.insert(log, 'afterAll 2')
					end)

					it('t', function ()
						table.insert(log, 'test')
					end)
				end)
			end)

			expect(log).toEqual({
				'beforeAll 1',
				'beforeAll 2',
				'beforeEach 1',
				'beforeEach 2',
				'test',
				'afterEach 2',
				'afterEach 1',
				'afterAll 2',
				'afterAll 1',
			})
		end)

		it('runs top-level afterAll hooks in reverse registration order too', function ()
			local log: { string } = {}
			Lest.capture(function ()
				afterAll(function ()
					table.insert(log, 'root afterAll 1')
				end)
				afterAll(function ()
					table.insert(log, 'root afterAll 2')
				end)
				it('t', function ()
					table.insert(log, 'test')
				end)
			end)

			expect(log).toEqual({ 'test', 'root afterAll 2', 'root afterAll 1' })
		end)
	end)

	-- Each-hooks are ordered by nesting depth, not registration order: an
	-- outer `beforeEach` declared textually *after* a nested describe must
	-- still run before the inner one.
	describe('declaration-order independence', function ()
		it('runs an outer beforeEach first even when declared after the child', function ()
			local trail: { string } = {}
			Lest.capture(function ()
				describe('parent', function ()
					describe('child', function ()
						beforeEach(function ()
							table.insert(trail, 'inner beforeEach')
						end)
						it('runs', function ()
							table.insert(trail, 'test')
						end)
					end)
					-- Declared after the nested describe on purpose.
					beforeEach(function ()
						table.insert(trail, 'outer beforeEach')
					end)
				end)
			end)

			expect(trail).toEqual({ 'outer beforeEach', 'inner beforeEach', 'test' })
		end)
	end)
end)

return nil
