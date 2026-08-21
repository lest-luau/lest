--!strict
--- Pure-Luau JSON encoding for sentinel-framed protocol events, embedded into
--- bundles that run where no engine JSON service exists (gargantuan has no
--- HttpService). This is the twin of the spawned-runtime harness template's
--- inline encoder — same rules, same degradations, kept in step by hand: the
--- harness must stay a single self-contained file, so it cannot require this
--- module.

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

return { value = encodeJson }
