--!strict
--- The cloud collector: an emitter that BUFFERS protocol events instead of
--- printing them, so a CLI-generated entrypoint can `return` the buffer and let
--- the Open Cloud Luau-execution API ship it back as JSON in `output.results[0]`.
---
--- It implements the exact emitter shape `lest/core` drives — a bare
--- `(event: { [string]: any }) -> ()` function (core's `Emit` type), handed to
--- `Lest.run(emit, options)`. Because the collector only ever appends plain
--- tables, `lest/core` runs completely unchanged; the collector never compares,
--- decides, or prints (the host — the CLI — owns snapshot comparison and
--- pass/fail policy). Every event is passed through `sanitize` on the way in, so
--- the buffer is JSON-safe by construction (see sanitize.luau).
---
--- Usage (from the CLI-generated entrypoint):
---
---     local collector = Collector.new()
---     Lest.run(collector.emit, { nameFilter = nil })
---     return collector.events()   -- JSON-safe array, in emission order
---
--- `collector.emit` is a plain closure (no `self`), matching this codebase's
--- dot-call convention, so it can be passed directly as the emitter.

local sanitize = require('./sanitize')

export type Event = { [string]: any }

export type Tally = {
	passed: number,
	failed: number,
	skipped: number,
	--- True once a `run_end` event has been buffered.
	ended: boolean,
}

export type Collector = {
	--- The emitter to hand to `Lest.run`. Sanitizes and buffers each event.
	emit: (event: Event) -> (),
	--- The buffered events, in emission order. This is the array the entrypoint
	--- returns; the CLI reads it from `output.results[0]`. Returned by reference
	--- (not copied) — callers should treat it as read-only.
	events: () -> { Event },
	--- Recomputes pass/fail/skip counts by scanning the buffer. The generated
	--- entrypoint does *not* call this: the CLI drops every backend's own
	--- `run_start`/`run_end` and synthesizes exactly one pair per suite, so an
	--- in-engine `run_end` would be discarded on arrival. Kept because a host
	--- driving this collector without the CLI needs it to close its own stream.
	tally: () -> Tally,
	--- Drops every buffered event. Hosts rarely need this — a fresh collector
	--- per cloud task is cleaner — but it mirrors `Lest.reset`.
	reset: () -> (),
}

local Collector = {}

--- Creates a fresh collector with an empty buffer.
function Collector.new (): Collector
	local buffer: { Event } = {}

	local self = {} :: Collector

	function self.emit (event: Event)
		-- Sanitize on ingest, not on return, so the buffer can never hold a
		-- value that would later fail JSON encoding and lose the whole run.
		table.insert(buffer, (sanitize.value(event) :: any) :: Event)
	end

	function self.events (): { Event }
		return buffer
	end

	function self.tally (): Tally
		local passed, failed, skipped, ended = 0, 0, 0, false
		for _, event in buffer do
			local kind = event.kind
			if kind == 'test_pass' then
				passed += 1
			elseif kind == 'test_fail' then
				failed += 1
			elseif kind == 'test_skip' then
				skipped += 1
			elseif kind == 'run_end' then
				ended = true
			end
		end
		return { passed = passed, failed = failed, skipped = skipped, ended = ended }
	end

	function self.reset ()
		table.clear(buffer)
	end

	return self
end

return Collector
