--!strict
local Lest = require('../../luau/core')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

--- Runs a failing matcher and hands back the structured assertion it raised.
local function capture (fn: () -> ()): any
	local ok, err = pcall(fn)
	expect(ok).toBe(false)
	expect(type(err)).toBe('table')
	expect((err :: any).__lestAssertion).toBe(true)
	return err
end

describe('expect', function ()
	describe('toBe', function ()
		it('passes on identical primitives', function ()
			expect(1).toBe(1)
			expect('a').toBe('a')
			expect(nil).toBe(nil)
		end)

		it('compares tables by reference', function ()
			local t = {}
			expect(t).toBe(t)
			expect(function ()
				expect({}).toBe({})
			end).toThrow('to be')
		end)

		it('raises a structured failure with expected and received', function ()
			local err = capture(function ()
				expect(1).toBe(2)
			end)
			expect((err :: any).expected).toBe('2')
			expect((err :: any).received).toBe('1')
		end)
	end)

	describe('toNotBe', function ()
		it('passes on different values', function ()
			expect(1).toNotBe(2)
			expect({ 1 }).toNotBe({ 1 })
		end)

		it('fails on identical values', function ()
			expect(function ()
				expect(1).toNotBe(1)
			end).toThrow('to not be')
		end)
	end)

	describe('toEqual', function ()
		it('compares tables structurally', function ()
			expect({ a = { 1, 2 } }).toEqual({ a = { 1, 2 } })
		end)

		it('fails on structural differences', function ()
			expect(function ()
				expect({ a = 1 }).toEqual({ a = 2 })
			end).toThrow('to equal')
		end)

		it('treats NaN as equal to NaN (Jest parity; toBe keeps plain ==)', function ()
			local nan = 0 / 0
			expect(nan).toEqual(nan)
			expect({ n = 0 / 0 }).toEqual({ n = 0 / 0 })
			-- toBe deliberately stays `==` semantics, where NaN ~= NaN.
			expect(function ()
				expect(0 / 0).toBe(0 / 0)
			end).toThrow('to be')
		end)

		it('treats -0 as equal to 0 (plain == short-circuits the deep walk)', function ()
			expect(-0).toEqual(0)
			expect({ n = -0 }).toEqual({ n = 0 })
		end)

		it('compares mixed array/dict tables structurally', function ()
			-- Built imperatively: a mixed table *literal* trips selene's
			-- mixed_table lint, and rightly so in ordinary code.
			local function mixed (foo: string?): { [any]: any }
				local t: { [any]: any } = { 1, 2 }
				t.foo = foo
				return t
			end
			expect(mixed('bar')).toEqual(mixed('bar'))
			expect(function ()
				expect(mixed('bar')).toEqual(mixed('baz'))
			end).toThrow('to equal')
			expect(function ()
				expect(mixed('bar')).toEqual(mixed(nil))
			end).toThrow('to equal')
		end)
	end)

	describe('toNotEqual', function ()
		it('passes on structural differences', function ()
			expect({ a = 1 }).toNotEqual({ a = 2 })
		end)

		it('fails on deep-equal tables', function ()
			expect(function ()
				expect({ a = 1 }).toNotEqual({ a = 1 })
			end).toThrow('to not equal')
		end)
	end)

	describe('toBeTruthy and toBeFalsy', function ()
		it('treats only false and nil as falsy', function ()
			expect(0).toBeTruthy()
			expect('').toBeTruthy()
			expect({}).toBeTruthy()
			expect(false).toBeFalsy()
			expect(nil).toBeFalsy()
		end)

		it('fails in the other direction', function ()
			expect(function ()
				expect(false).toBeTruthy()
			end).toThrow('to be truthy')
			expect(function ()
				expect(1).toBeFalsy()
			end).toThrow('to be falsy')
		end)
	end)

	describe('toBeNil and toNotBeNil', function ()
		it('distinguishes nil from everything else', function ()
			expect(nil).toBeNil()
			expect(false).toNotBeNil()
			expect(0).toNotBeNil()
		end)

		it('fails in the other direction', function ()
			expect(function ()
				expect(1).toBeNil()
			end).toThrow('to be nil')
			expect(function ()
				expect(nil).toNotBeNil()
			end).toThrow('to not be nil')
		end)
	end)

	describe('toBeA', function ()
		it('checks the typeof a value', function ()
			expect(1).toBeA('number')
			expect('x').toBeA('string')
			expect({}).toBeA('table')
			expect(print).toBeA('function')
		end)

		it('reports the actual type on failure', function ()
			local err = capture(function ()
				expect(1).toBeA('string')
			end)
			expect((err :: any).received).toBe('number')
		end)
	end)

	describe('toContain', function ()
		it('finds plain substrings in strings', function ()
			expect('hello world').toContain('lo wo')
			expect(function ()
				expect('hello').toContain('bye')
			end).toThrow('to contain')
		end)

		it('finds deep-equal elements in arrays', function ()
			expect({ 1, 2, 3 }).toContain(2)
			expect({ { id = 1 }, { id = 2 } }).toContain({ id = 2 })
			expect(function ()
				expect({ 1, 2 }).toContain(3)
			end).toThrow('to contain')
		end)

		it('walks only the array part of a table (dict entries are invisible)', function ()
			-- `toContain` iterates via ipairs, so a dict-shaped table contains
			-- nothing: values stored under string keys are never visited. Use
			-- `toEqual` on the whole table, or read the key directly, instead.
			expect(function ()
				expect({ a = 1, b = 2 }).toContain(1)
			end).toThrow('to contain')
			-- The array part of a mixed table is still searched; the dict part
			-- of the same table is not.
			local mixed: { [any]: any } = { 1, 2 }
			mixed.extra = 3
			expect(mixed).toContain(2)
			expect(function ()
				expect(mixed).toContain(3)
			end).toThrow('to contain')
		end)
	end)

	describe('toHaveLength', function ()
		it('measures strings and arrays', function ()
			expect('four').toHaveLength(4)
			expect({ 1, 2, 3 }).toHaveLength(3)
			expect({}).toHaveLength(0)
		end)

		it('reports the actual length on failure', function ()
			local err = capture(function ()
				expect({ 1 }).toHaveLength(5)
			end)
			expect((err :: any).received).toBe('1')
		end)
	end)

	describe('comparisons', function ()
		it('orders numbers', function ()
			expect(2).toBeGreaterThan(1)
			expect(2).toBeGreaterThanOrEqual(2)
			expect(1).toBeLessThan(2)
			expect(2).toBeLessThanOrEqual(2)
		end)

		it('orders strings lexically', function ()
			expect('b').toBeGreaterThan('a')
		end)

		it('rejects mixed types', function ()
			expect(function ()
				expect(1).toBeGreaterThan('a')
			end).toThrow('requires two numbers or two strings')
		end)

		it('fails the failing direction of the less-than family with the exact message', function ()
			local err = capture(function ()
				expect(2).toBeLessThan(2)
			end)
			expect((err :: any).message).toBe('expected 2 toBeLessThan 2')
			err = capture(function ()
				expect(3).toBeLessThanOrEqual(2)
			end)
			expect((err :: any).message).toBe('expected 3 toBeLessThanOrEqual 2')
		end)
	end)

	describe('toBeCloseTo', function ()
		it('tolerates float error at the default precision', function ()
			expect(0.1 + 0.2).toBeCloseTo(0.3)
		end)

		it('respects an explicit precision', function ()
			expect(1.23456).toBeCloseTo(1.2346, 4)
			expect(function ()
				expect(1.23).toBeCloseTo(1.3, 2)
			end).toThrow('to be close to')
		end)

		it('fails on NaN in either position rather than silently passing', function ()
			local nan = 0 / 0
			expect(function ()
				expect(nan).toBeCloseTo(5)
			end).toThrow('to be close to')
			expect(function ()
				expect(5).toBeCloseTo(nan)
			end).toThrow('to be close to')
		end)
	end)

	describe('toMatch', function ()
		it('matches Luau string patterns', function ()
			expect('lest-0.1.0').toMatch('%d+%.%d+%.%d+')
			expect(function ()
				expect('abc').toMatch('%d')
			end).toThrow('to match pattern')
		end)
	end)

	describe('toThrow', function ()
		it('passes when the function throws', function ()
			expect(function ()
				error('kapow')
			end).toThrow()
		end)

		it('matches the error message as a plain substring', function ()
			expect(function ()
				error('connection refused (code 61)')
			end).toThrow('(code 61)')
		end)

		it('fails when the message does not contain the substring', function ()
			local err = capture(function ()
				expect(function ()
					error('kapow')
				end).toThrow('boom')
			end)
			expect((err :: any).message).toBe('expected error message to contain "boom"')
		end)

		it('fails when the function does not throw', function ()
			local err = capture(function ()
				expect(function () end).toThrow()
			end)
			expect((err :: any).message).toBe('expected function to throw, but it did not')
		end)

		it("matches messages from lest's own assertions", function ()
			expect(function ()
				expect(1).toBe(2)
			end).toThrow('expected 1 to be 2')
		end)

		it('rejects non-function values', function ()
			expect(function ()
				expect(5).toThrow()
			end).toThrow('requires a function')
		end)
	end)

	-- Argument-side validation: a bad *expected* argument must raise a clear
	-- structured assertion, not a raw runtime error from library plumbing.
	describe('argument validation', function ()
		it('rejects a non-number comparison target in toBeCloseTo', function ()
			expect(function ()
				expect(1).toBeCloseTo('1' :: any)
			end).toThrow('requires a number to compare against')
		end)

		it('rejects a non-string pattern in toMatch', function ()
			expect(function ()
				expect('abc').toMatch(5 :: any)
			end).toThrow('requires a string pattern')
		end)

		it('rejects a non-string substring in toThrow', function ()
			expect(function ()
				expect(function ()
					error('boom')
				end).toThrow(5 :: any)
			end).toThrow('requires a string substring')
		end)
	end)

	describe('toNotThrow', function ()
		it('passes when the function completes', function ()
			expect(function () end).toNotThrow()
		end)

		it('fails and includes the original message when it throws', function ()
			expect(function ()
				expect(function ()
					error('kapow')
				end).toNotThrow()
			end).toThrow('kapow')
		end)
	end)

	it('chains', function ()
		expect(1).toBe(1).toNotBe(2).toBeTruthy()
	end)
end)

describe('skipping', function ()
	Lest.xit('is reported but never executed', function ()
		error('this must never run')
	end)
end)

return nil
