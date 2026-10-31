# SoundSync — design spec + implementation plan

## Context

`input.txt` asks for a new LAN multi-room audio system for Windows and Mac.
- Any Windows PC can become a **master**: its audio is captured bit-exact, including multichannel, plays locally,
  and streams losslessly to every node.
- Every node plays the mix of all masters, with **per-device delay compensation**. Laptop speakers, TVs, eARC
  and Bluetooth speakers then all sound in sync, even when the master's own speaker is the slowest.
- Mac is receive-only.
- The app is a compiled Rust exe with a GUI and tray. It starts with the system, auto-discovers peers on a busy
  LAN, and is configured from a config file.

The repo (`C:\Users\dan\GolandProjects\SoundSync`) is empty: only `input.txt` and `.idea`, and it isn't a git repo.
I read the reference projects without changing them:
- `Toolbox-Apps\windows\ToolboxApps.cs` and `install-startup.ps1`
- `Sparkle\windows\SparkleBackend.cs` and `install-backend-startup.ps1`

They share one pattern, which SoundSync copies:
- A per-user install with no admin.
- Autostart through a Startup-folder `.lnk`, and a Start Menu `.lnk` with an arg that opens the window.
- `-Remove` deletes only the shortcuts it owns.
- A named mutex plus a named event for single instance.
- Closing the window hides it; only tray Exit quits.
- The tray menu starts with a disabled status line.
- Logs rotate: keep 5, and a crashed session's log is archived with a `-recovered` suffix.

**Decisions made with you:**
- **Virtual device = VB-CABLE.** It's free, Microsoft-signed and handles up to 8ch. A driver SoundSync creates
  itself would need an EV cert plus Microsoft attestation. The open-source drivers (Scream, VirtualDrivers) need
  test mode or Secure Boot off on Win11.
- **Virtual-device mode is the default**, giving full sync. **Direct capture** of a selected real output is an
  opt-in mode with lower latency but only partial sync.
- **Switching the Windows default output to CABLE Input is an opt-in toggle.** The previous defaults are saved and
  restored even after a crash.
- **Calibration:** manual per-node offsets, a sync click test, and automatic mic calibration.
- **Mac builds:** GitHub Actions. You test on a real Mac.
- **This PC:** install Rust only (per-user rustup). Don't install drivers, don't change default devices, don't add
  firewall rules, don't change any other Windows settings.

## Architecture

Rust workspace on the MSVC toolchain; release profile `lto="fat"`, `codegen-units=1`, `opt-level=3`:

```
SoundSync/
  Cargo.toml                 workspace (pin windows = 0.62 to match wasapi 0.26)
  crates/core/               soundsync-core: pure logic, no OS deps, heavily unit-tested
    protocol.rs              packet encode/decode + HMAC auth
    clock.rs                 peer clock offset/skew estimator (min-RTT filter + regression)
    jitter.rs                frame-indexed jitter buffer (late packets fill holes)
    playout.rs               timing controller: pre-align, PI→ratio, deadband, hard resync w/ fade
    layout.rs                channel masks, mismatch detection, position-based remap/downmix
    delay.rs                 presentation-delay policy (virtual auto-D, direct reference)
    calib.rs                 chirp/click generation, FFT cross-correlation, offset solver
    limiter.rs               soft limiter for summed streams (inactive below 0 dBFS)
    config.rs                TOML schema, defaults, validation
  crates/audio/              soundsync-audio: AudioBackend trait + implementations
    win/                     wasapi crate: enum, loopback capture, keep-alive render, timed render,
                             RAW mic capture, IPolicyConfig default-device save/restore, device notifications
    mac/                     coreaudio-rs/objc2-core-audio: enum, timed AUHAL render, mic capture
    fake.rs                  virtual-clock backend for tests (ppm drift, latency, jitter)
  crates/engine/             soundsync-engine: headless node — sockets, mDNS discovery, handshake,
                             gossip, leases, sender, receiver+mixer, clock service, calibration coordinator.
                             API: command channel in, Arc<Status> snapshot out.
  crates/app/                soundsync (bin): egui GUI, tray, autostart, single instance, logs, CLI
  scripts/                   build-windows.ps1, install-windows.ps1, bundle-macos.sh
  assets/                    icons (.ico/.png/.icns)
  .github/workflows/build.yml
```

Key crates:
- Audio: `wasapi` 0.26 + `windows` 0.62 (raw COM for `GetStreamLatency`, IPolicyConfig and MMCSS),
  `coreaudio-rs`/`objc2-core-audio`.
- `rubato` 5: the Async resampler with `FixedAsync::Output`, `set_resample_ratio_relative` and allocation-free
  `process_into_buffer`; sinc length 128, linear interpolation.
- Networking: `rtrb`, `mdns-sd` ≥0.21.6, `socket2`.
- DSP and auth: `realfft`, `hmac`+`sha2`.
- Config: `serde`+`toml`, `directories`.
- App: `eframe`/`egui` (winit ≥0.30.6), `tray-icon`, `tracing`(+appender), `clap`.

### Threads (per node)
- **Network RX:** one UDP socket with `SO_RCVBUF` set to 4 MB. Audio goes into each stream's `rtrb` ring;
  control packets go to the engine thread.
- **Capture (Windows master):** WASAPI loopback on a thread with MMCSS "Pro Audio" priority. It reads into
  preallocated buffers, builds packets and sends them unicast to each subscriber.
- **Render:** WASAPI event-driven shared mode, or the CoreAudio callback on Mac.
  - It drains the rings into frame-indexed jitter buffers, then runs timing, resampling and layout mapping for each
    stream, sums them, applies the soft limiter and writes the output.
  - No allocations and no locks on this path.
  - It always renders, writing zeros when there's nothing to play, so HDMI and Bluetooth sinks never sleep and
    clip the start of the next sound.
- **Engine:** discovery, gossip, clock pings, leases, the delay policy, calibration and status snapshots.
- **UI:** egui plus the tray, reading snapshots and sending commands. The engine never depends on the UI's
  `update()` running.

## Master modes (Windows only)

**1. Virtual device (default).** Apps play to VB-CABLE "CABLE Input".
- SoundSync loopback-captures CABLE Input, which skips the cable's internal buffer. It doesn't use CABLE Output,
  because WASAPI can't open that as 8 channels.
- The node plays its own stream to a configured real **monitor output** through the same scheduled path as every
  receiver, so all speakers, including the master's, stay in sync.
- The stream's `presentation_offset` is D, the group delay. Auto D = the largest requirement any node reports, plus
  a margin.
  - D is chosen at stream start and **only increases automatically**. An increase is applied as a faded hard
    resync; slewing 100 ms at ±300 ppm would take minutes.
  - D can also be set manually. It decreases only by manual change or at a stream restart.
- **VB-CABLE helper:**
  - SoundSync detects the cable. If it's missing, a "Set up VB-CABLE" flow first saves the current default devices
    (all three roles), then opens the official VB-Audio page and says the cable is a VB-Audio product.
  - Once the new device appears, it restores the saved defaults, because the install can switch them.
  - It checks the CABLE Input mix format. If its channel count is lower than the monitor output's, it asks you to
    set the cable to 5.1 or 7.1 ("Configure speakers…" opens `mmsys.cpl`).
- **Opt-in toggle: "Switch Windows default output to VB-CABLE while broadcasting".**
  - The previous defaults are saved to a state file and restored when broadcasting stops, on exit, or at the next
    launch after a crash.
  - If the toggle is off, the UI shows routing instructions and an "Open Sound settings" button.
- Silence on CABLE Input while apps are playing (an exclusive-mode app, or DRM content) triggers a warning.
- If VB-CABLE is missing when you start a broadcast, SoundSync offers to set it up or to switch to direct mode.

**2. Direct capture (opt-in, lowest latency).** WASAPI loopback on a chosen real render endpoint.
- Windows plays the audio as usual, and the master's speaker becomes the time reference:
  `presentation_offset` = the master's measured output path time plus its calibrated extra.
- Remote nodes that are fast enough aim for that reference. Any node that can't make it plays as soon as it can.
  The UI shows "behind by X ms" and a one-click "Switch to virtual mode for full sync".
- While direct-capturing, this node doesn't play other masters' streams on the captured device, because they would
  be recaptured and cause echo. The UI says so. A later option could use process-loopback exclusion
  (Win10 20348+).

**Capture details (both modes):**
- **Format:** bit-exact f32 in the endpoint's mix format, with the full channel count and `dwChannelMask`.
- **Keep-alive:** a silent render stream on the captured endpoint keeps loopback packets flowing. It uses
  IAudioClient3's minimum period where the device supports it, which lowers that endpoint's engine period.
  IAudioClient3 can't be used on the loopback stream itself.
- **Timestamps:** `BufferInfo.timestamp` is in 100 ns units. A linear regression of frame index against QPC time
  smooths it.
  - The `SILENT` flag means the data is treated as zeros.
  - `DATA_DISCONTINUITY` and `TIMESTAMP_ERROR` reset the regression.
- **Self-timing probe at capture start:** what loopback timestamps mean isn't documented, so SoundSync measures it.
  It renders a marker through its own render path, finds it in the loopback capture, and derives the real
  relationship between capture time and output time on that machine. Calibration absorbs whatever remains.
- **Volume:**
  - Loopback is requested pre-volume, so the captured content stays bit-exact.
  - The master's endpoint volume and mute are sent as stream metadata. Receivers apply them when "Follow master
    volume" is on (default on), so the media keys on the master control the whole group.
  - M2 measures on this PC whether loopback here is really pre-volume.

## Network and discovery (LAN-friendly)
- **One UDP port** (default 47810), IPv4.
  - mDNS DNS-SD `_soundsync._udp.local.` through `mdns-sd`, with vEthernet, WSL, Hyper-V, Docker, Tailscale,
    VPN and loopback interfaces disabled (`disable_interface` predicate). A config `interface` setting overrides
    the choice.
  - SoundSync only uses a peer address that falls in the subnet of an enabled local interface. An authenticated
    HELLO handshake has to succeed before anything is streamed.
  - Only nodes in the same `group` are peered.
  - Optional `static_peers` cover networks where mDNS is blocked.
  - No subnet scans and no broadcasts.
- **Audio goes unicast to subscribed receivers only.** Multicast isn't used, so switches without IGMP snooping
  don't flood the other LAN devices.
  - Leases renew every 1 s and expire after 3 s.
  - The payload is ≤1400 B, so there's no fragmentation even over VPN or IPv6 paths.
  - **Silence suppression:** after 2 s of digital silence, the master sends small `SILENT` packets instead of
    audio. Receivers keep rendering zeros.
- **Packet types:** HELLO, AUDIO, SILENT, PING/PONG, STATE (1 s gossip), SUBSCRIBE, SET_OFFSET, CALIB_*,
  SYNC_TEST.
  - Every packet carries the magic "SSYN", a version, the type and the sender id.
  - Every packet ends with an 8-byte truncated HMAC-SHA256 keyed by the group key. The default key is derived
    from the group name; setting a secret key is optional.
- **AUDIO header:** stream id, seq, `frame_index` (u64), `capture_time_ns` (master clock),
  `presentation_offset_ns`, rate, channels, channel mask, sample format, and master volume and mute.
  - The payload is raw interleaved PCM as captured, so it's lossless; 7.1 at 48 kHz in f32 is about 12 Mbps.
  - Lost packets are filled with zeros and counted in the stats.
- **Windows Firewall:** `install-windows.ps1` includes one elevated step, which you can skip with `-NoFirewall`.
  - It adds inbound UDP allow rules for this exe's full path, covering the app port and 5353.
  - The UI detects when no peer can reach this node and shows "inbound blocked".
  - Nothing is installed on this PC unless you run the script.

## Sync engine
- **Clock:** each node uses its monotonic clock in ns: QPC on Windows, `mach_absolute_time` on Mac.
  - Every 250 ms (a faster burst at startup), each node sends an NTP-style 4-timestamp PING to every peer.
  - The estimator keeps the samples with the lowest RTT and fits a regression to get offset and skew. The target is
    better than 100 µs on wired LAN.
  - Clock state resets after sleep or resume, because `mach_absolute_time` stops during sleep.
- **Schedule:** node n aims for each sample to reach its DAC at
  `to_local(capture_time) + presentation_offset − E_n`, where E_n is node n's calibrated or manual extra latency.
- **DAC time of the next output frame:**
  - **Windows:** from `IAudioClock::GetPosition` (position divided by `GetFrequency`) plus its QPC time, the frames
    written, and the stream latency obtained through raw COM. Receivers use the standard shared period by default;
    IAudioClient3 is opt-in because it changes the period for every app on that endpoint.
  - **Mac:** `mHostTime + (device latency + stream latency)/rate`. The stream latency is queried on the stream
    objects. The safety offset and buffer size are not added, because `mHostTime` already includes them. The AUHAL
    client format is set to the device's native rate, which avoids converter latency.
  - Both also add rubato's `output_delay()` and any intermediate FIFO. Any remaining constant error is absorbed by
    calibration.
- **Controller:**
  1. Playback starts with a hard pre-alignment: drop frames or pad with zeros until the error is near zero.
  2. A PI controller then works on the low-pass-filtered error, with a 0.5 ms deadband. Its output is the relative
     resample ratio, capped at ±300 ppm.
  3. The same resampler converts the stream rate to the device rate.
  4. If |error| > 15 ms, it hard-resyncs with a 5 ms fade.
- **Feasibility:** each node reports its minimum margin, the time between a packet arriving and its write deadline.
  This feeds auto-D in virtual mode and the "behind by X ms" display in direct mode.
- **Mixing:** streams from several masters are each timed, resampled and layout-mapped on their own, then summed.
  Each stream has its own volume and mute, and the soft limiter only acts above 0 dBFS.
- **Layouts:** WAVEFORMATEXTENSIBLE masks, and Mac `AudioChannelLayout` labels mapped to the same bits.
  - 5.1 comes as 0x3F (back) or 0x60F (side), and 7.1 as 0xFF or 0x63F. SL/SR and BL/BR substitute for each other
    instead of being downmixed.
  - Matching layouts pass straight through. Real mismatches log a warning, show a UI banner ("Configure
    speakers…" opens `mmsys.cpl` or Audio MIDI Setup), and fall back to a position-based ITU downmix or upmix.

## Calibration
1. **Manual:** each node has an "extra latency (ms)" slider in the group table. It can be edited from any node via an
   authenticated SET_OFFSET message and is saved in that node's config.
2. **Sync test:** a click track (1 click/s, a different pitch per node) scheduled at shared group times on all nodes
   at once. You adjust the sliders until the clicks merge into one.
3. **Auto (mic):** choose a mic node and its mic, Windows or Mac.
   - The wizard warns against Bluetooth headset mics, because they switch the output to HFP and change its latency.
   - Capture uses RAW mode (no AGC or noise suppression). Mic sample times are converted through a QPC regression,
     not the nominal sample rate.
   - The coordinator pauses stream playback and has each node in turn render a 100 ms log sweep at a scheduled
     group time.
   - FFT cross-correlation with peak interpolation gives each node's arrival residual.
   - The new value is `E_n += residual_n − residual_ref`. The reference is the median node in virtual mode and the
     master in direct mode.
   - Only relative values matter, so the mic's input latency cancels out. You see the results before applying them.
   - Bluetooth latency drifts (AirPods report about 160 ms but measure 180–220 ms), so the UI suggests rerunning
     calibration when a Bluetooth device reconnects.

## App / OS integration
- **egui window:**
  - Header: node name and group.
  - **This PC:** Broadcast toggle, mode (VB-CABLE plus monitor device / direct device), VB-CABLE helper,
    default-switch toggle, and layout info and warnings.
  - **Playback:** output device, volume, follow master volume, and play remote streams.
  - **Group:** a table of nodes showing OS, role, RTT and jitter, loss, reported latency, the extra latency slider,
    and sync state (in sync, or behind by X ms).
  - **Sync:** auto or manual D, the click test, and the calibration wizard.
  - **Settings:** autostart, group name and key, interface, and buttons to open the config and logs.
- **Tray** (tray-icon): a disabled status line, Open SoundSync, Broadcast this PC (Windows), Mute, Sync test, Quit.
  Double-clicking opens the window. Closing the window hides it; only Quit exits.
  - The tray is created inside the eframe creator closure, which runs on the main thread after the event loop
    starts. macOS requires that.
  - Show and hide work through `TrayIconEvent`/`MenuEvent::set_event_handler`. On Windows, the handler calls
    `ShowWindow` on the HWND taken from `cc.window_handle()`, then `request_repaint`. This avoids the egui bug
    where `update()` stops running while the window is hidden.
- **Windows:**
  - `#![windows_subsystem="windows"]`, so no console window.
  - A named mutex `Local\SoundSync.<hash(config path)>` and a `.Show` event; a second launch brings up the
    existing window.
  - `--show` opens the window. `--config` and `--instance` let two instances run side by side for local tests.
  - "Start with Windows" creates or removes `Startup\SoundSync.lnk`, with the same ownership check as the
    reference projects.
  - `install-windows.ps1` copies the exe to `%LOCALAPPDATA%\Programs\SoundSync` and creates the Start Menu
    (`--show`) and Startup shortcuts. Flags: `-Remove`, `-NoStartup`, `-NoFirewall`, `-Start`. Only the firewall
    step needs elevation.
- **Mac:**
  - A menu bar app (`LSUIElement`; the activation policy is never set to Regular).
  - `Info.plist` includes `NSMicrophoneUsageDescription`, `NSLocalNetworkUsageDescription` and `NSBonjourServices`.
  - "Start at login" adds a LaunchAgent that runs `open -a SoundSync.app`, not the binary directly, so macOS Local
    Network permission is tied to the app.
  - The Local Network prompt is triggered from the foreground UI on first run. A "no LAN access" status appears
    when discovery is blocked.
  - Ad-hoc signing can reset that permission after an update. This is documented, and Developer ID signing is noted
    as an optional upgrade.
  - `bundle-macos.sh` builds a universal binary with lipo, then the `.app`, ad-hoc codesign and a zip.
- **Config:** `%APPDATA%\SoundSync\config.toml` on Windows, `~/Library/Application Support/SoundSync/config.toml`
  on Mac. It's generated on first run with a random node id and the hostname as the node name.
  - Sections: `[network] group/key/port/interface/static_peers`,
    `[master] enabled/mode/capture_device/monitor_device/switch_default`,
    `[playback] output_device/volume/follow_master_volume/extra_latency_ms`, `[sync] delay_ms="auto"`.
  - An unknown device name falls back to the default device with a warning.
- **Logs:** go to `%LOCALAPPDATA%\SoundSync\logs` on Windows and `~/Library/Logs/SoundSync` on Mac. A new file each
  session, keep 5, and a crashed session's log gets a `-recovered` suffix.
- **Robustness:** SoundSync reopens streams when a device is removed or the default device changes (WASAPI device
  notifications, CoreAudio listeners) and after sleep or resume.

## Implementation milestones
Each milestone uses TDD for the pure logic. After you approve this plan, I'll:
- Save it as `docs/superpowers/specs/2026-10-30-soundsync-design.md`.
- Break it into tasks with the writing-plans skill.
- Execute them with checkpoints.

No git commits or pushes unless you ask.
- **M0 Setup:**
  - Install rustup per-user (stable-x86_64-pc-windows-msvc) and `git init`.
  - Create the workspace skeleton, the CI workflow (windows-latest: test + release exe zip; macos-latest: test +
    universal `.app` zip), and the icons.
- **M1 core:** protocol, clock, jitter, playout controller, layout, delay policy, limiter, config, calibration DSP.
- **M2 Windows audio:** device listing (name, mix format and mask, periods, latency), loopback capture with timestamps,
  flags and keep-alive, the self-timing probe, timed render, RAW mic capture, IPolicyConfig save and restore, and device
  notifications.
  - Dev CLI: `soundsync devices`, `soundsync tone --device X`, and `soundsync measure --a X --b Y` (loopback-captures
    two endpoints and cross-correlates them).
- **M3 engine:** sockets, mDNS plus interface filtering, HELLO, gossip, leases, sender, receiver and mixer, the clock
  service, the delay policy, and headless `soundsync run`.
- **M4 calibration:** manual offsets, sync test, and the mic wizard backend.
- **M5 app:** egui UI, tray, single instance, autostart, the VB-CABLE helper, logs, the build and install scripts.
- **M6 macOS:** CoreAudio render and mic, menu bar, LaunchAgent, bundle script, CI. You test it.
- **M7 hardening:** hot-plug, sleep and resume, layout prompts, silence suppression, stats polish.

## Verification
- **Unit tests** (`cargo test`, CI on both OSes):
  - Packet round-trip and HMAC rejection.
  - The clock estimator stays within 50 µs under simulated offset, ±100 ppm skew and jitter.
  - The controller keeps error under 0.2 ms at ±200 ppm drift, pre-aligns, and hard-resyncs on a 50 ms jump.
  - A late packet still fills its jitter-buffer hole.
  - Layout matrices are correct, including the 0x3F↔0x60F and 0xFF↔0x63F substitutions.
  - Calibration recovers synthetic delays to within ±0.5 ms with added noise.
  - Auto-D only increases.
  - The limiter does nothing below 0 dBFS.
- **Engine integration test:** two or three engines in one process over 127.0.0.1, with fake virtual-clock backends
  using different ppm and latencies.
  - Each receiver's output timing must match the presentation time to within 1 ms.
  - Tested under jitter, 1% loss and reordering, and masters joining and leaving.
  - Direct mode must report the correct "behind by" values.
- **On this PC (no settings changed, nothing installed except Rust):**
  - `soundsync devices` lists the five endpoints with their layouts.
  - Two instances (`--instance a/b`, different ports) discover each other over the LAN interface, handshake and sync
    clocks.
  - A loopback-captures **Steam Streaming Speakers**, an existing silent virtual sink that stands in for VB-CABLE,
    while `soundsync tone` plays into it. B receives the stream, and `measure` and stats confirm it's bit-exact
    against the tone and correctly timed.
  - This also checks whether loopback here is pre- or post-volume.
  - I'll ask you before any test that makes sound (LG TV or Realtek).
- **GUI:** I'll launch the app and check the tray menu, hide and show, the autostart toggle (create the `.lnk`, then
  remove it), that a second launch shows the existing window, and that logs are written.
- **Mac (you):** run the CI artifact. Check that the Local Network prompt appears, it discovers the Windows master,
  plays in sync, starts at login, and that mic calibration works.
- **VB-CABLE end to end (you, or me later with your OK):** install it through the helper (defaults are restored),
  set it to 7.1, broadcast in virtual mode, and calibrate across the TV, a laptop and a Bluetooth speaker.
