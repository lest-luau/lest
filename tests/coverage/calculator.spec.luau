--!strict
-- Native suite that produces a real coverage report. Run with:
--   lest run coverage --coverage
local Lest = require('@lest')
local Calculator = require('./calculator')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

describe('Calculator', function ()
	it('adds two numbers', function ()
		expect(Calculator.add(2, 3)).toBe(5)
	end)

	it('subtracts two numbers', function ()
		expect(Calculator.subtract(5, 2)).toBe(3)
	end)

	it('divides two numbers', function ()
		expect(Calculator.divide(10, 2)).toBe(5)
	end)
end)

return nil
