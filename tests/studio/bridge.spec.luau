--!strict
-- The Studio plugin's bridge, driven entirely by scripted fakes: every
-- request the bridge would make is recorded and answered from a queue, so
-- these specs pin down the plugin's observable behavior — what it sends,
-- what it warns, how it backs off — without Studio anywhere near the run.

local Lest = require('@lest')
local Bridge = require('../../luau/studio/bridge')

local describe, it, expect = Lest.describe, Lest.it, Lest.expect

type Recorded = {
	method: string,
	url: string,
	headers: { [string]: string },
	body: string?,
}

type Harness = {
	bridge: Bridge.Bridge,
	requests: { Recorded },
	logs: { string },
	warnings: { string },
	--- Queue one transport result for the next request.
	respond: (result: Bridge.RequestResult) -> (),
	--- Register what a body string decodes to (decode throws on anything else).
	registerBody: (text: string, value: any) -> (),
	--- Everything the bridge passed to `encode`, in order.
	encoded: { any },
}

local CONFIG: Bridge.Config = { port = 41999, secret = 'shh-secret', version = '1.2.3' }

local function makeHarness (): Harness
	local requests: { Recorded } = {}
	local logs: { string } = {}
	local warnings: { string } = {}
	local queue: { Bridge.RequestResult } = {}
	local bodies: { [string]: any } = {}
	local encoded: { any } = {}

	local deps: Bridge.Deps = {
		request = function (options)
			table.insert(requests, {
				method = options.method,
				url = options.url,
				headers = options.headers,
				body = options.body,
			})
			if #queue == 0 then
				error('the spec did not queue a response for this request: ' .. options.url)
			end
			return table.remove(queue, 1) :: Bridge.RequestResult
		end,
		encode = function (value)
			table.insert(encoded, value)
			return 'encoded'
		end,
		decode = function (text)
			local value = bodies[text]
			if value == nil then
				error('undecodable body: ' .. text)
			end
			return value
		end,
		log = function (message)
			table.insert(logs, message)
		end,
		warn = function (message)
			table.insert(warnings, message)
		end,
		place = function ()
			return { name = 'Test Place', placeId = 12345 }
		end,
	}

	local bridge = Bridge.new(CONFIG, deps)
	return {
		bridge = bridge,
		requests = requests,
		logs = logs,
		warnings = warnings,
		respond = function (result)
			table.insert(queue, result)
		end,
		registerBody = function (text, value)
			bodies[text] = value
		end,
		encoded = encoded,
	}
end

local NO_SERVER: Bridge.RequestResult = { ok = false, error = 'ConnectFail: connection refused' }
local IDLE: Bridge.RequestResult = { ok = true, status = 204 }

describe('polling', function ()
	it('GETs /job on the loopback port with the secret and version headers', function ()
		local h = makeHarness()
		h.respond(IDLE)
		h.bridge.step()
		expect(#h.requests).toBe(1)
		local request = h.requests[1]
		expect(request.method).toBe('GET')
		expect(request.url).toBe('http://127.0.0.1:41999/job')
		expect(request.headers['x-lest-secret']).toBe('shh-secret')
		expect(request.headers['x-lest-version']).toBe('1.2.3')
	end)

	it('stays quiet and quick on an idle long-poll expiry', function ()
		local h = makeHarness()
		h.respond(IDLE)
		local delay = h.bridge.step()
		expect(delay).toBeLessThan(0.5)
		expect(#h.warnings).toBe(0)
	end)
end)

describe('backoff', function ()
	it('grows per consecutive failure and is capped low for an absent server', function ()
		local h = makeHarness()
		local previous = 0
		local last = 0
		-- Enough failures to hit any reasonable cap.
		for _ = 1, 8 do
			h.respond(NO_SERVER)
			previous = last
			last = h.bridge.step()
		end
		expect(last).toBeGreaterThan(1)
		-- Capped: the last two delays are equal once the ceiling is reached.
		expect(last).toBe(previous)
		-- And capped *low*: connection refusals are near-free local errors,
		-- and this ceiling is the worst-case latency before the plugin
		-- notices a freshly started CLI session.
		expect(last).toBeLessThanOrEqual(2)
	end)

	it('refused polls back off past the absent-server ceiling and cap', function ()
		local h = makeHarness()
		local previous = 0
		local last = 0
		for _ = 1, 8 do
			h.respond({ ok = true, status = 403 })
			previous = last
			last = h.bridge.step()
		end
		-- A server that answers-but-refuses costs real requests against
		-- Studio's shared HTTP budget: the suspect ceiling is higher.
		expect(last).toBeGreaterThan(4)
		expect(last).toBe(previous)
	end)

	it('resets after a successful contact', function ()
		local h = makeHarness()
		for _ = 1, 5 do
			h.respond(NO_SERVER)
			h.bridge.step()
		end
		h.respond(IDLE)
		h.bridge.step()
		h.respond(NO_SERVER)
		local delay = h.bridge.step()
		expect(delay).toBeLessThan(1)
	end)

	it('never warns about an absent server', function ()
		local h = makeHarness()
		for _ = 1, 10 do
			h.respond(NO_SERVER)
			h.bridge.step()
		end
		expect(#h.warnings).toBe(0)
	end)
end)

describe('connection logging', function ()
	it('logs connect and disconnect once per transition, not per poll', function ()
		local h = makeHarness()
		h.respond(IDLE)
		h.bridge.step()
		h.respond(IDLE)
		h.bridge.step()
		expect(#h.logs).toBe(1)
		expect(h.logs[1]).toContain('connected')
		expect(h.logs[1]).toContain('41999')
		h.respond(NO_SERVER)
		h.bridge.step()
		h.respond(NO_SERVER)
		h.bridge.step()
		expect(#h.logs).toBe(2)
		expect(h.logs[2]).toContain('disconnected')
	end)
end)

describe('warnings', function ()
	it('warns once, with guidance, when Studio blocks HTTP permission', function ()
		local h = makeHarness()
		h.respond({ ok = false, error = 'Http requests are blocked: missing Permission for this plugin' })
		h.bridge.step()
		h.respond({ ok = false, error = 'Http requests are blocked: missing Permission for this plugin' })
		h.bridge.step()
		expect(#h.warnings).toBe(1)
		expect(h.warnings[1]).toContain('Plugin Management')
	end)

	it('warns once and points at reinstall when the CLI refuses the secret', function ()
		local h = makeHarness()
		h.respond({ ok = true, status = 403 })
		h.bridge.step()
		h.respond({ ok = true, status = 403 })
		h.bridge.step()
		expect(#h.warnings).toBe(1)
		expect(h.warnings[1]).toContain('lest studio install')
	end)

	it('a refused secret is not reported as a connection', function ()
		local h = makeHarness()
		h.respond({ ok = true, status = 401 })
		h.bridge.step()
		expect(#h.logs).toBe(0)
	end)

	it('a refusal after a live connection does not claim the session ended', function ()
		local h = makeHarness()
		h.respond(IDLE)
		h.bridge.step()
		h.respond({ ok = true, status = 403 })
		h.bridge.step()
		-- The connect line only: a refusal is not the session "ending", and
		-- the deduplicated warning already names what actually happened.
		expect(#h.logs).toBe(1)
		expect(#h.warnings).toBe(1)
	end)

	it('re-warns about a problem that returns in a later session', function ()
		local h = makeHarness()
		local blocked: Bridge.RequestResult = { ok = false, error = 'permission denied by the user' }
		h.respond(blocked)
		h.bridge.step()
		h.respond(blocked)
		h.bridge.step()
		expect(#h.warnings).toBe(1)
		-- A healthy contact opens a fresh warning epoch...
		h.respond(IDLE)
		h.bridge.step()
		-- ...so the same problem recurring warns again instead of never.
		h.respond(blocked)
		h.bridge.step()
		expect(#h.warnings).toBe(2)
	end)

	it('treats an undecodable 200 as suspect: warns once, backs off, never connects', function ()
		local h = makeHarness()
		local first = 0
		local last = 0
		for index = 1, 6 do
			h.respond({ ok = true, status = 200, body = 'garbage' })
			local delay = h.bridge.step()
			if index == 1 then
				first = delay
			end
			last = delay
		end
		expect(#h.warnings).toBe(1)
		expect(h.warnings[1]).toContain('lest studio install')
		-- Growing, not fast-polling: a foreign server answering 200s must
		-- not be hammered at the idle rate (Studio's HTTP budget is shared).
		expect(last).toBeGreaterThan(first)
		expect(#h.logs).toBe(0)
	end)

	it('treats an unexpected status as suspect: warns once naming it, backs off', function ()
		local h = makeHarness()
		local last = 0
		for _ = 1, 6 do
			h.respond({ ok = true, status = 500 })
			last = h.bridge.step()
		end
		expect(#h.warnings).toBe(1)
		expect(h.warnings[1]).toContain('500')
		expect(last).toBeGreaterThan(2)
		expect(#h.logs).toBe(0)
	end)

	it('treats a job missing its id or kind as undecodable', function ()
		local h = makeHarness()
		h.registerBody('no-kind', { id = 'j1' })
		h.respond({ ok = true, status = 200, body = 'no-kind' })
		h.bridge.step()
		expect(#h.warnings).toBe(1)
		-- Nothing was posted back: the one request is the poll itself.
		expect(#h.requests).toBe(1)
	end)

	it('treats a job that decodes to a non-table as undecodable', function ()
		local h = makeHarness()
		h.registerBody('just-a-number', 5)
		h.respond({ ok = true, status = 200, body = 'just-a-number' })
		h.bridge.step()
		expect(#h.warnings).toBe(1)
		expect(#h.requests).toBe(1)
	end)
end)

describe('jobs', function ()
	it('answers ping with the plugin version and place identity', function ()
		local h = makeHarness()
		h.registerBody('ping-job', { id = 'job-1', kind = 'ping' })
		h.respond({ ok = true, status = 200, body = 'ping-job' })
		h.respond({ ok = true, status = 200 }) -- the POST /result answer
		h.bridge.step()

		expect(#h.requests).toBe(2)
		local post = h.requests[2]
		expect(post.method).toBe('POST')
		expect(post.url).toBe('http://127.0.0.1:41999/result')
		expect(post.headers['content-type']).toBe('application/json')
		expect(post.headers['x-lest-secret']).toBe('shh-secret')
		expect(post.headers['x-lest-version']).toBe('1.2.3')
		expect(post.body).toBe('encoded')

		expect(#h.encoded).toBe(1)
		local payload = h.encoded[1]
		expect(payload.id).toBe('job-1')
		expect(payload.ok).toBe(true)
		expect(payload.plugin).toEqual({ version = '1.2.3' })
		expect(payload.place).toEqual({ name = 'Test Place', placeId = 12345 })
	end)

	it('refuses an unknown job kind by name, keeping the job id', function ()
		local h = makeHarness()
		h.registerBody('future-job', { id = 'job-2', kind = 'run-suite' })
		h.respond({ ok = true, status = 200, body = 'future-job' })
		h.respond({ ok = true, status = 200 })
		h.bridge.step()

		expect(#h.encoded).toBe(1)
		local payload = h.encoded[1]
		expect(payload.id).toBe('job-2')
		expect(payload.ok).toBe(false)
		expect(payload.error).toContain('run-suite')
		expect(payload.error).toContain('lest studio install')
	end)

	it('still answers a valid job after warning about a bad one', function ()
		local h = makeHarness()
		h.respond({ ok = true, status = 200, body = 'garbage' })
		h.bridge.step()
		h.registerBody('ping-job', { id = 'job-9', kind = 'ping' })
		h.respond({ ok = true, status = 200, body = 'ping-job' })
		h.respond({ ok = true, status = 200 })
		h.bridge.step()
		expect(#h.encoded).toBe(1)
		expect(h.encoded[1].id).toBe('job-9')
	end)

	it('survives a result post failing (the session died mid-job)', function ()
		local h = makeHarness()
		h.registerBody('ping-job', { id = 'job-3', kind = 'ping' })
		h.respond({ ok = true, status = 200, body = 'ping-job' })
		h.respond(NO_SERVER) -- the POST fails
		expect(function ()
			h.bridge.step()
		end).toNotThrow()
	end)
end)
