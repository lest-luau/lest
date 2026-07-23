--!strict
-- This suite runs under `backend = "lune"`: a real spawned lune process with
-- the real @lune/* std, through the same harness seam as the lute backend.
local Lest = require('@lest')
local fs = require('@lune/fs')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

describe('lune runtime', function ()
	it('has the real @lune/fs module', function ()
		expect(fs.readFile).toBeA('function')
		expect(fs.isFile).toBeA('function')
	end)

	it('sees the project it was launched from', function ()
		-- The CLI runs the harness with the project root as cwd.
		expect(fs.isFile('lest.toml')).toBeTruthy()
	end)

	it('runs plain Luau alongside runtime APIs', function ()
		expect(('lest'):rep(2)).toBe('lestlest')
	end)
end)

return nil
