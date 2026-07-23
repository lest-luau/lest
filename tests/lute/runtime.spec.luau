--!strict
-- This suite runs under `backend = "lute"`: a real spawned lute process with
-- the real @lute/* std. Nothing here is mocked — that is the point.
local Lest = require('@lest')
local fs = require('@lute/fs')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

describe('lute runtime', function ()
	it('has the real @lute/fs module', function ()
		expect(fs.listdir).toBeA('function')
		expect(fs.exists).toBeA('function')
	end)

	it('sees the project it was launched from', function ()
		-- The CLI runs the harness with the project root as cwd.
		expect(fs.exists('lest.toml')).toBeTruthy()
	end)

	it('runs plain Luau alongside runtime APIs', function ()
		expect(('lest'):upper()).toBe('LEST')
	end)
end)

return nil
