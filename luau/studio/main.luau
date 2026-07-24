--!strict
-- lest Studio companion plugin — the entry script. `lest studio install`
-- writes this (with the placeholder lines below substituted) into the local
-- Plugins folder as `lest.rbxmx`; hand edits there are overwritten by the
-- next install.
--
-- Engine globals live only in this file, which is why it is excluded in
-- selene.toml (the same accommodation the engine specs get). Every decision —
-- polling, backoff, warnings, job handling, the run state machine — lives in
-- the `bridge` module next door, which is pure Luau and tested by lest's own
-- native suite (tests/studio). This file only wires the real engine into the
-- bridge's `Deps` and loops.
--
-- The three placeholder lines carry working defaults and are swapped whole,
-- keyed on their trailing marker comments — the runtime harness convention:
-- keep each marker at the end of its line, and never spell a complete marker
-- anywhere else.

local VERSION = '0.0.0-dev' -- __LEST_STUDIO_VERSION__
local PORT = 28806 -- __LEST_STUDIO_PORT__
local SECRET = 'unconfigured' -- __LEST_STUDIO_SECRET__

local Bridge = require(script.bridge)

local HttpService = game:GetService('HttpService')
local RunService = game:GetService('RunService')
local LogService = game:GetService('LogService')
local ServerScriptService = game:GetService('ServerScriptService')

-- The Script the current run injected, if any; cleanup destroys it (injected
-- instances survive Stop, so nothing else will).
local runScript: Script? = nil

local bridge = Bridge.new({ port = PORT, secret = SECRET, version = VERSION }, {
	request = function (options)
		local ok, response = pcall(function ()
			return HttpService:RequestAsync({
				Url = options.url,
				Method = options.method,
				Headers = options.headers,
				Body = options.body,
			})
		end)
		if not ok then
			return { ok = false, error = tostring(response) }
		end
		return { ok = true, status = response.StatusCode, body = response.Body }
	end,
	encode = function (value)
		return HttpService:JSONEncode(value)
	end,
	decode = function (text)
		return HttpService:JSONDecode(text)
	end,
	log = print,
	warn = warn,
	place = function ()
		return { name = game.Name, placeId = game.PlaceId }
	end,
	clock = os.clock,
	run = {
		inject = function (source)
			local ok, err = pcall(function ()
				-- Sweep any stale script first: a crashed or closed session
				-- never ran teardown, and a leftover re-executes on every
				-- Run press — doubling outcomes during the next lest run.
				local stale = ServerScriptService:FindFirstChild('LestRun')
				while stale ~= nil do
					stale:Destroy()
					stale = ServerScriptService:FindFirstChild('LestRun')
				end
				local injected = Instance.new('Script')
				injected.Name = 'LestRun'
				injected.Source = source
				injected.Parent = ServerScriptService
				runScript = injected
			end)
			return ok, if ok then nil else tostring(err)
		end,
		tryStart = function ()
			-- Expected to be a no-op on today's platform (hand-verified);
			-- attempted anyway so lest gets the automatic start the moment
			-- the engine ever honors it. The wait gives a successful call
			-- time to actually transition before we report the verdict.
			pcall(function ()
				RunService:Run()
			end)
			task.wait(1)
			return RunService:IsRunning()
		end,
		tryStop = function ()
			pcall(function ()
				RunService:Stop()
			end)
			task.wait(0.5)
			return not RunService:IsRunning()
		end,
		isRunning = function ()
			return RunService:IsRunning()
		end,
		cleanup = function ()
			if runScript ~= nil then
				pcall(function ()
					(runScript :: Script):Destroy()
				end)
				runScript = nil
			end
		end,
		connectLog = function (callback)
			local connection = LogService.MessageOut:Connect(function (message)
				callback(message)
			end)
			return function ()
				connection:Disconnect()
			end
		end,
	},
})

-- The one load-time line: this plugin has no toolbar button and no UI, so
-- without it there is nothing visible to distinguish "loaded and idle" from
-- "never loaded at all" — the first question every setup problem starts with.
print(`lest plugin {VERSION} loaded (bridge port {PORT})`)

task.spawn(function ()
	while true do
		-- `step` is not expected to throw (its deps are called protected),
		-- but an unexpected error must not kill this thread — a dead loop is
		-- a plugin that silently never polls again until Studio restarts.
		local ok, delay = pcall(bridge.step)
		task.wait(if ok then delay else 5)
	end
end)
