# Studio

The studio backend runs engine suites by **launching Roblox Studio** through
its official command-line interface: Lest bundles your specs, starts Studio
on your configured place with `--task RunScript`, waits for the run to
finish, and decodes the results from Studio's output file. Zero clicks —
no plugin to install, no permission prompts, nothing to set up beyond a
place to run against. Nothing is mocked; the tests run in the real engine.

```console
$ lest run engine --backend studio
```

```toml
[suites.engine]
include = ["tests/engine/**/*.spec.luau"]
backend = "cloud"           # CI stays on cloud
default = false
```

In CI, engine suites keep using the [cloud backend](backends.md#cloud) —
the studio backend launches the Studio application and refuses to run under
`$CI` on purpose.

## What a run looks like

1. Lest bundles the suite (the same bundling and `[place] rojo`
   delegation the cloud backend uses) and launches Studio on the place.
2. Studio boots, loads the place, executes the suite, writes its output,
   and quits itself.
3. Lest decodes the output: the same tree, diffs, snapshot behavior, and
   exit codes as every other backend.

Honest costs, stated plainly:

- **Every run pays a Studio boot** — typically 15–45 seconds before the
  suite even starts. The per-run budget allows 180 seconds for the boot on
  top of the per-spec timeouts.
- The suite runs against the **place you configured** — a built place file
  or a published place — never an unsaved session you happen to have open.
- Execution is Studio's **edit-mode** RunScript context: real engine APIs,
  real Instances and services, but not a running server and not a stepping
  playtest. See [Execution context](#execution-context) — cloud and studio
  genuinely differ here.
- Watch mode does not include studio suites (a boot per save is unusable).

## Execution context

The two engine backends run the same suite in genuinely different contexts,
and code that asks *where* it is running gets different answers:

|  | cloud | studio |
| --- | --- | --- |
| Runs as | A server script on a real game server | An edit-mode script, like the command bar |
| Permission level | GameScript (an ordinary server script) | The command bar's (plugin-level APIs work) |
| `RunService:IsStudio()` | `false` | `true` |
| `RunService:IsServer()` | `true` | `true` |
| `RunService:IsClient()` | `false` | `true` — an edit-mode quirk, see below |
| `RunService:IsRunning()` | `false` | `false` |
| `RunService:IsEdit()` | *throws* — plugin security | `true` |

**cloud** boots a fresh Roblox game server for each task and runs the bundle
as an ordinary server script. Server-only services behave as they do in
production — this is a real server, not an emulation of one. (It is its own
documented RunService context, though: Roblox's reference lists a "Luau
Execution" row, with `IsRunning()` false.)

**studio** executes in an *edit* session, at the same permission level as
Studio's command bar. Nothing is simulating: place scripts don't run, physics
doesn't step, and — an edit-mode quirk worth knowing — `IsClient()` **and**
`IsServer()` both return `true`, because an edit session acts as both
contexts at once. To tell the two backends apart, branch on
`RunService:IsStudio()`, never on `IsClient()`/`IsServer()` — and not on
`IsEdit()` either: it is a plugin-security method, so the same call that
returns `true` under studio *throws* at cloud's server permission.

In practice:

- Specs that exercise Instances, services, and the DataModel behave the same
  in both — which is what lets one engine suite run on studio locally and
  cloud in CI.
- **Server-only cloud services differ.** DataStores and friends work on cloud
  as on any game server. Under studio they follow edit-mode rules: the place
  must be *published* and have Studio API access enabled in Game Settings —
  and a local `[place] file` can never reach them, since an unpublished file
  has no universe to store into.
- **Plugin-level APIs differ the other way.** The command bar's permission
  level is higher than a game server's, so an API that requires plugin
  security works under studio and errors under cloud.

A spec that genuinely needs one context should branch on
`RunService:IsStudio()` — or live in a suite that only ever runs on the
backend that provides it.

## Choosing the place

The launch needs a place. In order of preference:

```toml
[place]
file = "test-place.rbxl"         # a built local place file (recommended)
```

or, for a published place:

```toml
[place]
universe_id = 1234567890
place_id = 9876543210
```

These are the same keys the cloud backend uses — one `[place]` block serves
both backends, which is the point: the same engine suite runs via studio
locally and via cloud in CI.

## Finding Studio

Lest looks for Studio in the platform's standard install location
(`%LOCALAPPDATA%\Roblox\Versions\...` on Windows, `/Applications` on
macOS). For non-standard installs:

```toml
[studio]
executable = "D:/Custom/RobloxStudioBeta.exe"
```

## Troubleshooting

- **The run times out with nothing decoded** — Studio may be sitting on a
  login screen or a modal dialog. Launch Studio by hand once, sign in, and
  re-run.
- **"exited without completing"** — the bundle failed to load; the error
  points at the kept output file, and Studio's own output is usually the
  fastest diagnosis.
- Lest keeps `.lest/studio-run.luau` and `.lest/studio-output.log` after a
  failure for inspection, and removes them after a success.
