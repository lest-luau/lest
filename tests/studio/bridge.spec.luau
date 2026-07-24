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

--- A scripted engine for run jobs: records every hook call, lets the spec
--- fire log lines and flip the running state.
type FakeRun = {
	hooks: Bridge.RunHooks,
	injected: { string },
	startAttempts: { boolean },
	stopAttempts: { boolean },
	cleanups: { boolean },
	disconnects: { boolean },
	--- What `isRunning` reports; `tryStart`/`tryStop` set it to their
	--- configured outcomes below.
	running: { boolean },
	startWorks: { boolean },
	stopWorks: { boolean },
	--- Fires the callback `connectLog` registered (errors when none is).
	emitLog: (message: string) -> (),
}

local function makeFakeRun (): FakeRun
	local fake: FakeRun
	local logCallback: ((message: string) -> ())? = nil
	fake = {
		injected = {},
		startAttempts = {},
		stopAttempts = {},
		cleanups = {},
		disconnects = {},
		running = { false },
		startWorks = { false },
		stopWorks = { true },
		emitLog = function (message)
			assert(logCallback ~= nil, 'no log callback connected')
			logCallback(message)
		end,
		hooks = {
			inject = function (source)
				table.insert(fake.injected, source)
				return true, nil
			end,
			tryStart = function ()
				table.insert(fake.startAttempts, true)
				fake.running[1] = fake.startWorks[1]
				return fake.running[1]
			end,
			tryStop = function ()
				table.insert(fake.stopAttempts, true)
				if fake.stopWorks[1] then
					fake.running[1] = false
				end
				return not fake.running[1]
			end,
			isRunning = function ()
				return fake.running[1]
			end,
			cleanup = function ()
				table.insert(fake.cleanups, true)
			end,
			connectLog = function (callback)
				logCallback = callback
				return function ()
					table.insert(fake.disconnects, true)
					logCallback = nil
				end
			end,
		},
	}
	return fake
end

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
	--- Advance the fake clock by seconds.
	advance: (seconds: number) -> (),
}

local CONFIG: Bridge.Config = { port = 41999, secret = 'shh-secret', version = '1.2.3' }

local function makeHarness (fakeRun: FakeRun?): Harness
	local requests: { Recorded } = {}
	local logs: { string } = {}
	local warnings: { string } = {}
	local queue: { Bridge.RequestResult } = {}
	local bodies: { [string]: any } = {}
	local encoded: { any } = {}
	local now = { 0 }

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
		clock = function ()
			return now[1]
		end,
		run = if fakeRun ~= nil then fakeRun.hooks else nil,
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
		advance = function (seconds)
			now[1] += seconds
		end,
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

	it('absent-server polls shed the memory of a suspect episode', function ()
		local h = makeHarness()
		-- Grow the backoff to the suspect ceiling first.
		for _ = 1, 6 do
			h.respond({ ok = true, status = 403 })
			h.bridge.step()
		end
		-- Once the port goes quiet, delays clamp back to the absent ceiling
		-- within two steps: a fresh CLI session is still noticed fast.
		h.respond(NO_SERVER)
		h.bridge.step()
		h.respond(NO_SERVER)
		local delay = h.bridge.step()
		expect(delay).toBeLessThanOrEqual(2)
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

local RUN_JOB = {
	id = 'run-1',
	kind = 'run',
	bundle = 'THE BUNDLE',
	markers = { event = '@@E@@', spec = '@@S@@', done = '@@D@@' },
}

--- Feeds the run job to an idle bridge and answers its ack. One poll, one
--- POST /result.
local function startRun (h: Harness)
	h.registerBody('run-job', RUN_JOB)
	h.respond({ ok = true, status = 200, body = 'run-job' })
	h.respond({ ok = true, status = 200 })
	h.bridge.step()
end

describe('run jobs', function ()
	it('refuses a run when the host has no engine hooks', function ()
		local h = makeHarness()
		h.registerBody('run-job', RUN_JOB)
		h.respond({ ok = true, status = 200, body = 'run-job' })
		h.respond({ ok = true, status = 200 })
		h.bridge.step()
		expect(#h.encoded).toBe(1)
		expect(h.encoded[1].ok).toBe(false)
		expect(h.encoded[1].error).toContain('cannot run suites')
	end)

	it('refuses a malformed run job with update guidance', function ()
		local fake = makeFakeRun()
		local h = makeHarness(fake)
		h.registerBody('bad-run', { id = 'r2', kind = 'run', bundle = 'X', markers = { event = '@@E@@' } })
		h.respond({ ok = true, status = 200, body = 'bad-run' })
		h.respond({ ok = true, status = 200 })
		h.bridge.step()
		expect(h.encoded[1].ok).toBe(false)
		expect(h.encoded[1].error).toContain('lest studio install')
		expect(#fake.injected).toBe(0)
	end)

	it('injects, attempts the start, and acks with started=false plus the armed log', function ()
		local fake = makeFakeRun()
		local h = makeHarness(fake)
		startRun(h)
		expect(fake.injected).toEqual({ 'THE BUNDLE' })
		expect(#fake.startAttempts).toBe(1)
		local ack = h.encoded[1]
		expect(ack.id).toBe('run-1')
		expect(ack.ok).toBe(true)
		expect(ack.started).toBe(false)
		-- The connect log plus exactly one armed instruction.
		expect(h.logs[#h.logs]).toContain('press Run')
	end)

	it('acks started=true and skips the armed log when the start works', function ()
		local fake = makeFakeRun()
		fake.startWorks[1] = true
		local h = makeHarness(fake)
		startRun(h)
		expect(h.encoded[1].started).toBe(true)
		for _, line in h.logs do
			-- No toNotContain matcher yet; a nil find is the same claim.
			expect(string.find(line, 'press Run', 1, true)).toBeNil()
		end
	end)

	it('relays only marker-carrying lines, batched to /events with the job id', function ()
		local fake = makeFakeRun()
		local h = makeHarness(fake)
		startRun(h)
		fake.emitLog('plain studio chatter')
		fake.emitLog('@@E@@{"kind":"test_pass"}')
		fake.emitLog('@@S@@1')
		h.respond({ ok = true, status = 200 })
		h.bridge.step()
		local flush = h.encoded[2]
		expect(flush.id).toBe('run-1')
		expect(flush.lines).toEqual({ '@@E@@{"kind":"test_pass"}', '@@S@@1' })
		local post = h.requests[#h.requests]
		expect(post.url).toBe('http://127.0.0.1:41999/events')
	end)

	it('finishes on the done marker: flush, stop, cleanup, disconnect, /done', function ()
		local fake = makeFakeRun()
		local h = makeHarness(fake)
		startRun(h)
		fake.running[1] = true -- the user pressed Run
		fake.emitLog('@@E@@{"kind":"run_end"}')
		fake.emitLog('@@D@@')
		h.respond({ ok = true, status = 200 }) -- the /events flush
		h.respond({ ok = true, status = 200 }) -- the /done
		h.bridge.step()
		expect(#fake.stopAttempts).toBe(1)
		expect(#fake.cleanups).toBe(1)
		expect(#fake.disconnects).toBe(1)
		local done = h.encoded[#h.encoded]
		expect(done.id).toBe('run-1')
		expect(done.ok).toBe(true)
		expect(done.stopped).toBe(true)
		local doneUrl = h.requests[#h.requests].url
		expect(doneUrl).toBe('http://127.0.0.1:41999/done')
		-- Idle again: the next step polls /job.
		h.respond(IDLE)
		h.bridge.step()
		expect(h.requests[#h.requests].url).toBe('http://127.0.0.1:41999/job')
	end)

	it('asks the user to press Stop when the stop attempt fails', function ()
		local fake = makeFakeRun()
		fake.stopWorks[1] = false
		local h = makeHarness(fake)
		startRun(h)
		fake.running[1] = true
		fake.emitLog('@@D@@')
		h.respond({ ok = true, status = 200 })
		h.respond({ ok = true, status = 200 })
		h.bridge.step()
		expect(h.logs[#h.logs]).toContain('press Stop')
		expect(h.encoded[#h.encoded].stopped).toBe(false)
	end)

	it('sends an empty keepalive after the quiet interval', function ()
		local fake = makeFakeRun()
		local h = makeHarness(fake)
		startRun(h)
		-- No lines, clock still fresh: the step makes no request at all.
		h.bridge.step()
		local before = #h.requests
		h.advance(3)
		h.respond({ ok = true, status = 200 })
		h.bridge.step()
		expect(#h.requests).toBe(before + 1)
		expect(h.encoded[#h.encoded].lines).toEqual({})
	end)

	it('aborts the run after repeated failed posts (the CLI died)', function ()
		local fake = makeFakeRun()
		local h = makeHarness(fake)
		startRun(h)
		fake.running[1] = true
		for _ = 1, 3 do
			fake.emitLog('@@E@@{"kind":"test_pass"}')
			h.respond(NO_SERVER)
			h.bridge.step()
			h.advance(3)
		end
		expect(#h.warnings).toBe(1)
		expect(h.warnings[1]).toContain('lost its CLI session')
		expect(#fake.stopAttempts).toBe(1)
		expect(#fake.cleanups).toBe(1)
		expect(#fake.disconnects).toBe(1)
		-- Idle again afterward.
		h.respond(IDLE)
		h.bridge.step()
		expect(h.requests[#h.requests].url).toBe('http://127.0.0.1:41999/job')
	end)
end)
