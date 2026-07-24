--!strict
--- The Studio plugin's brain: polls the lest CLI's loopback bridge for jobs
--- and answers them. Deliberately free of engine globals — everything the
--- engine provides (HttpService, JSON, the output window, place identity)
--- arrives through `Deps`, injected by the entry script (main.luau) in Studio
--- and by scripted fakes in lest's own native suite (tests/studio). The same
--- dependency-injection accommodation the cloud scheduler makes for `task`,
--- for the same two reasons: this file stays lint-clean under the plain Luau
--- std, and its behavior is testable without opening Studio.
---
--- Polling, not pushing: the CLI comes and goes (a `lest run` here, a watch
--- session there) while Studio stays open, so the plugin must be the side
--- that knocks. run-in-roblox POSTs unprompted because it launches Studio
--- itself and knows a server is listening; lest's persistent-session design
--- inverts that. Failed polls while no CLI is running are the *normal* state
--- and must stay silent — the backoff exists so an idle Studio costs nothing.
---
--- Wire contract (the CLI's bridge server implements the other half):
---   * `GET /job` — long-poll; 200 + JSON `{ id, kind, ... }` hands the
---     plugin a job, 204 means "alive, nothing for you". The server must
---     hold well under 30 seconds: Studio's `RequestAsync` timeout is fixed
---     near 30s, and a hold that trips it reads as a disconnect here.
---   * `POST /result` — the answer to a job, JSON `{ id, ok, ... }`. The id
---     is the only correlation — the server tracks outstanding job ids.
---   * Every request carries `x-lest-secret` and `x-lest-version` headers;
---     the server must match header names case-insensitively.
---   * Two Studio instances share one installed plugin (and secret) and will
---     both poll the same port; the server must tolerate concurrent pollers
---     and use the ping answer's `place` to tell sessions apart.

export type RequestOptions = {
	method: 'GET' | 'POST',
	url: string,
	headers: { [string]: string },
	body: string?,
}

--- Transport-level result. `ok = true` means a response arrived (any HTTP
--- status); `ok = false` means the request itself failed — no server, no
--- permission, no network stack.
export type RequestResult = {
	ok: boolean,
	status: number?,
	body: string?,
	error: string?,
}

--- Engine hooks for run jobs, implemented by main.luau against the real
--- engine and by scripted fakes in the specs. `nil` on hosts that cannot
--- run suites — the bridge then refuses run jobs with a clear error.
export type RunHooks = {
	--- Parent the bundle as an injected Script. `false` plus a reason on
	--- failure (no Script Injection permission, say).
	inject: (source: string) -> (boolean, string?),
	--- Attempt to start the playtest and report whether the world is now
	--- running. Expected to fail on today's platform (the Run() API is a
	--- no-op from plugin-adjacent contexts); the user's Run press is the
	--- fallback, which `isRunning` observes.
	tryStart: () -> boolean,
	--- Attempt to stop the playtest; report whether it is now stopped.
	tryStop: () -> boolean,
	isRunning: () -> boolean,
	--- Remove whatever `inject` created (injected instances survive Stop).
	cleanup: () -> (),
	--- Subscribe to log lines (LogService); returns the disconnect.
	connectLog: (callback: (message: string) -> ()) -> () -> (),
}

export type Deps = {
	request: (options: RequestOptions) -> RequestResult,
	encode: (value: any) -> string,
	--- May throw on malformed input; the bridge calls it protected.
	decode: (text: string) -> any,
	log: (message: string) -> (),
	warn: (message: string) -> (),
	place: () -> { name: string, placeId: number },
	--- Monotonic seconds (os.clock); drives the run keepalive cadence.
	clock: () -> number,
	run: RunHooks?,
}

--- Baked into the installed plugin by `lest studio install` (main.luau holds
--- the placeholder lines); never read from anywhere at runtime.
export type Config = {
	port: number,
	secret: string,
	version: string,
}

export type Bridge = {
	--- One poll cycle. Returns how many seconds the caller should wait before
	--- the next cycle — small while a CLI session is live, growing toward a
	--- backoff ceiling while nothing (or something suspicious) is listening.
	step: () -> number,
}

--- Delay after a healthy contact (an idle 204 or a decoded job): the server
--- side long-polls, so the next request can go straight back out.
local POLL_IDLE = 0.1
--- Step cadence while a run is active: fast enough that event batches feel
--- live in the terminal, slow enough to stay far inside the HTTP budget.
local RUN_POLL = 0.2
--- With no lines to flush, an empty `/events` post goes out this often —
--- the CLI's liveness signal, and the plugin's way of noticing a dead CLI.
local KEEPALIVE_SECONDS = 2
--- Consecutive failed posts before the plugin declares the CLI dead and
--- aborts the run (stop, clean up, back to idle polling).
local MAX_POST_FAILURES = 3
--- First retry delay after any unhealthy poll; doubles per consecutive one.
local BACKOFF_START = 0.5
--- Ceiling when nothing is listening at all. Kept low — a connection refusal
--- is a near-free local error, and this ceiling is exactly the worst-case
--- latency between `lest run` starting and the plugin noticing. (S3's warm
--- loop budget rides on it.)
local BACKOFF_MAX_ABSENT = 2
--- Ceiling when a server *answered* but not like the lest bridge — a foreign
--- server on the port, a refused secret, an undecodable job. These cost real
--- requests against something real (and Studio's HTTP budget is shared
--- across every plugin), so back way off instead of hammering it.
local BACKOFF_MAX_SUSPECT = 8

local Bridge = {}

--- Transport errors that look like Studio's permission refusals rather than
--- an absent server. Matched loosely on purpose: the exact wording is
--- Studio's to change, and misclassifying an absent server as a permission
--- problem only costs one extra (deduplicated) warning.
local function looksLikePermissionError (message: string): boolean
	local lowered = string.lower(message)
	return string.find(lowered, 'permission', 1, true) ~= nil
		or string.find(lowered, 'not allowed', 1, true) ~= nil
		or string.find(lowered, 'trust', 1, true) ~= nil
end

function Bridge.new (config: Config, deps: Deps): Bridge
	-- Current backoff; grows toward BACKOFF_MAX_SUSPECT, clamped per path by
	-- the caller's ceiling, reset by any healthy contact.
	local backoff = BACKOFF_START
	-- Whether the last poll reached a healthy CLI session, so connect and
	-- disconnect each log exactly once per transition instead of per poll.
	local connected = false
	-- Problems already warned about this connection epoch; the output window
	-- is shared with every other plugin, so each distinct problem gets one
	-- line, not a stream. Cleared on (re)connect, so a problem that comes
	-- back in a later session warns again instead of never.
	local warned: { [string]: boolean } = {}

	local function warnOnce (key: string, message: string)
		if warned[key] then
			return
		end
		warned[key] = true
		deps.warn(message)
	end

	local function headers (): { [string]: string }
		return {
			['x-lest-secret'] = config.secret,
			['x-lest-version'] = config.version,
		}
	end

	local function url (path: string): string
		return `http://127.0.0.1:{config.port}{path}`
	end

	--- An unhealthy poll of either kind: escalate the backoff (never
	--- logging) and return the delay to wait, clamped to the path's ceiling.
	--- The stored backoff is clamped too, so absent-server polls shed any
	--- 8s memory a suspect episode left behind: once the port goes quiet,
	--- the plugin is back to noticing a fresh CLI session within 2s.
	local function escalate (ceiling: number): number
		connected = false
		local delay = math.min(backoff, ceiling)
		backoff = math.min(backoff * 2, ceiling)
		return delay
	end

	--- A transport failure — the one unhealthy shape that means "the session
	--- ended" when we were connected, which is worth a log line.
	local function failedPoll (): number
		if connected then
			deps.log('lest bridge disconnected — the CLI session ended')
		end
		return escalate(BACKOFF_MAX_ABSENT)
	end

	--- A healthy contact: reset the backoff, open a fresh warning epoch on
	--- the transition, and note the connect exactly once.
	local function contacted ()
		backoff = BACKOFF_START
		if not connected then
			connected = true
			warned = {}
			deps.log(`lest bridge connected (port {config.port})`)
		end
	end

	--- POSTs a JSON payload; true when the server acknowledged it.
	local function postJson (path: string, payload: { [string]: any }): boolean
		local postHeaders = headers()
		postHeaders['content-type'] = 'application/json'
		local result = deps.request({
			method = 'POST',
			url = url(path),
			headers = postHeaders,
			body = deps.encode(payload),
		})
		return result.ok and (result.status == 200 or result.status == 204)
	end

	--- Answers one decoded non-run job. Every known kind posts a `/result`
	--- addressed by the job's id; unknown kinds post a refusal that names the
	--- plugin version, so a newer CLI talking to an older plugin fails loudly
	--- with the fix visible (`lest studio install` again) instead of hanging.
	--- A failed post means the session died mid-job; the next poll notices.
	local function answer (job: { [string]: any })
		local payload: { [string]: any }
		if job.kind == 'ping' then
			payload = {
				id = job.id,
				ok = true,
				plugin = { version = config.version },
				place = deps.place(),
			}
		else
			payload = {
				id = job.id,
				ok = false,
				error = `unsupported job kind '{tostring(job.kind)}' — plugin {config.version} predates it; run \`lest studio install\` to update`,
			}
		end
		postJson('/result', payload)
	end

	-- The in-flight run, if any. One at a time: the CLI serializes suites
	-- over one bridge session, so a second run job during one is refused.
	type ActiveRun = {
		id: string,
		disconnect: () -> (),
		buffer: { string },
		markers: { event: string, spec: string, done: string },
		done: boolean,
		lastPost: number,
		postFailures: number,
	}
	local active: ActiveRun? = nil

	local function lineHasMarker (run: ActiveRun, message: string): boolean
		return string.find(message, run.markers.event, 1, true) ~= nil
			or string.find(message, run.markers.spec, 1, true) ~= nil
			or string.find(message, run.markers.done, 1, true) ~= nil
	end

	--- Starts a run job: subscribe to the log relay, inject the bundle,
	--- attempt the adaptive start, and acknowledge with whether the world is
	--- already running (`started = false` means the user's Run press starts
	--- it). Marker strings ride in the job payload, so the plugin never
	--- hardcodes them and cannot drift from the CLI's decoder.
	local function startRun (job: { [string]: any }): number
		local hooks = deps.run
		if hooks == nil then
			postJson('/result', {
				id = job.id,
				ok = false,
				error = 'this host cannot run suites (no engine hooks)',
			})
			return POLL_IDLE
		end
		if active ~= nil then
			postJson('/result', { id = job.id, ok = false, error = 'a run is already in progress' })
			return POLL_IDLE
		end
		local markers = job.markers
		if
			type(job.bundle) ~= 'string'
			or type(markers) ~= 'table'
			or type(markers.event) ~= 'string'
			or type(markers.spec) ~= 'string'
			or type(markers.done) ~= 'string'
		then
			postJson('/result', {
				id = job.id,
				ok = false,
				error = `malformed run job — plugin {config.version}; run \`lest studio install\` to update`,
			})
			return POLL_IDLE
		end

		local run: ActiveRun = {
			id = job.id,
			disconnect = function () end,
			buffer = {},
			markers = { event = markers.event, spec = markers.spec, done = markers.done },
			done = false,
			lastPost = deps.clock(),
			postFailures = 0,
		}
		-- Only marker-carrying lines relay: LogService is shared with every
		-- other plugin's chatter, and the CLI only decodes framed lines.
		run.disconnect = hooks.connectLog(function (message)
			if not lineHasMarker(run, message) then
				return
			end
			table.insert(run.buffer, message)
			if string.find(message, run.markers.done, 1, true) ~= nil then
				run.done = true
			end
		end)

		local injected, reason = hooks.inject(job.bundle)
		if not injected then
			run.disconnect()
			postJson('/result', {
				id = job.id,
				ok = false,
				error = 'cannot inject the test bundle: ' .. tostring(reason),
			})
			return POLL_IDLE
		end

		local started = hooks.tryStart()
		if not started then
			deps.log('lest run armed — press Run (F8) in Studio to execute the suite')
		end
		active = run
		postJson('/result', { id = job.id, ok = true, started = started })
		return RUN_POLL
	end

	--- Ends a run and restores idle state, stopping the world and removing
	--- the injected script. Shared by completion and CLI-death abort.
	local function teardown (run: ActiveRun): boolean
		local hooks = deps.run :: RunHooks
		local stopped = if hooks.isRunning() then hooks.tryStop() else true
		run.disconnect()
		hooks.cleanup()
		active = nil
		return stopped
	end

	--- One management step of the active run: flush buffered lines (or a
	--- keepalive), detect a dead CLI, and finish when the done marker has
	--- been seen and fully relayed.
	local function manageRun (): number
		local run = active :: ActiveRun
		local doneBeforeFlush = run.done
		local lines = run.buffer
		if #lines > 0 or (deps.clock() - run.lastPost) >= KEEPALIVE_SECONDS then
			run.buffer = {}
			run.lastPost = deps.clock()
			if postJson('/events', { id = run.id, lines = lines }) then
				run.postFailures = 0
			else
				run.postFailures += 1
				if run.postFailures >= MAX_POST_FAILURES then
					deps.warn('lest lost its CLI session mid-run — stopping the playtest and cleaning up')
					local _ = teardown(run)
					return failedPoll()
				end
			end
		end
		-- Late lines land in the fresh buffer during the flush's yield and
		-- go out next step; finishing waits until everything relayed.
		if doneBeforeFlush and #run.buffer == 0 and run.postFailures == 0 then
			local stopped = teardown(run)
			if not stopped then
				deps.log('lest run finished — press Stop in Studio to end the playtest')
			end
			postJson('/done', { id = run.id, ok = true, stopped = stopped })
			return POLL_IDLE
		end
		return RUN_POLL
	end

	local function step (): number
		if active ~= nil then
			return manageRun()
		end

		local result = deps.request({
			method = 'GET',
			url = url('/job'),
			headers = headers(),
		})

		if not result.ok then
			if looksLikePermissionError(result.error or '') then
				warnOnce(
					'permission',
					'lest cannot reach the local CLI: Studio is blocking this plugin from making HTTP requests'
						.. ' — allow the lest plugin under Plugin Management and it will connect on its own'
				)
			end
			return failedPoll()
		end

		local status = result.status or 0

		if status == 401 or status == 403 then
			-- A server answered but refused us. Not a connection (reporting
			-- one while every request bounces would be a lie), and not the
			-- transport disconnect log either — the session did not "end".
			warnOnce(
				'secret',
				'the lest CLI refused this plugin (mismatched install)'
					.. ' — run `lest studio install`, then restart Studio to load the refreshed plugin'
			)
			return escalate(BACKOFF_MAX_SUSPECT)
		end

		if status == 204 then
			-- Long-poll expiry: a live session with no work for us. Normal.
			contacted()
			return POLL_IDLE
		end

		if status ~= 200 then
			-- Something answered, but not like the lest bridge — most likely
			-- a foreign server on this port. Never treat it as a connection
			-- and never fast-poll it: real requests against a real server we
			-- don't understand deserve the suspect ceiling.
			warnOnce(
				`status-{status}`,
				`the lest bridge port answered with unexpected HTTP status {status}`
					.. ' — is something else running on this port? see `[studio] port` in lest.toml'
			)
			return escalate(BACKOFF_MAX_SUSPECT)
		end

		local okDecode, job = pcall(deps.decode, result.body or '')
		if not okDecode or type(job) ~= 'table' or type(job.id) ~= 'string' or type(job.kind) ~= 'string' then
			-- A 200 whose body is not a lest job: a foreign server, or a CLI
			-- newer/older than this plugin. Suspect either way — warn once
			-- and back off rather than fast-poll (see the status branch).
			warnOnce(
				'bad-job',
				'the lest bridge port sent something this plugin cannot decode — a foreign server on'
					.. ' the port, or a CLI/plugin version mismatch; run `lest studio install` to update'
			)
			return escalate(BACKOFF_MAX_SUSPECT)
		end

		contacted()
		if job.kind == 'run' then
			return startRun(job)
		end
		answer(job)
		return POLL_IDLE
	end

	return { step = step }
end

return Bridge
