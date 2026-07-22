--!strict
-- Engine suite: exercises real Roblox engine APIs that no other backend has.
-- These run in an actual place via Open Cloud — Instances, services, and the
-- DataModel are genuine, never emulated. The native/lune/lute backends cannot
-- run this spec (there is no `Instance`, `game`, or `workspace` there); that is
-- the whole reason the cloud backend exists.

local Lest = require('../../luau/core')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

describe('Instance', function ()
	it('creates a Folder with the expected ClassName', function ()
		local folder = Instance.new('Folder')
		expect(folder.ClassName).toBe('Folder')
	end)

	it('names and re-reads an instance', function ()
		local folder = Instance.new('Folder')
		folder.Name = 'Greebled'
		expect(folder.Name).toBe('Greebled')
	end)

	it('parents one instance under another', function ()
		local parent = Instance.new('Folder')
		local child = Instance.new('Folder')
		child.Parent = parent
		expect(child.Parent).toBe(parent)
		expect(parent:FindFirstChild(child.Name)).toBe(child)
	end)

	it('reports IsA across the class hierarchy', function ()
		local folder = Instance.new('Folder')
		expect(folder:IsA('Folder')).toBe(true)
		expect(folder:IsA('Instance')).toBe(true)
		expect(folder:IsA('Part')).toBe(false)
	end)
end)

describe('require delegation', function ()
	-- The bundle's require shim owns string requires; anything else must reach
	-- the engine's own require. lest's test place is empty, so there is no
	-- ModuleScript to require for a positive proof — instead, require a Folder
	-- and check the *engine* rejects it in its own words. The bundler's
	-- 'unresolved require' error here would mean delegation never happened.
	it('hands Instance arguments to the engine require, not the bundler', function ()
		local dynamicRequire = require :: any
		local folder = Instance.new('Folder')
		local ok, err = pcall(function ()
			return dynamicRequire(folder)
		end)
		expect(ok).toBe(false)
		expect(string.find(tostring(err), 'lest cloud bundle', 1, true)).toBeNil()
	end)
end)

describe('DataModel', function ()
	it('resolves core services through game:GetService', function ()
		local workspaceService = game:GetService('Workspace')
		expect(workspaceService).toBe(workspace)
		expect(workspaceService:IsA('Workspace')).toBe(true)
	end)

	it('exposes ReplicatedStorage as a container', function ()
		local rs = game:GetService('ReplicatedStorage')
		local marker = Instance.new('Folder')
		marker.Name = 'LestMarker'
		marker.Parent = rs
		expect(rs:FindFirstChild('LestMarker')).toBe(marker)
		marker:Destroy()
		expect(rs:FindFirstChild('LestMarker')).toBeNil()
	end)
end)

return nil
