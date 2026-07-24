# Studio

The studio backend runs engine suites by **launching Roblox Studio** through
its official command-line interface: lest bundles your specs, starts Studio
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

1. lest bundles the suite (the same bundling and `[settings] rojo`
   delegation the cloud backend uses) and launches Studio on the place.
2. Studio boots, loads the place, executes the suite, writes its output,
   and quits itself.
3. lest decodes the output: the same tree, diffs, snapshot behavior, and
   exit codes as every other backend.

Honest costs, stated plainly:

- **Every run pays a Studio boot** — typically 15–45 seconds before the
  suite even starts. The per-run budget allows 180 seconds for the boot on
  top of the per-spec timeouts.
- The suite runs against the **place you configured** — a built place file
  or a published place — never an unsaved session you happen to have open.
- Execution uses Studio's RunScript context: real engine APIs, real
  Instances and services, but not a stepping Run-mode playtest.
- Watch mode does not include studio suites (a boot per save is unusable).

## Choosing the place

The launch needs a place. In order of preference:

```toml
[cloud]
place_file = "test-place.rbxl"   # a built local place file (recommended)
```

or, for a published place:

```toml
[cloud]
universe_id = 1234567890
place_id = 9876543210
```

These are the same keys the cloud backend uses — one `[cloud]` block serves
both backends, which is the point: the same engine suite runs via studio
locally and via cloud in CI.

## Finding Studio

lest looks for Studio in the platform's standard install location
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
- lest keeps `.lest/studio-run.luau` and `.lest/studio-output.log` after a
  failure for inspection, and removes them after a success.
