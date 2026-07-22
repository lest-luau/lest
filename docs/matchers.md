# Matchers

`expect(value)` returns an expectation carrying every matcher below. Matchers
are plain dot-calls, and each returns the expectation, so they chain:

```luau
expect(name).toBeA('string').toMatch('^user_')
```

Negations are spelled `toNot*`. `not` is a Luau keyword, so Jest's `.not.toBe`
isn't available — `toNotBe` is the equivalent.

A failing matcher raises a structured assertion failure, which the reporter
renders as `Expected:` and `Received:` lines (or the failure message alone,
for matchers like `toBeTruthy` that have no meaningful pair). Diffs are
rendered for snapshot mismatches only. Deeply nested values are truncated in
these messages to keep them readable — that's message formatting only, never
what was compared.

## Equality

### `toBe(expected)` · `toNotBe(expected)`

Identity — Luau's `==`. Two tables are `toBe` only if they are the same table.

```luau
expect(1 + 1).toBe(2)
expect(cache.get('k')).toBe(cache.get('k'))   -- same reference
expect({}).toNotBe({})                        -- different tables
```

### `toEqual(expected)` · `toNotEqual(expected)`

Deep structural equality. Tables compare by contents, recursively — this is
what you want for most table assertions.

```luau
expect(parse('a=1')).toEqual({ a = 1 })
expect({ 1, { 2 } }).toEqual({ 1, { 2 } })
```

The one place `toEqual` diverges from `==` on non-tables: **NaN is equal to
NaN**, matching Jest. `toBe` keeps plain Luau `==` semantics, where
`NaN ~= NaN`. Both agree that `-0 == 0`.

```luau
expect(0 / 0).toEqual(0 / 0)   -- passes: toEqual treats NaN as equal to NaN
expect(0 / 0).toNotBe(0 / 0)   -- passes: toBe is plain ==, and NaN ~= NaN
```

## Truthiness

### `toBeTruthy()` · `toBeFalsy()`

Luau truthiness: everything except `false` and `nil` is truthy. Note that `0`
and `''` are **truthy** in Luau, unlike in JavaScript.

```luau
expect(0).toBeTruthy()
expect(nil).toBeFalsy()
```

### `toBeNil()` · `toNotBeNil()`

Specifically `nil`, rather than merely falsy.

```luau
expect(map.missing).toBeNil()
expect(map.present).toNotBeNil()
```

## Types

### `toBeA(typeName)`

A `typeof` check, so it understands Roblox types (`'Instance'`, `'Vector3'`,
`'CFrame'`) as well as the Luau primitives.

```luau
expect(42).toBeA('number')
expect(workspace).toBeA('Instance')
```

## Strings and collections

### `toContain(item)`

For a **string**, a plain substring check (not a pattern). For an **array**, it
passes when any element is deep-equal to `item` — reference equality is rarely
what a Luau test means.

```luau
expect('hello world').toContain('lo wo')
expect({ 'a', 'b' }).toContain('b')
expect({ { id = 1 } }).toContain({ id = 1 })   -- deep, not by reference
```

Anything that is neither a string nor a table is a matcher misuse and fails
saying so.

### `toHaveLength(length)`

`#value` for strings and tables.

```luau
expect('abc').toHaveLength(3)
expect({ 1, 2, 3 }).toHaveLength(3)
```

### `toMatch(pattern)`

A **Luau string pattern** match — not a plain substring, and not a regex. Use
`toContain` when you want a literal. Both the value and the pattern must be
strings; anything else is a matcher misuse and fails saying so.

```luau
expect(id).toMatch('^user_%d+$')
```

## Numbers

### `toBeGreaterThan(expected)` · `toBeGreaterThanOrEqual(expected)`
### `toBeLessThan(expected)` · `toBeLessThanOrEqual(expected)`

Ordering comparisons. Both sides must be two numbers or two strings; a mismatch
fails as a matcher misuse rather than silently comparing something meaningless.

```luau
expect(elapsed).toBeLessThan(100)
expect(score).toBeGreaterThanOrEqual(0)
```

### `toBeCloseTo(expected, precision?)`

Floating-point comparison: passes when `|value - expected| < 10^-precision / 2`.
`precision` defaults to `2`, matching Jest. `NaN` is never close to anything,
including itself, and a non-number `expected` is a matcher misuse that fails
with a clear message rather than a raw arithmetic error.

```luau
expect(0.1 + 0.2).toBeCloseTo(0.3)
expect(math.pi).toBeCloseTo(3.14159, 5)
```

## Errors

### `toThrow(substring?)` · `toNotThrow()`

The value must be a **function**, which the matcher calls in protected mode.
With no argument, `toThrow` passes if the call raises at all; with a substring,
the error message must contain it.

```luau
expect(function ()
	parse('{')
end).toThrow('unexpected end of input')

expect(function ()
	parse('{}')
end).toNotThrow()
```

Passing a non-function is a matcher misuse — `expect(parse('{'))` would have
raised while building the expectation, before any matcher ran. So is passing a
non-string as the substring: it fails as a clear assertion rather than a raw
`string.find` error.

## Snapshots

### `toMatchSnapshot(hint?)`

Serializes the value and compares it against the stored snapshot for this test,
writing it on first run. The optional `hint` distinguishes multiple snapshots
within one test.

```luau
expect(render(doc)).toMatchSnapshot()
expect(render(doc, { compact = true })).toMatchSnapshot('compact')
```

The framework only reports the serialized value; the comparison, the diff, and
the update happen in the CLI. Serialization is lossless — no depth cap — and a
value that can't serialize deterministically (a cycle, or a table/function key
anywhere inside it) fails the assertion rather than degrading the snapshot. See
**[Snapshots](snapshots.md)**.

## Quick reference

| Matcher | Negation | Notes |
| --- | --- | --- |
| `toBe(expected)` | `toNotBe` | Identity (`==`) |
| `toEqual(expected)` | `toNotEqual` | Deep structural equality; NaN equals NaN |
| `toBeTruthy()` | — | Everything but `false`/`nil` |
| `toBeFalsy()` | — | Only `false` and `nil` |
| `toBeNil()` | `toNotBeNil` | Specifically `nil` |
| `toBeA(typeName)` | — | `typeof` check |
| `toContain(item)` | — | Substring, or deep-equal array element |
| `toHaveLength(length)` | — | `#value` |
| `toMatch(pattern)` | — | Luau pattern |
| `toBeGreaterThan(expected)` | — | Also `…OrEqual` |
| `toBeLessThan(expected)` | — | Also `…OrEqual` |
| `toBeCloseTo(expected, precision?)` | — | Precision defaults to `2` |
| `toThrow(substring?)` | `toNotThrow` | Value must be a function |
| `toMatchSnapshot(hint?)` | — | See [Snapshots](snapshots.md) |
