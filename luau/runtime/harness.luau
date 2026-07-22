--!strict
--- The spawned-runtime harness. This file is a *template*: the CLI rewrites the
--- three marked lines below and writes the result to `.lest/harness-<suite>`,
--- which is what lune/lute actually run. Edit this file, never the generated one.
---
--- Substitution is whole-line and keyed on the trailing marker comment, so each
--- placeholder line holds a *working default* rather than a magic token. That is
--- what keeps this file ordinary Luau: it parses, highlights, type-checks
--- against the real `lest/core` next door, and reformats like any other source
--- file. Two rules preserve that. Keep the marker comment at the end of the line
--- it belongs to — stylua may re-wrap the code before it, but a marker that
--- migrates to another line moves the substitution with it. And never spell a
--- complete marker in prose; the CLI matches by substring and would rewrite this
--- comment instead.
---
--- It loads the suite's specs inside the real runtime and streams protocol
--- events to stdout as sentinel-prefixed JSON lines, so test code printing to
--- stdout can never corrupt the stream. Each spec is reset, loaded, and run in
--- turn — mirroring the native backend's per-file isolation — and a spec
--- boundary marker before each lets the CLI attribute events (snapshots in
--- particular) back to the originating spec file.

local Lest = require('../core') -- __LEST_CORE_REQUIRE__

type Spec = { name: string, load: () -> () }

--- The suite's spec files, one entry per spec the CLI selected for the suite.
local specs: { Spec } = {} -- __LEST_SPECS__

--- Substring every test's full name must contain, or nil to run all of them.
local nameFilter: string? = nil -- __LEST_NAME_FILTER__

local SENTINEL = '@@LEST@@'
local SPEC_SENTINEL = '@@LEST_SPEC@@'

--- Replaces every byte that is not part of a valid UTF-8 sequence with U+FFFD
--- (the replacement character). Luau strings are byte arrays, but this stream
--- ends in a real JSON decoder: a raw invalid byte makes the sentinel line
--- undecodable, and one bad message becomes a tool error for the whole run.
--- The invariant, shared with the cloud sanitizer: degrade one string, never
--- the run. Valid multi-byte UTF-8 passes through untouched.
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

local function escapeJson (s: string): string
	s = toValidUtf8(s)
	return (
		s:gsub('[%c"\\]', function (c: string): string
			if c == '"' then
				return '\\"'
			elseif c == '\\' then
				return '\\\\'
			elseif c == '\n' then
				return '\\n'
			elseif c == '\r' then
				return '\\r'
			elseif c == '\t' then
				return '\\t'
			end
			return string.format('\\u%04x', string.byte(c))
		end)
	)
end

local function encodeJson (value: any): string
	local t = type(value)
	if t == 'nil' then
		return 'null'
	elseif t == 'boolean' then
		return tostring(value)
	elseif t == 'number' then
		-- JSON has no NaN or Infinity. On the cloud path a Luau table cannot
		-- hold nil, so `null` is unrepresentable there; `0` is the substitute
		-- every wire path can produce, so both paths emit `0` and backends
		-- agree on the shape of the same pathological event.
		if value ~= value or value == math.huge or value == -math.huge then
			return '0'
		end
		return tostring(value)
	elseif t == 'string' then
		return '"' .. escapeJson(value) .. '"'
	elseif t == 'table' then
		-- A table is an array only when its keys are exactly 1..#value: the
		-- ipairs walk below would silently drop any other key, so a mixed table
		-- (`{1, 2, foo = 'bar'}`) encodes as an object instead. An empty table
		-- encodes as an empty array — protocol objects always carry at least a
		-- `kind`/`type` field, so empty can only mean an empty array (e.g. a
		-- top-level test's path).
		local arrayLength = #value
		local keyCount = 0
		local contiguous = true
		for key in pairs(value) do
			keyCount += 1
			if type(key) ~= 'number' or key ~= math.floor(key) or key < 1 or key > arrayLength then
				contiguous = false
			end
		end
		if keyCount == 0 or (contiguous and keyCount == arrayLength) then
			local parts = {}
			for _, item in ipairs(value) do
				table.insert(parts, encodeJson(item))
			end
			return '[' .. table.concat(parts, ',') .. ']'
		end
		-- Object: every key stringified (JSON objects hold only string keys).
		-- Two distinct keys can stringify to the same text (1 and '1'), so a
		-- collision takes a ` #n` suffix rather than emitting duplicate JSON
		-- keys — the same rule as the cloud sanitizer.
		local parts = {}
		local taken: { [string]: boolean } = {}
		for key, item in pairs(value) do
			local keyString = tostring(key)
			if taken[keyString] then
				local suffix = 2
				while taken[`{keyString} #{suffix}`] do
					suffix += 1
				end
				keyString = `{keyString} #{suffix}`
			end
			taken[keyString] = true
			table.insert(parts, '"' .. escapeJson(keyString) .. '":' .. encodeJson(item))
		end
		return '{' .. table.concat(parts, ',') .. '}'
	end
	return '"<' .. t .. '>"'
end

local function emit (event: { [string]: any })
	print(SENTINEL .. encodeJson(event))
end

for index, spec in specs do
	-- Boundary marker: the CLI attributes subsequent events to this spec file.
	print(SPEC_SENTINEL .. tostring(index))
	Lest.reset()
	local ok, err = pcall(spec.load)
	if not ok then
		emit({
			kind = 'test_fail',
			path = { spec.name },
			name = '(load)',
			durationMs = 0,
			failure = { type = 'error', message = tostring(err), trace = '' },
		})
	else
		Lest.run(emit, { nameFilter = nameFilter })
	end
end
