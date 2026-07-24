# Studio

The **studio backend is under construction**: it will run engine suites in a
live Roblox Studio session — the same specs the cloud backend runs headless,
executed in a local playtest with results streaming back to your terminal.
It lands in stages; what works today is the **companion plugin and its
installer**, documented here. This page grows as the backend does.

The design in one paragraph: you keep Studio open with your place, like you
already do while developing. A small companion plugin — installed once,
described below — polls a loopback bridge that the lest CLI opens during a
run. When a studio-backend run starts, the CLI hands the plugin your bundled
specs, the plugin runs them in a playtest, and every event streams back into
the same reporters every other backend feeds. Nothing is mocked; the tests
run in the real engine. In CI, engine suites keep using the
[cloud backend](backends.md#cloud) — studio is for the local loop.

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
2. **Script Injection** — not used yet; when the studio backend's run
   support lands, this is how a run's test bundle enters the place.

Until a lest CLI session is running, the plugin does nothing but quietly
retry its local connection with a growing backoff — an idle Studio costs
nothing.

## Checking and removing

```console
$ lest studio status     # installed version, port, and file state
$ lest studio uninstall  # removes the plugin (only if lest wrote it)
```

`install` and `uninstall` refuse to touch a `lest.rbxmx` they don't
recognize as lest's own (`--force` overrides for install). Live-session
detection — "is Studio connected right now?" — arrives with the bridge.

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
