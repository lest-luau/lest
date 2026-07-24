--!strict
-- lest Studio companion plugin — the entry script. `lest studio install`
-- writes this (with the placeholder lines below substituted) into the local
-- Plugins folder as `lest.rbxmx`; hand edits there are overwritten by the
-- next install.
--
-- Engine globals live only in this file, which is why it is excluded in
-- selene.toml (the same accommodation the engine specs get). Every decision —
-- polling, backoff, warnings, job handling — lives in the `bridge` module
-- next door, which is pure Luau and tested by lest's own native suite
-- (tests/studio). This file only wires the real engine into the bridge's
-- `Deps` and loops.
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
})

task.spawn(function ()
	while true do
		-- `step` is not expected to throw (its deps are called protected),
		-- but an unexpected error must not kill this thread — a dead loop is
		-- a plugin that silently never polls again until Studio restarts.
		local ok, delay = pcall(bridge.step)
		task.wait(if ok then delay else 5)
	end
end)
