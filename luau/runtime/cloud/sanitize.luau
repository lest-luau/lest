--!strict
--- Deep-copies a protocol event into a strictly JSON-safe table.
---
--- The cloud flow returns the buffered events straight out of the Open Cloud
--- Luau-execution task, where the platform JSON-encodes the return value into
--- `output.results[0]`. That encoder (and `HttpService:JSONEncode` in Studio)
--- throws on functions, threads, userdata (Instances!), non-finite numbers,
--- and strings holding invalid UTF-8. One bad value would lose the *entire*
--- run, so every event is sanitized on the way into the collector's buffer,
--- by construction:
---
---   * boolean                — kept as-is.
---   * string                 — kept, except that invalid UTF-8 byte sequences
---                              are replaced with U+FFFD (Luau strings are byte
---                              arrays; the platform encoder rejects invalid
---                              UTF-8, and one bad string must degrade alone,
---                              never the run).
---   * number                 — kept, unless NaN / +inf / -inf, which become 0
---                              (see the comment at the coercion site;
---                              durationMs is the only number core emits, so a
---                              finite fallback is harmless and stays a number).
---   * table                  — deep-copied; a contiguous array stays an array,
---                              anything else becomes an object with *every*
---                              key stringified; cycles collapse to "<cycle>"
---                              and depth beyond MAX_DEPTH collapses to a string.
---   * function/thread/Instance/… — replaced with `tostring(value)` (an
---                              Instance stringifies to its name), never dropped.
---
--- Object keys are all stringified because a JSON object holds only string
--- keys: keeping finite number keys as numbers produced a Luau table mixing
--- number and string keys, which the platform encoder cannot represent as one
--- object. Two distinct keys can stringify to the same text (1 and "1"), so a
--- collision takes a ` #n` suffix instead of silently overwriting the entry
--- already there. Core only ever emits string keys and contiguous
--- integer-array keys, so the collision path never runs in practice — it is
--- the insurance that keeps a whole cloud run from being unrecoverable.

local sanitize = {}

-- Deep but bounded: protocol events nest only a few levels (failure objects,
-- string-array paths), so this ceiling only ever trips on a pathological value.
local MAX_DEPTH = 16

--- Replaces every byte that is not part of a valid UTF-8 sequence with U+FFFD
--- (the replacement character). Luau strings are byte arrays, but this data
--- ends in a real JSON encoder: a raw invalid byte makes the platform encoder
--- throw, and one bad string would lose the entire task result. The invariant,
--- shared with the spawned-runtime harness: degrade one string, never the run.
--- Valid multi-byte UTF-8 passes through untouched.
local REPLACEMENT = utf8.char(0xFFFD)

local function toValidUtf8 (s: string): string
	-- Fast path: `utf8.len` validates the whole string in one pass.
	if utf8.len(s) ~= nil then
		return s
	end
	local parts = {}
	local index = 1
	while index <= #s do
		local remaining, invalidAt = utf8.len(s, index)
		if remaining ~= nil then
			-- Everything from `index` to the end is valid.
			table.insert(parts, string.sub(s, index))
			break
		end
		-- Bytes before the reported position are valid; the byte there is not,
		-- so it degrades alone and scanning resumes right after it.
		local bad = invalidAt :: number
		if bad > index then
			table.insert(parts, string.sub(s, index, bad - 1))
		end
		table.insert(parts, REPLACEMENT)
		index = bad + 1
	end
	return table.concat(parts)
end

local function safeString (value: any): string
	local ok, result = pcall(tostring, value)
	if ok then
		-- A `__tostring` metamethod can return arbitrary bytes, so even this
		-- fallback goes through the UTF-8 repair.
		return toValidUtf8(result)
	end
	return `<{typeof(value)}>`
end

local function isFinite (value: number): boolean
	return value == value and value ~= math.huge and value ~= -math.huge
end

local function recurse (value: any, depth: number, seen: { [any]: boolean }): any
	local t = type(value)
	if t == 'string' then
		return toValidUtf8(value)
	elseif t == 'boolean' then
		return value
	elseif t == 'number' then
		-- JSON has no NaN or Infinity. On the cloud path a Luau table cannot
		-- hold nil, so `null` is unrepresentable there; `0` is the substitute
		-- every wire path can produce, so both paths emit `0` and backends
		-- agree on the shape of the same pathological event.
		if not isFinite(value) then
			return 0
		end
		return value
	elseif t == 'table' then
		if depth >= MAX_DEPTH then
			return safeString(value)
		end
		-- Backtracking cycle guard: cleared on the way out so a value shared by
		-- two sibling branches (a DAG) is copied twice, not falsely flagged.
		if seen[value] then
			return '<cycle>'
		end
		seen[value] = true
		-- A table is an array only when its keys are exactly 1..#value; those
		-- copy key-for-key so the platform encoder still emits a JSON array
		-- (protocol `path` fields depend on it). Anything else becomes an
		-- object.
		local arrayLength = #value
		local keyCount = 0
		local contiguous = true
		for key in pairs(value) do
			keyCount += 1
			if type(key) ~= 'number' or key ~= math.floor(key) or key < 1 or key > arrayLength then
				contiguous = false
			end
		end
		if contiguous and keyCount == arrayLength then
			local out = table.create(arrayLength)
			for index = 1, arrayLength do
				out[index] = recurse(value[index], depth + 1, seen)
			end
			seen[value] = nil
			return out
		end
		-- Object: every key stringified — a JSON object holds only string keys,
		-- and a Luau table mixing number and string keys is unencodable as one
		-- object. String keys get the same UTF-8 repair as string values (a raw
		-- invalid byte in a key fails the encode exactly like one in a value);
		-- everything else goes through `safeString`, so a NaN / infinite number
		-- key is as safe as any other. Keys already used in `out` are tracked
		-- so a stringified key that collides with another takes a ` #n` suffix
		-- rather than overwriting it.
		local out: { [string]: any } = {}
		local taken: { [string]: boolean } = {}
		for key, item in pairs(value) do
			local keyString = if type(key) == 'string' then toValidUtf8(key) else safeString(key)
			if taken[keyString] then
				local suffix = 2
				while taken[`{keyString} #{suffix}`] do
					suffix += 1
				end
				keyString = `{keyString} #{suffix}`
			end
			taken[keyString] = true
			out[keyString] = recurse(item, depth + 1, seen)
		end
		seen[value] = nil
		return out
	end
	-- function, thread, userdata (Instances, vectors, buffers), etc.
	return safeString(value)
end

--- Returns a JSON-safe deep copy of `value`. The input is never mutated.
function sanitize.value (value: any): any
	return recurse(value, 0, {})
end

return sanitize
