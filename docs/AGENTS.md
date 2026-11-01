# AGENTS.md — SoundSync

Guidance for AI coding agents (and people) working on this repository: what the project is, how to build
and test it, how it is put together, the rules the owner works by, and the decisions and history behind the
current code. Read it before changing anything. The user-facing documentation is [`README.md`](../README.md);
detailed designs are in [`docs/superpowers/`](superpowers/).

## Project overview

SoundSync is LAN multi-room audio written in Rust.

- **Windows** PCs can be a **master** (broadcast their audio) and/or a **receiver**; **macOS** is receive-only.
- Audio is lossless PCM, in the source's own sample rate and channel layout (up to 7.1).
- Every node, including the master, schedules each sample so that all speakers emit it at the same moment.
  Each node also compensates its own output delay (HDMI TVs, AV receivers, Bluetooth).
- **Master modes:**
  - **VB-CABLE** (default, full sync): apps play to the virtual *CABLE Input*, which SoundSync captures.
  - **Direct** (opt-in, lowest latency, partial sync): SoundSync captures a real output.
- Discovery is mDNS. Packets are authenticated with an HMAC keyed by the group, and audio goes unicast to
  subscribers.
- Calibration: per-node extra latency (edited by hand, a click sync test, or microphone sweeps), plus volume
  matching.
- The app: egui window plus tray, autostart, single instance, and a `config.toml` per node.

## Repository layout

| Path | What |
|---|---|
| `crates/core` (`soundsync-core`) | Pure logic, no OS dependencies, heavily unit-tested. Covers: <br>• `protocol` (wire format, HMAC, validation) <br>• `clock` (peer clock estimation) <br>• `jitter` <br>• `player` (per-stream pipeline: jitter → PI timing → rubato resampler → layout map) <br>• `sync` (PI controller) <br>• `delay` (group delay policy) <br>• `layout` <br>• `limiter` <br>• `calib` (sweeps, detection, levels, results) <br>• `config` <br>• `paths` <br>• `timeline` |
| `crates/audio` (`soundsync-audio`) | The `AudioHost` trait and its backends: <br>• `win/` (WASAPI via `wasapi` 0.26 + `windows` 0.62) <br>• `mac/` (CoreAudio via `coreaudio-rs` / `objc2-core-audio`) <br>• `fake.rs` (virtual clocks, devices, rooms and mics for tests) <br>• `null.rs` (timer-driven discard output) <br>• `select.rs` (device-name resolution) <br>• `measure.rs` (loopback diagnostics) |
| `crates/engine` (`soundsync-engine`) | A headless node. Covers: <br>• `net` (UDP socket and receive thread) <br>• `discovery` (mDNS) <br>• `peers` <br>• `node` (engine thread, commands, status) <br>• `streams` (output, capture, slots, gossip, group delay) <br>• `sender` / `receiver` (capture → packets; render mixer) <br>• `calibration`, `synctest`, `volume` <br>• `devices` (hot-plug and reopen) <br>• `defaults` (Windows default-output switching) <br>• `power` (sleep detection) |
| `crates/app` (`soundsync` binary) | CLI (`cli.rs`), GUI (`ui/`: `app.rs`, `panels.rs`, `format.rs`, `tray.rs`, `vbcable.rs`, `config_watch.rs`), logging, single instance, autostart, Windows session end, macOS bundle metadata, icon (`icon.rs`, gold "Sparkle" style). |
| `crates/engine/tests/` | Integration tests: several in-process nodes over 127.0.0.1 on `FakeHost`s (helpers in `common/mod.rs`). |
| `scripts/` | `build-windows.ps1` (→ `dist/SoundSync-windows-x64.zip`), `install-windows.ps1`, `uninstall-windows.ps1`, `bundle-macos.sh`, `publish-release.sh` and `release-notes.sh` (CI release). |
| `.github/workflows/build.yml` | CI on `windows-latest` and `macos-latest`: test (one at a time), clippy `-D warnings`, package, upload artifacts. On pushes to `main` that pass, a `release` job publishes a GitHub release with both zips. |
| `docs/superpowers/specs/`, `docs/superpowers/plans/` | Original design and 27-task plan (2026-10-30/31), and the volume-matching design (2026-10-31). |
| `.superpowers/sdd/2026-10-31-soundsync/` | **Local only** (git-ignored by its own `.gitignore`): the build ledger `progress.md` and PowerShell helper scripts (see [Real-hardware testing](#real-hardware-testing)). |

## Build, test and run

Toolchain: stable Rust (1.99), MSVC on Windows, edition 2024, workspace resolver 3. No other installs are
needed.

```sh
cargo build --release -p soundsync                    # target/release/soundsync(.exe)
cargo test --workspace                                # ~230 tests, about a minute after building
SS_FAKE_MIN_SLEEP_MS=20 cargo test --workspace        # same, with coarse timers like GitHub's macOS runners
cargo clippy --workspace --all-targets                # must be clean (CI uses -D warnings)
cargo clippy --workspace --all-targets --target aarch64-apple-darwin   # cross-checks the macOS code from Windows (no SDK needed)
cargo test -p soundsync-audio -- --ignored            # hardware tests (silent "Steam Streaming Speakers" endpoint)
pwsh scripts/build-windows.ps1 [-SkipTests]           # release zip in dist/
bash scripts/bundle-macos.sh                          # on macOS: universal .app zip in dist/
```

CI runs the tests **one at a time** (`RUST_TEST_THREADS=1` in `build.yml`). Locally they run in parallel,
which is fine on a machine with many cores.

**Versions and releases:**

- **The version label** comes from `soundsync_core::version::version()`.
  - CI sets `SOUNDSYNC_BUILD` (the workflow run number) and `SOUNDSYNC_COMMIT` (the short SHA) at compile
    time, which gives "0.1.0 (build 42, fcaec3d)". Local builds say "0.1.0 (dev)".
  - The label appears in Settings, the log's first line, `soundsync --version`, the HELLO handshake (peers
    show it on hover in the group table) and the mDNS TXT record.
  - The macOS bundle's `CFBundleVersion` is the build number.
- **Every push to `main`** that passes on both systems is published by `scripts/publish-release.sh`:
  - Tag `v<workspace version>-build.<run number>`, title "SoundSync 0.1.0 (build 42)", marked Latest.
  - Assets: `SoundSync-windows-x64.zip` and `SoundSync-macos-universal.zip`.
  - Notes come from `scripts/release-notes.sh`: commit subjects and bodies since the previous release,
    without trailers. Preview them locally with
    `bash scripts/release-notes.sh v0.1.0-build.0 <previous tag or commit> https://github.com/<owner>/<repo>`.
  - A newer push cancels an older run still in progress, so releases never come out of order.
  - Re-running a build updates its release.
  - Bump `version` in the workspace `Cargo.toml` for a new release line. Don't create `v*-build.*` tags by
    hand.

Before calling a change done, run these, all green:

1. The full suite, normally and with `SS_FAKE_MIN_SLEEP_MS=20`.
2. Clippy for both targets.

The project does **not** use rustfmt: there is no `rustfmt.toml`, lines run to about 160 characters, and CI
doesn't check formatting. Never run `cargo fmt` on the tree; match the surrounding style by hand.

Useful runtime commands:

- **`soundsync devices`**: lists endpoints, formats, layouts and periods.
- **`soundsync tone --device X [--pulses]`**: plays a test tone, or clicks with `--pulses`.
- **`soundsync measure`**: loopback timing measurements.
- **`soundsync run [--broadcast] [--bind IP] [--no-mdns] [--peer host:port] [--output null]`**: a headless node.
- **`soundsync --show`**: opens the GUI.
- **`soundsync --quit`**: asks a running instance to quit cleanly.
- **`soundsync --instance b ...`**: a separate instance with its own config, logs and state (`%APPDATA%\SoundSync-b\`); give it another `network.port`.
- **`--bind 127.0.0.1 --no-mdns`** (GUI and `run`): keeps tests off the LAN and away from the firewall prompt.
- **Debug logging:** `RUST_LOG="info,soundsync_engine::calibration=debug,wgpu_hal=warn,wgpu_core=warn,egui_wgpu=warn,naga=warn"`. Calibration debug lines show every sweep's arrival, SNR, level and peak, plus the mic level before the first sweep.

Paths:

| What | Windows | macOS |
|---|---|---|
| Config | `%APPDATA%\SoundSync\config.toml` | `~/Library/Application Support/SoundSync/config.toml` |
| Logs (`soundsync.log` is the current session; 5 kept; crashed sessions get `-recovered`) | `%LOCALAPPDATA%\SoundSync\logs` | `~/Library/Logs/SoundSync` |
| State (defaults saved before switching) | `%LOCALAPPDATA%\SoundSync\state.toml` | `~/Library/Application Support/SoundSync/state.toml` |
| Installed exe | `%LOCALAPPDATA%\Programs\SoundSync\soundsync.exe` | `/Applications/SoundSync.app` |

## Architecture

### Threads (per node)

- **Network RX** (`net.rs`): reads into a 64 KiB buffer. It decodes and verifies each packet:
  - Audio goes into each stream's `rtrb` ring.
  - Control messages go to the engine thread.
  - Packets from another protocol version are recorded in `RxCounters::other_version`.
- **Engine** (`node.rs`): runs on a 10 ms ticker. It handles HELLO, clock pings, peers, commands, gossip,
  delay policy, calibration and device events. It publishes a `Status` every 100 ms through an `ArcSwap`.
  The UI only reads `Status` and sends `Command`s.
- **Capture** (Windows master, `sender.rs`): WASAPI loopback with MMCSS. It packetizes audio and sends it
  unicast to each subscriber's handshake address.
- **Render** (`receiver.rs` `Mixer`): drains the rings and runs each stream's `StreamPlayer`. It sums the
  streams, adds scheduled signals (sweeps, clicks, wake-up noise), and applies the software volume and the
  limiter.
  - It renders zeros when idle.
  - No locks and no allocations: slots and signals arrive and leave as `Box`es through rings.
- **UI thread** (eframe 0.37): `logic()` runs even while the window is hidden. A 500 ms ticker and the tray
  events call `request_repaint`.

### Timing model

- A sample reaches the listener at
  `local(capture_time) + D − extra_latency(node)`, where D is the presentation offset carried in each
  `AudioHeader`.
- **Clocks:** NTP-style pings every 250 ms (50 ms for the first 2 s) feed a min-RTT regression over 128
  samples. The estimate restarts when a peer's clock steps, for example when a Mac sleeps.
- **Controller** (`sync.rs`): a PI loop on the 0.2 s low-passed error.
  - Gains: kp 2000 ppm/ms, ki 1000 ppm/(ms·s).
  - Deadband 0.1 ms; ratio capped at ±300 ppm.
  - A hard resync with a fade happens above 15 ms of error.
  - A realign keeps the integrator (`reset_error`), because drift belongs to the DAC. Only a new stream
    resets it fully.
  - The player realigns when the DAC timeline jumps or the offset changes by more than 1 ms.
- **Group delay D** (`delay.rs`, virtual mode):
  - Auto D is the largest requirement any node reports, plus margin, in 5 ms steps, between 20 ms and 2 s.
  - It **only increases** while a stream runs. It drops back at a stream restart or by manual setting.
  - Requirements are offset-independent: arrival delay + device lead + period + resampler lookahead + extra.
  - Before a receiver has measured anything, it estimates lead window + period + 20 ms headroom, and it
    remembers its last measured requirement until its output reopens.
  - D counts every peer that plays remote streams (`plays_remote`), subscribed yet or not.
- **Direct mode:** D = the master's output path plus its extra latency. Receivers too slow for that play as
  soon as they can and report "Behind N ms".

### Network protocol (`core/src/protocol.rs`)

- **Datagram layout:** `[SSYN][version][type][sender u64][body][HMAC-SHA256 tag, 8 bytes]`.
  - Datagrams are at most 1472 B; audio payload at most 1400 B.
  - The key is SHA-256 of `soundsync-v1|group|secret`.
- **Types:**
  - Control: postcard-encoded `Control`.
  - Audio: a fixed 49-byte header followed by raw f32 LE samples.
  - Silent: header only, sent after 2 s of digital silence.
- **`PROTOCOL_VERSION` = 2.** Bump it for **any** wire change, because nodes on different versions can't
  talk. A mismatching datagram raises the warning "The SoundSync at <ip> runs a different version". Such
  datagrams also count as proof that inbound packets get through, so they don't trigger the firewall
  warning.
- **Validate** every new numeric field from the network in `control_ok` or `read_audio_header`, and test it
  in `out_of_range_formats_and_values_are_rejected`. Examples:
  - Formats: 8–384 kHz, 1–32 channels.
  - Volume within 0..=1; offsets within ±10 s.
  - Turn-down within 0..=60 dB; levels finite.
- **Messages:**
  - Hello, Ping/Pong.
  - State (1 s gossip). `NodeState` carries role, stream, output, extra latency, required delay,
    `plays_remote`, volume, mute and `device_volume`.
  - Subscribe/Unsubscribe: leases with a 3 s TTL, accepted only from the peer's handshake address.
  - SetOffset.
  - SetVolume (`VolumeChange::Set | Muted | TurnDown{db}`).
  - ListInputs → `Inputs(InputList)`, trimmed with `control_fits` so the reply fits one datagram.
  - `Calib(Request | Begin | Play{Sweep|Click|Wake} | End | Result)`.
  - SyncTest.
- **Admission:** HELLO is accepted only from loopback, a local LAN subnet, or a static peer. mDNS only
  announces on physical LAN interfaces; virtual adapters (Hyper-V, WSL, Docker, VPN, Tailscale) are
  excluded.

### Calibration (`core/src/calib.rs`, `engine/src/calibration.rs`)

1. The coordinator, which is the node with the mic, opens the mic and pauses stream playback everywhere.
2. Every speaker plays a soft wake-up noise at +0.2 s, for 1 s, at 0.03 peak. TVs and soundbars mute after
   silence.
3. From +2 s, the speakers take turns, 1 s apart: 3 sweeps each, or 1 for a Check. Each sweep is 100 ms, from
   200 Hz to 8 kHz, at amplitude 0.5.
4. **Detection:** deconvolution over the sweep's band with a Hann taper, then a Hilbert envelope. It takes the
   **earliest** local maximum at ≥ −10 dB of the peak and 20 dB above noise, so a louder later reflection
   doesn't win.
5. **Level:** the recording's energy over the sweep length (+20 %), minus the noise power measured before the
   arrival. This gives pink-like weighting. The peak is also recorded.
6. **Results:** per speaker, the median residual against the reference and the median level relative to the
   quietest speaker.
   - Reference: the median node in virtual mode, the master in direct mode.
   - `CalibResult::problems()` flags: sweeps not heard; spread over 1 ms; SNR under 30 dB; peak over −6 dBFS
     (overload); level spread over 2 dB (a mic with automatic gain).
7. **Apply** adds the deltas to each node's extra latency (SetOffset). With "Also match volumes", it also
   sends `TurnDown{level_db}` (at least 0.5 dB) to every louder speaker. Check / "Correct these" repeats this
   with one sweep.
8. Any node can trigger a run on any node's mic: `CalibMsg::Request`, with mics listed through `ListInputs`.

### Volume (`engine/src/volume.rs`)

- **The Volume slider is the output device's own OS volume** (Windows endpoint volume, macOS device volume),
  in both directions: it's polled every 250 ms. Only outputs without a volume control (e.g. `null`) use
  SoundSync's software level (`config.playback.volume`).
- Any node can change any node's volume and mute (`Command::ChangeVolume` → `Control::SetVolume`).
- Turn-down is relative in dB: `set_endpoint_volume_db(current_db − x)`, or software level ×10^(−x/20).
- **Follow master volume:** receivers apply the master's endpoint **gain** from its dB level
  (`EndpointVolume::gain()`), not the slider position. Windows' 50% is −10.5 dB, a gain of 0.30. Loopback
  capture of CABLE Input was measured to be **pre-volume**, so the gain is never applied twice.

### Devices

- Windows device events map state changes to Added/Removed. The engine:
  - reopens a lost output or capture with backoff;
  - falls back to the default output while the configured one is missing;
  - follows default-output changes, except that a virtual master's monitor never follows, because the new
    default is usually the cable itself.
- `Status.device_events` counts OS device events, and the UI refreshes its device lists when it moves. There
  is also a 🔄 button.
- macOS sets `kAudioHardwarePropertyRunLoop` to NULL so CoreAudio tracks device changes on its own thread. A
  1 s polling watcher emits the events.
- **Default-output switching** (`defaults.rs`, opt-in): saves all three Windows roles to a state file. It
  restores them when broadcasting stops, on exit, on `WM_ENDSESSION` (a hidden window in `session_end.rs`),
  and at the next start after a crash. Roles that can't be restored stay pending for 10 minutes.

## Conventions

- **Style:** idiomatic Rust 2024, long lines, no rustfmt. Every module starts with a `//!` doc saying what it
  does. Comments explain *why* in plain English. Names and messages read like the surrounding code.
- **User-facing text** is plain, specific and actionable, e.g. "CABLE Input is silent: apps have it open but
  nothing is playing…".
  - A warning (⚠, `Status.warnings`) needs action.
  - A note (ℹ, `Status.notes`) is information.
  - A notice (blue, `app.notice`) is feedback on what the user just did.
- **Errors:** `thiserror` in libraries, `anyhow` in the app. Device and OS failures turn into warnings and
  retries, not panics. Core has no logging, so functions return values (e.g. "restarted") and the engine
  logs them.
- **Real-time:** no locks, allocations or frees on the render path. Known exceptions are listed under
  [Open items](#open-items-and-known-limitations); don't add new ones.
- **COM / threads (Windows):** never initialize COM as MTA on the UI thread. winit's `OleInitialize` then
  panics with `RPC_E_CHANGED_MODE`. Device watching, crash recovery and enumeration in the engine all run on
  the engine thread. `WinHost::new` probes WASAPI on a short-lived thread.
- **Config:** `Config::parse` has no side effects. The engine owns `config.toml` and publishes the
  fingerprint of what it last wrote. The UI applies hand edits only when it sees them on two consecutive 1 s
  checks. Node ids are 63-bit, because TOML integers are signed.
- **Tests (TDD):** write the failing test first and watch it fail for the right reason, then implement.
  - When a new test passes at once, do a **mutation check**: temporarily remove the feature and confirm the
    test fails.
  - Test names are sentences describing behaviour, e.g. `a_speaker_that_sleeps_after_silence_still_plays_its_first_sweep`.
  - Engine tests use `spawn_with(Spec{..}, configure, setup)`, `wait_until` and `FakeHost`. The fake host
    provides: `FakeRoom`, `add_mic`, `make_sleepy`, `set_default_render`, device volumes with a dB curve
    where 50% is −10.5 dB, and `set_active_sessions`.
  - Timing tests accept any group delay in effect during the window (`DelayLog`). A test that needs punctual
    timers is skipped when `timers_are_punctual()` is false.
- **Commits:** an imperative summary line, then a short body of bullets. End the message with the
  `Co-Authored-By:` trailer the system specifies. Commit to `main`, and **never push**.

## Gotchas (learned the hard way)

- **Windows line endings:** Python's `open(p, 'w')` on Windows writes CRLF. The repo is LF (with
  `core.autocrlf=input`). Use `open(p, 'w', newline='\n')` or the editor tools. Check with
  `python -c "print(open(f,'rb').read().count(b'\r'))"`; Git Bash's `grep -c $'\r'` loops were unreliable.
- **Bash heredocs** containing Python with `'''` or `\u` break quoting. Write the script to a file with the
  Write tool, then run it.
- **The exe is locked while SoundSync runs,** and closing the window only hides it. Quit with
  `soundsync.exe --quit` (or `--instance b --quit`), not `taskkill`.
- **`set_read_timeout` on every receive** crashed the network thread on Windows (an access violation in
  `mswsock.dll` `setsockopt`). The RX loop now sets the timeout only when it changes. Don't reintroduce
  per-packet socket option calls.
- **Engine tests starve each other on small machines.** Each one runs 2–3 complete nodes in real time. On a
  3-vCPU CI runner, running tests in parallel made the group delay step up after warm-up or lost a click.
  Pinned to one core, parallel runs failed 5 of 5 and serial runs passed 3 of 3. That is why CI runs one
  test at a time. To reproduce a small runner here, use `run-pinned.ps1` (below).
- **GitHub macOS runners oversleep about 20 ms per timer.** The fake devices adapt their buffering
  (`WakeTracker`), and `SS_FAKE_MIN_SLEEP_MS=20` reproduces the runners locally.
- **The LG TV/soundbar mutes the start of sound after minutes of digital silence:** it was lost after
  4.5 minutes and kept after 42 s. Hence the calibration wake-up noise. The same muting clips the start of
  real audio after long pauses; a keep-awake option was offered but isn't implemented.
- **Webcam/camera mics (OsmoPocket3) have automatic gain:** a 10.5 dB step measured as about 7 dB, and
  levels drifted between sweeps. Level matching under-corrects, so the user runs Check → Correct these until
  within 1 dB.
- **egui UI automation:**
  - UIA works for tabs, buttons, checkboxes and sliders (`RangeValuePattern`).
  - ComboBoxes cannot be expanded through UIA, and posted mouse clicks don't open them. Posted keyboard input
    does: Space on the focused combo, Tab to an item, Space.
- **Screenshots:** only `PrintWindow` of SoundSync's own window (`printwindow.ps1`). A `CopyFromScreen`
  capture once grabbed the user's other windows.
- **WASAPI facts measured on this hardware:**
  - Loopback timestamps are presentation times, so `probe_loopback_latency` returns 0.
  - Loopback is bit-exact (own render on Steam Streaming Speakers, gain 1.000000) and pre-volume (CABLE Input
    at 50% still captured at full level).
  - A virtual endpoint's `IAudioClock` steps in 480-frame blocks, so it is no timing reference.
  - "HDMI (Live Gamer Ultra-Audio)" is a capture device.

## Working with the owner

These agreements come from the owner's instructions over the project's history. Follow them unless told
otherwise.

- **Committing:** commit when a piece of work is done (on `main`); the user pushes. Never push or create
  remote branches.
- **Process:** keep it light. For non-trivial features, ask only questions that change the outcome, offer
  options with a recommendation, then implement. The owner has waived written-spec review ("don't let me
  review it, write it up and continue"). Report outcomes faithfully, including what is unverified.
- **Leave the machine as found:**
  - Don't change Windows default devices or other settings, and don't touch the firewall.
  - Don't install drivers or software.
  - Ask before anything audible. The owner has allowed microphone calibration runs and volume measurements,
    provided every volume changed is restored. Restore everything and delete test-instance folders
    (`%APPDATA%\SoundSync-*`, `%LOCALAPPDATA%\SoundSync-*`) afterwards.
- **Out of bounds:** reference projects (`~/GolandProjects/Sparkle`, `~/GolandProjects/Toolbox-Apps`) are
  read-only. Never ssh into the router.
- **Privacy:** screenshots only via `PrintWindow` of SoundSync's window. The owner's email is for commit
  attribution only.

## Real-hardware testing

Dev PC (Windows 11, named "Sylphy"):

- **Outputs:**
  - **LG TV SSCR2** over NVIDIA HDMI, 7.1, audible. It's the monitor output.
  - **CABLE Input**, 7.1: VB-CABLE, installed by the user.
  - **Steam Streaming Speakers:** a **silent** virtual output, safe for tests.
  - Digital Output (Realtek USB2.0), and Speakers (OsmoPocket3).
- **Mic:** "Capture Input terminal (OsmoPocket3)", a camera mic with automatic gain.
- **Peer:** the user's MacBook ("ramy") is in the same group. It needs a build of the current protocol version
  from CI.

Helper scripts live in `.superpowers/sdd/2026-10-31-soundsync/` and are local only. Each acts on SoundSync's
own window or reads device state:

| Script | Purpose |
|---|---|
| `printwindow.ps1 -ProcessId P -Out f.png` | Screenshot of SoundSync's window only |
| `click-tab.ps1 -ProcessId P -Name X` | Invoke or toggle a named element (tabs, Start, Check…) |
| `uia-slider.ps1 -ProcessId P [-Index i -Value v]` | List sliders, or set one |
| `key.ps1 -ProcessId P -Keys "Space Tab Enter"` | Post key presses to the window (works for combos) |
| `click-at.ps1`, `combo.ps1` | Mouse posting and UIA combo expansion: **both don't work** with egui, kept for reference |
| `resize-window.ps1 -ProcessId P` | Resize the window to see whole panels |
| `read-volumes.ps1 [-Device name -Scalar s]` | Read every output's volume, or set one (restore afterwards!) |
| `run-pinned.ps1 -Exe <test binary or cargo> -Cores n -Runs k -TestArgs "..."` | Run tests pinned to n cores, like a small CI runner; pinning cargo also pins the test binaries it starts |

To test remote features without the Mac, run a second instance on the silent device:

1. Copy the user's config to `%APPDATA%\SoundSync-b\config.toml`.
2. Change it: new id, `name = "TestB"`, `port = 47811`, `master.enabled = false`,
   `output_device = "Speakers (Steam Streaming Speakers)"`, `extra_latency_ms = 0`.
3. Run `soundsync --instance b`, and delete both `SoundSync-b` folders afterwards.

Don't run a calibration while TestB is up: its sweeps are silent, so the run fails.

## Decision log

The original design is in `docs/superpowers/specs/2026-10-30-soundsync-design.md`. Plan deviations and
review rulings are recorded in the local ledger. The decisions most likely to matter:

| Decision | Why |
|---|---|
| VB-CABLE as the virtual device (default mode); Direct capture opt-in | Free and Microsoft-signed with up to 8 channels. A custom driver needs an EV certificate and Microsoft attestation; open-source drivers need test mode. |
| Capture *CABLE Input* by loopback, not *CABLE Output* | WASAPI can't open CABLE Output as 8 channels, and loopback skips the cable's buffer. |
| The master plays its own stream through the same scheduled path (monitor output) | Every speaker, including the master's, stays in sync. |
| Switching the Windows default to the cable is opt-in, with save/restore everywhere (stop, exit, session end, crash) | Never leave the user's speakers changed. |
| The monitor never opens the captured cable | Prevents a feedback loop (review finding C1). |
| Unicast to subscribers, no multicast | Switches without IGMP snooping would flood the LAN. |
| HMAC on every packet; key from group name plus optional secret | Cheap authentication. The default key is public by design; the README tells users to set one. |
| Group delay only grows during a stream; realign on change instead of slewing | The spec says so. Slewing 100 ms at 300 ppm would take minutes. |
| PI gains 2000/1000 and a 0.1 ms deadband (the plan said 40/4 and 0.5 ms) | The plan's values couldn't meet its own 200 µs tracking test. |
| Requirements are offset-independent | Measuring slack against old offsets made D ratchet forever. |
| `probe_loopback_latency` = 0 on Windows | Loopback timestamps measured as presentation times. An audible probe on every broadcast start is unacceptable. |
| macOS device watching polls every second, and the HAL runs on its own thread | No run-loop plumbing; stale device lists after hot-plug. |
| CI packages zips (`ditto` for macOS) | `upload-artifact` drops Unix permissions and would break the `.app`. |
| Calibration: earliest strong arrival, 3 sweeps (median), SNR and spread flags, Check run | Reflections can be louder than the direct sound; one disturbed sweep shouldn't spoil a run. |
| **The Volume slider is the device's own volume** (option A, chosen by the owner) | One volume per speaker, consistent with the OS. Software level only without a device volume. |
| Follow the master's volume by its dB gain | The Windows slider is a loudness curve, and loopback is pre-volume. |
| Volume matching turns louder speakers down to the quietest one, using device volume (owner's choice over a separate trim) | Requested. It never boosts, and under-correction with automatic-gain mics converges via Check. |
| Remote mic lists on request (`ListInputs`), not in gossip | Enumerating every second is costly, and lists can overflow a datagram. |
| Silence on CABLE Input is a note, not a warning | Usually nothing is playing at all (a paused video keeps its session). |
| Protocol v2, with a version-mismatch warning | Mixed versions used to fail silently. |
| A GitHub release for every push to `main` that passes; build number and commit stamped into the app (owner's choice over a rolling release or tag-only releases) | Every build is downloadable from one page without an Actions login (artifacts expire after 90 days), and each device shows exactly which build it runs. |

## History

| Commit | Change |
|---|---|
| `e9206d1` | Whole initial system built from a 27-task plan (core → audio → engine → app → macOS → hardening). A five-pass review found 3 critical and 7 important issues, all fixed (C1–C3, I1–I7, see ledger). Initially uncommitted at the owner's request, then committed in one go. |
| `35d40d3` | Removed the original brief `input.txt`. |
| `5c74c95` | Gold tray/app icon in the style of the owner's Sparkle app; macOS CI test fixes. |
| `3b42a80` | `uninstall-windows.ps1`; README on install, update and uninstall (no registry, per-user install). |
| `6c38993` | Stale layout warning fixed (warnings computed live); macOS CI timing robustness (`WakeTracker`, `SS_FAKE_MIN_SLEEP_MS`). |
| `da5c282` | Calibration: earliest arrival, three sweeps per device, signal quality, Check run. |
| `c925531` | Volume slider = the output device's own volume; follow the master in dB (measured: loopback is pre-volume). |
| `12b8566` | Windows network-thread crash (setsockopt per packet) fixed; first stream starts with enough delay. |
| `2c03073` | Design: volume matching, remote volume and microphones, device refresh. |
| `c2401eb` | Volume matching, remote volume and mics, device refresh, TV wake-up noise, silence note, protocol v2 with version warning. |
| `ed9b05d` | This guide. |
| `fcaec3d` | CI runs tests one at a time (the macOS job failed two timing tests from tests running in parallel on 3 vCPUs). |

## Open items and known limitations

- **The owner hasn't decided on these firewall options:**
  - The installer removes Windows' automatic *block* rules for the exe.
  - A clearer "inbound blocked" message.
  - A README section on running the exe without installing.
- **macOS runtime is unverified** (compile-checked only): CoreAudio timing, volume and dB setting, the HAL
  run loop, the LaunchAgent, the bundle. The Mac must run the current protocol version.
- **Not implemented:**
  - An optional inaudible keep-awake signal for TVs that mute after silence.
  - Per-stream volume and mute (in the original spec).
  - Replay protection for control messages.
- **Deferred review minors** (details in the ledger):
  - Jitter capacity is sized at the output rate.
  - Discontinuities keep `frame_index` contiguous.
  - The realign fade covers only the first segment.
  - Real-time exceptions: the calibration mic callback locks and allocates; the mixer frees on the render
    thread when its garbage ring is full; ArcSwap guard drops; `send_to` per subscriber.
  - NaN samples pass through the limiter.
  - Duplicate device names aren't disambiguated.
  - The keep-alive event handle is closed early.
  - The render loop relies on a 100 ms wait timeout.
  - Polling enumerates endpoints often.
  - Reopen backoff resets on each loss.
  - The gap detector trips on any engine stall over 2 s.
  - Some warnings persist until restart.
  - `hello_targets` are never pruned.
  - `state.toml` is written without a temp-file swap.
- **±1 ms timing tests on shared CI runners:** CPU starvation from parallel tests was the cause, and serial
  runs fixed it. If a test still flakes on CI, check for starvation first: reproduce with
  `run-pinned.ps1`, and fix the test or the CI setup, not the product.
