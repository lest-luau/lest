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

export type Deps = {
	request: (options: RequestOptions) -> RequestResult,
	encode: (value: any) -> string,
	--- May throw on malformed input; the bridge calls it protected.
	decode: (text: string) -> any,
	log: (message: string) -> (),
	warn: (message: string) -> (),
	place: () -> { name: string, placeId: number },
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

	--- Answers one decoded job. Every known kind posts a `/result` addressed
	--- by the job's id; unknown kinds post a refusal that names the plugin
	--- version, so a newer CLI talking to an older plugin fails loudly with
	--- the fix visible (`lest studio install` again) instead of hanging.
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
		local postHeaders = headers()
		postHeaders['content-type'] = 'application/json'
		-- A failed result post means the session died mid-job; the next poll
		-- notices and handles the disconnect, so nothing to do here.
		deps.request({
			method = 'POST',
			url = url('/result'),
			headers = postHeaders,
			body = deps.encode(payload),
		})
	end

	local function step (): number
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
		answer(job)
		return POLL_IDLE
	end

	return { step = step }
end

return Bridge
