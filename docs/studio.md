# Studio

The studio backend runs engine suites in a **live Roblox Studio session** —
the same specs the cloud backend runs headless, executed in a local playtest
with results streaming back to your terminal. Nothing is mocked; the tests
run in the real engine. In CI, engine suites keep using the
[cloud backend](backends.md#cloud) — studio is for the local loop, and it
refuses to run under `$CI` on purpose.

How it fits together: you keep Studio open with your place, like you already
do while developing. The companion plugin (installed once, below) polls a
loopback bridge the lest CLI opens during a run. A studio run bundles your
specs, hands them to the plugin, and the plugin injects them into the place
as a playtest suite.

## Running a suite

```console
$ lest run engine --backend studio
```

The CLI waits for your Studio session, then arms the suite and tells you:

1. **Arm** — the plugin injects the bundled specs and tries to start the
   playtest itself. Studio's API doesn't currently allow that, so expect
   step 2.
2. **Press Run (F8)** — both the terminal and Studio's output window say the
   suite is armed. Your Run press executes it.
3. **Results stream live** — the same tree, diffs, and snapshot behavior as
   every other backend, in your terminal as the playtest runs. When the
   suite finishes, the plugin tries to stop the playtest and cleans up its
   injected script (it will ask you to press Stop if the attempt fails).

`[settings] rojo` works exactly as it does for cloud: string requires mapped
to the open place delegate to the live instances. Snapshots compare and
store CLI-side, identical across backends. Timeouts follow the cloud rule
(a per-spec deadline inside the engine), with a generous fixed allowance
for the Run press on top.

Not yet: watch mode (each re-run would need its own Run press — an
armed-on-save design comes later), and print passthrough from test code
(only protocol events relay today).

## Installing the plugin

```console
$ lest studio install
```

This writes `lest.rbxmx` into your Roblox Studio local Plugins folder
(`%LOCALAPPDATA%\Roblox\Plugins` on Windows, `~/Documents/Roblox/Plugins` on
macOS). There is nothing to download and nothing from the Creator Store: the
plugin ships inside the lest binary, and the installer stamps what it wrote
so upgrades and repairs are automatic — re-run `lest studio install` after
updating lest and the plugin follows.

The first time the plugin is active in Studio you'll see one or two one-time
permission prompts:

1. **HTTP requests** — the plugin talks to the lest CLI on `127.0.0.1` and
   nowhere else. Allow it under *Plugin Management* when Studio asks.
2. **Script Injection** — how a run's bundled specs enter the place: the
   plugin writes them as a temporary Script and removes it after the run.

Until a lest CLI session is running, the plugin does nothing but quietly
retry its local connection with a growing backoff — an idle Studio costs
nothing.

## Checking and removing

```console
$ lest studio status     # install state, plus a live-session check
$ lest studio uninstall  # removes the plugin (only if lest wrote it)
```

`status` also probes for a live session: it briefly opens the bridge port
and waits a few seconds for the plugin to poll. With Studio open (and the
HTTP permission granted) it reports the place and plugin version:

```console
$ lest studio status
Installed at C:\...\Plugins\lest.rbxmx (lest 0.3.0, port 28806).
Live session: "My Game" (place 12345), plugin 0.3.0.
```

With two Studio instances open, `status` reports whichever answered first.

`install` and `uninstall` refuse to touch a `lest.rbxmx` they don't
recognize as lest's own (`--force` overrides for install).

## The bridge port

The plugin and CLI meet on a loopback port, `28806` by default. Override it
if something else owns that port:

```toml
[studio]
port = 41999
```

or per install with `lest studio install --port 41999`. The port is baked
into the installed plugin, so changing it means re-running the install;
re-installs without a port keep whatever the existing install used.

## Scope and honesty

- Studio must be open with your place for studio runs to work; that
  persistent session is what will make the loop fast (no per-run boot).
- Playtests run in **Run mode** (server simulation). Tests that need a
  `Player` and character are out of scope — that fidelity line is the same
  one the [no-emulation rule](backends.md) draws everywhere else.
- The studio backend will never be a CI backend; cloud owns CI.
