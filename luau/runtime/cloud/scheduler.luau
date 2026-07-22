--!strict
--- Task-scheduler integration. `lest/core`'s runner calls each test body
--- synchronously, so a test that yields (`task.wait`, waiting on a signal, a
--- DataStore/HTTP round-trip) yields `Lest.run` itself — which therefore has to
--- execute on a real scheduler thread. This is the one place where the engine's
--- genuine async semantics actually live; there is no emulation here.
---
--- `runSuite` drives a caller-supplied thunk (typically `function() Lest.run(...)
--- end`) to completion under the real task scheduler, with a wall-clock deadline
--- that mirrors the native backend's per-test timeout *intent*:
---
---   * The thunk is started with `task.spawn`, which runs it synchronously up to
---     its first yield. A purely synchronous suite finishes right there and the
---     wait loop below never runs — zero overhead for non-async tests.
---   * If it yields, the scheduler is pumped cooperatively (`task.wait`) until
---     the thunk finishes or the deadline elapses.
---   * On deadline the still-suspended thread is abandoned (`task.cancel`) so one
---     async test that never resumes cannot hang the whole cloud task. Every
---     event emitted *before* the hang is already safe in the collector's
---     buffer, so partial results survive.
---
--- The engine `task` library is passed in (`options.task`), not read from the
--- global, so this package carries no dependency on an engine-only global and
--- stays lint-clean and unit-testable under plain-Luau tooling. The in-engine
--- entrypoint the CLI generates simply passes the real `task` global.
---
--- Honest limitation (consistent with lest's "no emulation" rule): pure Luau has
--- no preemption. A test stuck in a tight, non-yielding loop never returns from
--- `task.spawn`, so this watchdog cannot fire for it — only the native backend's
--- VM interrupt can preempt that. The deadline here catches the realistic engine
--- failure mode: an async wait that never completes. Drive per spec file (or per
--- test, by wrapping narrowly) for finer-grained deadlines.

local Scheduler = {}

local DEFAULT_DEADLINE_MS = 30000

--- The slice of the Roblox `task` library this module needs. The entrypoint
--- passes the real `task` global, which satisfies this shape.
export type EngineTask = {
	spawn: (functionOrThread: () -> (), ...any) -> thread,
	wait: (duration: number?) -> number,
	cancel: (thread: thread) -> (),
}

export type Options = {
	--- The engine task library — pass the `task` global. Required: driving the
	--- real scheduler is the whole point, but it is injected rather than read
	--- from a global so the package depends on no engine-only identifier.
	task: EngineTask,
	--- Wall-clock budget for the thunk, in milliseconds. Default 30000.
	deadlineMs: number?,
}

export type RunResult = {
	--- True if the deadline elapsed before the thunk finished.
	timedOut: boolean,
	--- Wall-clock time actually spent, in milliseconds.
	durationMs: number,
	--- The thunk's error message if it raised, else nil. A timeout is reported
	--- via `timedOut`, not here. The generated in-engine entrypoint reports it
	--- as a failure for the spec rather than dropping it — a raise here means
	--- the run never produced its own events, so silence would read as a spec
	--- that simply had no tests.
	error: string?,
}

--- Runs `run` under the task scheduler with a deadline. Never raises — a thrown
--- error from `run` is captured into `RunResult.error`.
function Scheduler.runSuite (run: () -> (), options: Options): RunResult
	local task = options.task
	local deadlineMs = options.deadlineMs or DEFAULT_DEADLINE_MS
	local deadlineSec = deadlineMs / 1000
	local started = os.clock()

	local done = false
	local failure: string? = nil

	local thread = task.spawn(function ()
		local ok, err = pcall(run)
		if not ok then
			failure = tostring(err)
		end
		done = true
	end)

	-- Skipped entirely when `run` never yielded (it already set `done` above).
	while not done and (os.clock() - started) < deadlineSec do
		task.wait()
	end

	local durationMs = (os.clock() - started) * 1000

	if not done then
		-- Suspended past its deadline: abandon it. `task.cancel` throws if the
		-- thread is not cancellable, so guard it.
		pcall(task.cancel, thread)
		return { timedOut = true, durationMs = durationMs, error = failure }
	end

	return { timedOut = false, durationMs = durationMs, error = failure }
end

return Scheduler
