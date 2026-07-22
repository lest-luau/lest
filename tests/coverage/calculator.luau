--!strict
-- Real source under test for the `coverage` suite, so `lest --coverage` has
-- something to instrument (the framework's own source is excluded from
-- coverage). The spec exercises add/subtract/divide but deliberately never
-- triggers the divide-by-zero branch, so the report shows a realistic < 100%.
local Calculator = {}

function Calculator.add (a: number, b: number): number
	return a + b
end

function Calculator.subtract (a: number, b: number): number
	return a - b
end

function Calculator.divide (a: number, b: number): number
	if b == 0 then
		-- Intentionally left uncovered by the spec.
		error('cannot divide by zero')
	end
	return a / b
end

return Calculator
