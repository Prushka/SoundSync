# SoundSync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build SoundSync, a Rust tray app for Windows and macOS. A Windows master's audio is captured bit-exact and
sent losslessly over the LAN. Every node plays all masters' streams, kept in sync across speakers whose latencies
differ.

**Architecture:** A workspace with four crates:
- `soundsync-core`: all protocol, timing, sync, layout and DSP logic, as pure, deterministic and unit-tested code.
- `soundsync-audio`: WASAPI and CoreAudio behind an `AudioHost` trait, plus a `FakeHost` (virtual devices with
  configurable ppm, latency and clock offset) and a `null` output.
- `soundsync-engine`: a headless node (UDP, mDNS, clock sync, streaming, calibration) driven by `Command`s. It
  publishes `Status` snapshots.
- `soundsync` (app): CLI, egui GUI, tray, autostart, single instance and logs.

**Tech Stack:** Rust 1.99.1 (edition 2024, MSVC on Windows), wasapi 0.26, windows 0.62, coreaudio-rs 0.14,
objc2-core-audio 0.3, rubato 5, rtrb 0.4, mdns-sd 0.21.6, socket2 0.6, realfft 3.5, hmac 0.13, sha2 0.11,
postcard 1.1, serde, toml 1.1, eframe/egui 0.37, tray-icon 0.26, tracing, clap 4.6.

**Spec:** `docs/superpowers/specs/2026-10-30-soundsync-design.md`

## Global Constraints

- **Toolchain:** Rust stable 1.99.1, `x86_64-pc-windows-msvc`, edition 2024.
  `cargo clippy --workspace --all-targets -- -D warnings` must pass at every checkpoint.
- **Release profile:** `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`.
- **Platforms:** Windows 10 1703+ / 11 x64, and macOS 12+ as a universal binary (aarch64 + x86_64). macOS is
  receive-only: no capture and no master UI.
- **Names:**

  | Item | Value |
  |---|---|
  | Binary | `soundsync` / `soundsync.exe` |
  | Display name | `SoundSync` |
  | mDNS service | `_soundsync._udp.local.` |
  | Windows mutex | `Local\SoundSync.<16-hex FNV-1a-64 of the lowercased config path>` |
  | Windows events | `<mutex name>.Show`, `<mutex name>.Quit` |
  | Startup shortcut | `SoundSync.lnk` |
  | macOS bundle id / LaunchAgent label | `com.soundsync.app` |

- **Paths** (Windows / macOS):

  | File | Windows | macOS |
  |---|---|---|
  | Config | `%APPDATA%\SoundSync\config.toml` | `~/Library/Application Support/SoundSync/config.toml` |
  | Logs | `%LOCALAPPDATA%\SoundSync\logs\` | `~/Library/Logs/SoundSync/` |
  | State | `%LOCALAPPDATA%\SoundSync\state.toml` | `~/Library/Application Support/SoundSync/state.toml` |

  `--instance <name>` turns every `SoundSync` directory name into `SoundSync-<name>`.
- **Network:**
  - UDP port 47810 by default, IPv4 only. Audio is unicast only.
  - Datagram ≤ 1472 B; audio payload ≤ 1400 B.
  - Every packet ends with an 8-byte truncated HMAC-SHA256 tag.
  - The HELLO handshake must complete before any streaming.
  - PING every 250 ms (every 50 ms during a peer's first 2 s).
  - STATE gossip every 1 s. SUBSCRIBE renews every 1 s; leases expire after 3 s; peers expire after 5 s.
  - Silence suppression starts after 2 s of digital silence.
  - `SO_RCVBUF` = 4 MiB.
- **Sync:**
  - Ratio correction capped at ±300 ppm, with a 0.5 ms deadband.
  - Hard resync when |error| > 15 ms, with 5 ms fades.
  - Auto D: margin 5 ms, quantum 5 ms, min 20 ms, max 2000 ms. Auto D only increases while a stream runs; it
    decreases only by manual change or when the stream restarts.
  - rubato Async: `sinc_len 128`, `f_cutoff 0.95`, `oversampling_factor 256`, `Linear` interpolation,
    `BlackmanHarris2` window, `FixedAsync::Output`, chunk 64 frames.
- **Realtime rule:** no allocation, locking, logging or blocking syscalls inside render or capture callbacks or the
  realtime threads, except the audio API calls themselves.
- **Time:** every time value inside a node is `Nanos` (i64 ns) on that node's monotonic clock, read via
  `AudioHost::now_ns()`. Times inside control packets are in the sender's clock.
- **This PC:**
  - Never change default devices, Windows settings or the firewall, and never install drivers.
  - Any test that makes sound (LG TV, Realtek, Live Gamer) needs the user's OK first.
  - Silent endpoints for tests: `Steam Streaming Speakers`, plus the `null` output.
- **Git:** the repo is initialized. Never commit or push unless the user asks; "Checkpoint" steps only run tests
  and clippy.

## Review Focus

1. **Config copied between PCs.** A `config.toml` copied from one PC to another keeps the same `node.id`. The
   node that sees another address using its own id must regenerate and save a new id and warn, not fight it
   (Task 14, `duplicate_node_id_regenerates`).
2. **Hand-edited config.** A config saved by Notepad with a UTF-8 BOM, unknown keys or out-of-range values must
   load with clamped defaults and warnings, never crash. An unparseable file is backed up and replaced
   (Task 7, `bom_unknown_keys_and_bad_values_are_tolerated`).
3. **Ambiguous device names.** On this PC "LG TV SSCR2 (NVIDIA High Definition Audio)" appears twice, once
   inactive. Selecting by name must pick the active exact match and report real ambiguity
   (Task 9, `select_prefers_active_exact_match`).
4. **Capture format change mid-broadcast.** For example, you switch CABLE Input to 7.1 or change its rate. The
   sender must start a new stream id, and receivers must rebuild their player with no crash or garbage audio
   (Task 16, `format_change_starts_new_stream`; Task 17, `format_change_flags_slot`).
5. **Long gaps.** Laptop sleep, or a master restart where `frame_index` restarts. The receiver must realign without
   growing memory or spinning (Task 3, `far_future_insert_resets`; Task 5, `long_gap_realigns_without_growth`;
   Task 27, `tick_gap_resets_clocks`).

---

## File Structure

```
Cargo.toml                         workspace + shared deps + release profile
.gitignore                         target/, dist/
.github/workflows/build.yml        CI: windows-latest + macos-latest
crates/core/src/
  lib.rs                           re-exports
  types.rs                         Nanos, NodeId, StreamFormat, MasterMode, Role, Os
  protocol.rs                      wire format, Control/Packet, HMAC
  clock.rs                         ClockEstimator, ClockMap
  timeline.rs                      FrameClock (frame index → time regression)
  jitter.rs                        JitterBuffer
  layout.rs                        speaker masks, ChannelMap, CoreAudio label mapping
  limiter.rs                       SoftLimiter
  sync.rs                          SyncController
  player.rs                        StreamPlayer (jitter + resampler + controller + layout)
  delay.rs                         DelayPolicy
  config.rs                        Config schema, load/validate/save
  paths.rs                         Paths (config/log/state per instance)
  calib.rs                         sweep/click, find_signal, solve_offsets
crates/audio/src/
  lib.rs                           AudioHost trait + shared types, default_host()
  select.rs                        DeviceSelector, select_device
  null.rs                          NullOutput (timer-driven, discards audio)
  fake.rs                          FakeHost (tests)
  win/{mod,com,enumerate,render,loopback,probe,input,volume,notify,policy,mmcss}.rs
  mac/{mod,enumerate,output,input,clock}.rs
crates/engine/src/
  lib.rs                           Engine, EngineOptions
  command.rs  status.rs            Command, Status
  net.rs                           Transport, PacketSink, RX thread, NetImpair
  peers.rs                         PeerTable, Peer
  discovery.rs                     interface filter, LocalNet, mDNS Discovery
  chunk.rs                         AudioChunk
  leases.rs  sender.rs             Leases, CaptureSender
  receiver.rs                      Mixer (RenderCallback), StreamSlot, MixerCmd
  node.rs                          engine thread loop, orchestration
  streams.rs                       open/close capture + outputs, slots per remote stream
  calibration.rs  synctest.rs      calibration coordinator/participant, sync clicks
  defaults.rs                      DefaultSwitchGuard (save/switch/restore defaults)
  devices.rs  power.rs             reopen/backoff, sleep-gap detection
crates/engine/tests/
  common/mod.rs                    helpers: spawn engines on 127.0.0.1 with FakeHost
  clock_sync.rs  streaming.rs  calibration.rs  hotplug.rs
crates/app/
  build.rs                         procedural .ico → winresource (Windows)
  src/main.rs  cli.rs  headless.rs logging.rs  single_instance.rs  icon.rs
  src/autostart/{mod,windows,macos}.rs
  src/ui/{mod,app,panels,tray,format,vbcable}.rs
scripts/build-windows.ps1  install-windows.ps1  bundle-macos.sh
assets/Info.plist.in
README.md
```

---

## Phase 1 — Core logic (`soundsync-core`)

### Task 1: Workspace, CI, core types and wire protocol

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `.github/workflows/build.yml`, `crates/core/Cargo.toml`,
  `crates/core/src/{lib.rs,types.rs,protocol.rs}`
- Test: `crates/core/src/protocol.rs` (`mod tests`)

**Interfaces:**
- Produces (`types.rs`):
  ```rust
  pub type Nanos = i64;  pub type NodeId = u64;
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub struct StreamFormat { pub sample_rate: u32, pub channels: u16, pub channel_mask: u32 }
  pub enum MasterMode { Virtual, Direct }            // Copy, Eq, serde
  pub enum Role { Receiver, Master(MasterMode) }     // Copy, Eq, serde
  pub enum Os { Windows, MacOs, Other }  impl Os { pub fn current() -> Os }
  ```
- Produces (`protocol.rs`):
  ```rust
  pub const MAGIC: [u8;4] = *b"SSYN"; pub const PROTOCOL_VERSION: u8 = 1;
  pub const HEADER_LEN: usize = 14; pub const AUDIO_HEADER_LEN: usize = 49; pub const TAG_LEN: usize = 8;
  pub const MAX_DATAGRAM: usize = 1472; pub const MAX_AUDIO_PAYLOAD: usize = 1400; pub const DEFAULT_PORT: u16 = 47810;
  pub struct GroupKey([u8; 32]);  impl GroupKey { pub fn derive(group: &str, secret: &str) -> GroupKey }
  pub struct AudioHeader { pub stream_id: u32, pub seq: u32, pub frame_index: u64, pub frames: u16,
      pub capture_time_ns: Nanos, pub presentation_offset_ns: Nanos, pub format: StreamFormat,
      pub volume: f32, pub muted: bool }                       // Copy, PartialEq
  pub enum Control { Hello(Hello), Ping { t1: Nanos }, Pong { t1: Nanos, t2: Nanos, t3: Nanos },
      State(NodeState), Subscribe { stream_id: u32 }, Unsubscribe { stream_id: u32 },
      SetOffset { target: NodeId, extra_latency_ns: Nanos }, Calib(CalibMsg),
      SyncTest { active: bool, epoch_ns: Nanos } }             // serde, Clone, PartialEq
  pub struct Hello { pub reply: bool, pub name: String, pub os: Os, pub app_version: String, pub group: String, pub session: u64 }
  pub struct NodeState { pub name: String, pub os: Os, pub session: u64, pub role: Role,
      pub stream: Option<StreamInfo>, pub output: Option<OutputInfo>, pub extra_latency_ns: Nanos,
      pub required_delay_ns: Nanos, pub rx: Vec<RxStat> }
  pub struct StreamInfo { pub stream_id: u32, pub format: StreamFormat, pub mode: MasterMode, pub presentation_offset_ns: Nanos }
  pub struct OutputInfo { pub device: String, pub format: StreamFormat, pub device_latency_ns: Nanos }
  pub struct RxStat { pub master: NodeId, pub stream_id: u32, pub sync_error_ns: Nanos, pub behind_ns: Nanos, pub lost_frames: u64 }
  pub enum CalibMsg { Request { mic_device: String }, Begin { session: u32, until_ns: Nanos },
      Play { session: u32, at_ns: Nanos, signal: CalibSignal }, End { session: u32 },
      Result { session: u32, deltas: Vec<(NodeId, Nanos)>, error: Option<String> } }
  pub enum CalibSignal { Sweep, Click { freq_hz: u16 } }
  pub enum Packet<'a> { Control(Control), Audio { header: AudioHeader, payload: &'a [u8] }, Silent { header: AudioHeader } }
  pub fn encode(sender: NodeId, p: &Packet<'_>, key: &GroupKey, buf: &mut [u8; MAX_DATAGRAM]) -> Result<usize, EncodeError>;
  pub fn decode<'a>(buf: &'a [u8], key: &GroupKey) -> Result<(NodeId, Packet<'a>), DecodeError>;
  pub enum EncodeError { TooLarge, Serialize }
  pub enum DecodeError { TooShort, BadMagic, BadVersion(u8), BadTag, UnknownType(u8), Malformed }
  pub fn frames_per_packet(channels: u16) -> u16;           // MAX_AUDIO_PAYLOAD / (channels*4)
  pub fn f32_to_le_bytes(src: &[f32], dst: &mut [u8]) -> usize;   // returns bytes written
  pub fn le_bytes_to_f32(src: &[u8], dst: &mut [f32]) -> usize;   // returns samples written
  ```
- **Wire layout:** `[0..4] MAGIC`, `[4] version`, `[5] type`, `[6..14] sender u64 LE`, then the body, then the tag.
  - Type codes: 1 = Control (body is the postcard-encoded `Control`), 16 = Audio (49-byte LE header + payload),
    17 = Silent (header only).
  - Audio header field order: stream_id u32, seq u32, frame_index u64, frames u16, capture_time i64,
    presentation_offset i64, sample_rate u32, channels u16, channel_mask u32, volume f32, flags u8 (bit 0 = muted).
  - Tag = HMAC-SHA256(key, every byte before the tag)[..8], checked in constant time.
  - `GroupKey::derive` = SHA-256 of `"soundsync-v1|" + group + "|" + secret`.

- [ ] **Step 1: Scaffold the workspace.**
  - `Cargo.toml` with `members = ["crates/*"]` and `[workspace.dependencies]` holding every pin from Global
    Constraints (postcard with `alloc`, serde with `derive`), plus the release profile. Add `.gitignore`.
  - `crates/core` depends on serde, postcard, hmac, sha2, thiserror.
  - Add `.github/workflows/build.yml`, a matrix over `windows-latest` and `macos-latest`. Steps: checkout,
    `dtolnay/rust-toolchain@stable` with clippy, `cargo test --workspace`, then
    `cargo clippy --workspace --all-targets -- -D warnings`. Triggers: push, pull_request, workflow_dispatch.
- [ ] **Step 2: Write the failing tests.**
  ```rust
  fn hdr() -> AudioHeader { AudioHeader { stream_id: 7, seq: 42, frame_index: 1_000_000, frames: 43,
      capture_time_ns: 123_456_789, presentation_offset_ns: 80_000_000,
      format: StreamFormat { sample_rate: 48_000, channels: 8, channel_mask: 0x63F }, volume: 0.5, muted: false } }
  #[test] fn audio_roundtrip_preserves_header_and_payload() {
      let key = GroupKey::derive("home", "");
      let samples: Vec<f32> = (0..43*8).map(|i| i as f32 * 1e-3 - 0.1).collect();
      let mut bytes = vec![0u8; samples.len()*4]; f32_to_le_bytes(&samples, &mut bytes);
      let mut buf = [0u8; MAX_DATAGRAM];
      let n = encode(99, &Packet::Audio { header: hdr(), payload: &bytes }, &key, &mut buf).unwrap();
      assert_eq!(n, HEADER_LEN + AUDIO_HEADER_LEN + bytes.len() + TAG_LEN);
      let (sender, p) = decode(&buf[..n], &key).unwrap();
      assert_eq!(sender, 99);
      let Packet::Audio { header, payload } = p else { panic!() };
      assert_eq!(header, hdr());
      let mut out = vec![0f32; samples.len()];
      assert_eq!(le_bytes_to_f32(payload, &mut out), samples.len());
      assert_eq!(out.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), samples.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
  }
  #[test] fn wrong_key_is_rejected()        // encode with "home", decode with "office" → Err(DecodeError::BadTag)
  #[test] fn tampered_byte_is_rejected()    // flip buf[20] → Err(BadTag)
  #[test] fn control_messages_roundtrip()   // every Control variant incl. State{stream: Some, output: Some, rx: 2 items}, Calib::Result → decode == original
  #[test] fn silent_roundtrip()             // Packet::Silent{header} → identical header, n == 14+49+8
  #[test] fn frames_per_packet_fits_payload() { for ch in 1..=8u16 { assert!(frames_per_packet(ch) as usize * ch as usize * 4 <= MAX_AUDIO_PAYLOAD) }
      assert_eq!(frames_per_packet(8), 43); assert_eq!(frames_per_packet(2), 175); }
  #[test] fn oversized_payload_fails_to_encode()  // 1404-byte payload → Err(EncodeError::TooLarge)
  #[test] fn garbage_is_rejected_without_panic()  // 10_000 seeded random buffers, len 0..2000 → all Err
  ```
- [ ] **Step 3: Run the tests and check they fail.** Run `cargo test -p soundsync-core protocol`; it should fail to
  compile because nothing is defined yet.
- [ ] **Step 4: Implement `types.rs` and `protocol.rs`** to the interfaces above.
- [ ] **Step 5: Run the tests and check they pass.** Run `cargo test -p soundsync-core protocol`; all 8 should pass.
- [ ] **Step 6: Checkpoint.** `cargo clippy --workspace --all-targets -- -D warnings` must be clean.

### Task 2: Clock estimator and FrameClock

**Files:**
- Create: `crates/core/src/clock.rs`, `crates/core/src/timeline.rs`
- Test: inline `mod tests` in both

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Copy)] pub struct ClockSample { pub t1: Nanos, pub t2: Nanos, pub t3: Nanos, pub t4: Nanos }
  #[derive(Clone, Copy, Debug, PartialEq)] pub struct ClockMap { pub ref_local: Nanos, pub offset_ns: f64, pub skew: f64 }
  impl ClockMap { pub const IDENTITY: ClockMap;
      pub fn local_to_remote(&self, local: Nanos) -> Nanos;   // local + offset + skew*(local-ref_local)
      pub fn remote_to_local(&self, remote: Nanos) -> Nanos; } // exact inverse
  pub const CLOCK_MIN_SAMPLES: usize = 8; pub const CLOCK_WINDOW: usize = 128;
  impl ClockEstimator { pub fn new() -> Self; pub fn add(&mut self, s: ClockSample); pub fn map(&self) -> Option<ClockMap>;
      pub fn rtt_ns(&self) -> Option<Nanos>; pub fn jitter_ns(&self) -> Option<Nanos>; pub fn reset(&mut self); }
  impl FrameClock { pub fn new(nominal_rate: u32) -> Self; pub fn add(&mut self, frame: u64, time_ns: Nanos);
      pub fn time_of(&self, frame: u64) -> Nanos; pub fn rate_ratio(&self) -> f64; pub fn reset(&mut self); }
  ```
- **Algorithm (estimator):** per sample, θ = ((t2−t1)+(t3−t4))/2 and δ = (t4−t1)−(t3−t2), keeping the last
  `CLOCK_WINDOW` samples.
  - Select the 25% of samples with the lowest δ (at least 4).
  - If the window spans ≥ 2 s, least-squares θ against the local midpoint (t1+t4)/2, with skew clamped to ±500 ppm.
    Otherwise use skew = 0 and offset = the median θ of the selected samples.
  - `rtt_ns` = the minimum δ. `jitter_ns` = the median of |δ − min δ|.
- **Algorithm (FrameClock):** least-squares time against frame over the last 512 points. With fewer than 16 points,
  use the newest point plus the nominal rate.

- [ ] **Step 1: Write the failing tests.**
  ```rust
  #[test] fn estimates_offset_within_50us_with_jitter() {
      // remote = local + 3.7 ms + 80 ppm*(local - start); one-way delays U(100..400 µs) (seed 7), 10% get +2 ms
      // one exchange every 250 ms for 60 s → final map:
      assert!((map.remote_to_local(r_now) - local_now).abs() < 50_000);
      assert!((map.skew - 80e-6).abs() < 5e-6);
  }
  #[test] fn not_synced_before_min_samples()   // 7 samples → map() None; 8 → Some
  #[test] fn reset_clears_state()              // after reset, map() None
  #[test] fn identity_map_roundtrips()         // IDENTITY.remote_to_local(x) == x; map.local_to_remote(map.remote_to_local(x)) within 1 ns
  #[test] fn frame_clock_smooths_jittery_timestamps() {
      // 48 kHz × (1+50 ppm), 480-frame packets, ±200 µs uniform timestamp noise (seed 3), 5 s
      assert!((fc.time_of(f) - true_time(f)).abs() < 20_000);
  }
  #[test] fn frame_clock_reset_on_discontinuity() // reset(); add(0, T) → time_of(480) == T + 10 ms ± 1 µs
  ```
- [ ] **Step 2: Run the tests and check they fail.** `cargo test -p soundsync-core clock timeline` should not compile.
- [ ] **Step 3: Implement** `clock.rs` and `timeline.rs` using the algorithms above.
- [ ] **Step 4: Run the tests and check they pass.** `cargo test -p soundsync-core clock timeline`.
- [ ] **Step 5: Checkpoint.** Clippy must be clean.

### Task 3: JitterBuffer

**Files:** Create `crates/core/src/jitter.rs`. Tests go in its `mod tests`.

**Interfaces:**
- Produces:
  ```rust
  pub enum Insert { Stored, TooOld, Reset }
  impl JitterBuffer {
      pub fn new(channels: usize, capacity_frames: usize) -> Self;   // all allocation here
      pub fn insert(&mut self, frame_index: u64, samples: &[f32]) -> Insert;
      pub fn insert_silence(&mut self, frame_index: u64, frames: usize) -> Insert;
      pub fn read(&mut self, frame_index: u64, out: &mut [f32]) -> usize;   // returns missing frames (zero-filled)
      pub fn discard_before(&mut self, frame_index: u64);
      pub fn end_frame(&self) -> Option<u64>;                          // one past the newest stored frame
      pub fn clear(&mut self); }
  ```
- **Semantics:**
  - Frame f lives at ring slot f % capacity, with one presence flag per frame.
  - An insert older than `end − capacity` or below the discard point returns `TooOld`.
  - An insert more than `capacity` past `end` clears the buffer, stores the data and returns `Reset`.
  - No allocation after `new`.

- [ ] **Step 1: Write the failing tests:**
  - `in_order_read_returns_samples`
  - `late_packet_fills_hole_before_read`: insert frames 0..43, then 86..129, then 43..86; `read(0, 129 frames)`
    returns 0 missing, and the values match.
  - `missing_frames_are_zero_and_counted`: a hole of 43 frames gives 43 missing and zeros.
  - `too_old_insert_is_rejected`: `discard_before(1000)`, then inserting frame 500 → `TooOld`.
  - `far_future_insert_resets`: capacity 4800, data at 0..43; insert at 10_000_000 → `Reset`; reading frame 0
    afterwards reports it missing.
  - `silence_insert_marks_present_zero`
  - `no_allocation_after_new`: insert and read 10k times; the pointer to the internal buffer stays the same
    (`as_ptr`).
- [ ] **Step 2: Run** `cargo test -p soundsync-core jitter`; it should fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p soundsync-core jitter`; it should pass.
- [ ] **Step 5: Checkpoint.**

### Task 4: Channel layouts and the soft limiter

**Files:** Create `crates/core/src/layout.rs` and `crates/core/src/limiter.rs`. Tests go inline.

**Interfaces:**
- Produces:
  ```rust
  pub mod speaker { pub const FL: u32=0x1; pub const FR: u32=0x2; pub const FC: u32=0x4; pub const LFE: u32=0x8;
      pub const BL: u32=0x10; pub const BR: u32=0x20; pub const FLC: u32=0x40; pub const FRC: u32=0x80;
      pub const BC: u32=0x100; pub const SL: u32=0x200; pub const SR: u32=0x400; }
  pub fn effective_mask(f: StreamFormat) -> u32;      // mask, or default for channel count if mask==0 or popcount≠channels
  pub fn default_mask(channels: u16) -> u32;          // 1→FC, 2→0x3, 4→0x33, 6→0x3F, 8→0x63F, other→first N bits
  pub fn describe(f: StreamFormat) -> String;         // "Mono","Stereo","Quad","5.1","7.1", else "N ch (0xMASK)"
  pub fn mask_from_coreaudio_labels(labels: &[u32]) -> u32;
  pub enum MapKind { Identity, Substituted, Downmix, Upmix }
  impl ChannelMap { pub fn new(src: StreamFormat, dst: StreamFormat) -> ChannelMap;
      pub fn kind(&self) -> MapKind; pub fn is_mismatch(&self) -> bool;   // Downmix | Upmix
      pub fn coeff(&self, dst_ch: usize, src_ch: usize) -> f32;
      pub fn apply_add(&self, src: &[f32], dst: &mut [f32], gain: f32); } // interleaved frames, dst += M·src·gain
  impl SoftLimiter { pub fn new(sample_rate: u32) -> Self; pub fn process(&mut self, buf: &mut [f32], channels: usize); }
  ```
- **Mapping rules:** channel order follows ascending mask bits.
  - Identical effective masks → Identity.
  - SL↔BL and SR↔BR substitute 1:1 when the destination lacks one and has the other → Substituted.
  - Any remaining source speaker missing from the destination is downmixed:
    - FC → FL and FR at 0.7071
    - surrounds → the same-side front at 0.7071
    - BC → both surrounds (or both fronts) at 0.5
    - FLC/FRC → FL/FR at 1.0
    - LFE → dropped
    - a mono destination gets FL+FR at 0.7071
  - Destination speakers with no source stay silent (Upmix places only matching channels).
- **CoreAudio labels:** 1 = L → FL, 2 = R → FR, 3 = C → FC, 4 = LFEScreen → LFE.
  - 5 = Ls and 6 = Rs map to SL/SR if the layout also contains 33/34 (Rls/Rrs → BL/BR); otherwise to BL/BR.
  - 7 = Lc → FLC, 8 = Rc → FRC, 9 = Cs → BC, 10 = LeftSurroundDirect → SL, 11 = RightSurroundDirect → SR.
- **Limiter:** per frame, target = 1/peak when peak > 1, else 1. `gain = min(target, released gain)` with a 50 ms
  release; gain snaps to exactly 1.0 once it is within 1e-6. A sample is multiplied only when gain ≠ 1.0.

- [ ] **Step 1: Write the failing tests:**
  - `identical_masks_are_identity`: 0x63F→0x63F, kind Identity, `apply_add` with gain 1.0 into a zeroed dst is
    bit-exact.
  - `side_and_back_substitute`: 0x3F→0x60F gives kind Substituted, BL→SL coefficient 1.0, and
    `is_mismatch() == false`; same for 0xFF→0x63F.
  - `surround_to_stereo_downmix_itu`: 0x3F→0x3. FL row: FL 1.0, FC 0.7071, BL 0.7071, LFE 0.0. Kind Downmix,
    `is_mismatch() == true`.
  - `stereo_to_71_places_front_only`: kind Upmix, FL→FL 1.0, every other destination row 0.
  - `zero_mask_uses_default_for_channel_count`
  - `describe_names_common_layouts`: "7.1", "5.1", "Stereo", and "3 ch (0x7)".
  - `coreaudio_51_and_71_labels`: `[1,2,3,4,5,6]` → 0x3F; `[1,2,3,4,5,6,33,34]` → 0x63F.
  - `limiter_is_bit_exact_below_full_scale`: 1 s of seeded random |x| ≤ 1.0 keeps identical bits.
  - `limiter_caps_peaks`: 1.8 + 0.9 summed signals → every |y| ≤ 1.0.
  - `limiter_releases_to_unity`: one loud frame, then 1 s at 0.5 → the last 100 ms is bit-exact.
- [ ] **Step 2: Run** `cargo test -p soundsync-core layout limiter`; it should fail.
- [ ] **Step 3: Implement** to the rules above.
- [ ] **Step 4: Run** the tests; they should pass.
- [ ] **Step 5: Checkpoint.**

### Task 5: SyncController and StreamPlayer

**Files:**
- Create: `crates/core/src/sync.rs`, `crates/core/src/player.rs`
- Modify: `crates/core/Cargo.toml` (add rubato)
- Test: inline. Put the player simulation helpers in `player.rs` under `mod tests`.

**Interfaces:**
- Consumes: `AudioHeader` (Task 1), `ClockMap` (Task 2), `JitterBuffer` (Task 3), `ChannelMap` (Task 4).
- Produces:
  ```rust
  #[derive(Clone, Copy)] pub struct SyncParams { pub kp_ppm_per_ms: f64, pub ki_ppm_per_ms_s: f64, pub max_ppm: f64,
      pub deadband_ns: Nanos, pub resync_ns: Nanos, pub lowpass_alpha: f64 }
  impl Default for SyncParams   // kp 40, ki 4, max 300, deadband 500_000, resync 15_000_000, alpha 0.05
  pub enum SyncDecision { Realign, Ratio(f64) }      // relative resample ratio, 1.0 = nominal
  impl SyncController { pub fn new(p: SyncParams) -> Self;
      pub fn update(&mut self, error_ns: Nanos, dt_ns: Nanos) -> SyncDecision; pub fn reset(&mut self); }
  pub struct PlayerConfig { pub out: StreamFormat, pub chunk_frames: usize, pub jitter_capacity_frames: usize,
      pub sync: SyncParams, pub fade_frames: usize }
  impl PlayerConfig { pub fn for_output(out: StreamFormat) -> Self }   // chunk 64, capacity 2 s, fade 5 ms
  pub struct RenderTiming { pub now_ns: Nanos, pub dac_time_ns: Nanos }  // Copy
  pub enum PlayState { Waiting, Playing, Behind }
  pub struct RenderReport { pub state: PlayState, pub error_ns: Nanos, pub missing_frames: u32, pub realigned: bool }
  pub struct PlayerStats { pub min_slack_ns: Option<Nanos>, pub behind_ns: Nanos, pub lost_frames: u64,
      pub realigns: u64, pub ratio: f64, pub presentation_offset_ns: Nanos }
  impl StreamPlayer {
      pub fn new(src: StreamFormat, cfg: PlayerConfig) -> Self;   // allocates everything; render path never allocates
      pub fn src_format(&self) -> StreamFormat;
      pub fn layout_kind(&self) -> MapKind;
      pub fn push(&mut self, h: &AudioHeader, samples: &[f32], arrival_ns: Nanos, clock: &ClockMap, extra_latency_ns: Nanos); // empty samples = silent span of h.frames
      pub fn render_add(&mut self, out: &mut [f32], t: RenderTiming, clock: &ClockMap, extra_latency_ns: Nanos, gain: f32) -> RenderReport;
      pub fn stats(&self) -> PlayerStats; }
  ```
- **Error sign:** `error_ns = scheduled_dac_time − target_time` for the source frame at the output head. Positive
  means late, so `Ratio < 1.0` (consume input faster). Negative means early, so `Ratio > 1.0`.
- **Target:** for source frame f, `target_dac = clock.remote_to_local(capture_time(f)) + presentation_offset −
  extra_latency + behind_ns`.
  - `capture_time(f)` and `presentation_offset` come from the latest header:
    `h.capture_time_ns + (f − h.frame_index)·1e9/src_rate`.
- **Head position:** source frames consumed − (resampler `output_delay()` + output FIFO frames) × src_rate /
  out_rate. The base ratio is out_rate/src_rate; the controller multiplies it via `set_resample_ratio_relative`.
- **Start or Realign:** solve for the frame f0 whose `target_dac == t.dac_time_ns`.
  - If f0 ≥ `end_frame`, output nothing and report Waiting.
  - Otherwise drop frames up to f0, reset the resampler and controller, and fade in.
  - If f0 is older than the oldest buffered frame (the data arrived after its deadline), set
    `behind_ns = (oldest − f0) frames in ns + 2 ms`, start from the oldest frame and report Behind.
  - `behind_ns` decays back to 0 only through a realign once slack allows it.
- **Slack:** at push, `slack = target_dac(first frame) − arrival_ns − lead`, where lead = `dac_time − now` from the
  last render. Track the minimum over a rolling 10 s window (two 5 s buckets).

- [ ] **Step 1: Write the failing controller tests:**
  - `late_error_speeds_up`: error +2 ms → `Ratio(r)` with r < 1.0.
  - `early_error_slows_down`
  - `ratio_is_capped_at_300ppm`: error 10 ms for 1000 updates → |r − 1| ≤ 300e-6.
  - `deadband_keeps_ratio_steady`: errors alternating ±0.3 ms for 100 updates → r changes by < 1e-7.
  - `large_error_requests_realign`: error 16 ms → `Realign`.
- [ ] **Step 2: Write the failing player tests.** The sim helper sets src 48 kHz, 43-frame packets,
  presentation_offset 50 ms, identity clock, and output periods of 480 frames with dac_time advancing at the
  device rate × (1 + dev_ppm). The source places a +0.8 one-sample pulse every 100 ms; onset times come from
  output frame dac times with linear interpolation.
  ```rust
  #[test] fn tracks_200ppm_drift_within_200us() { for ppm in [-200.0, 200.0] { let r = sim(SimCfg { dev_ppm: ppm, secs: 40.0, ..Default::default() });
      for (cap, emit) in r.pulses_after(20.0) { assert!((emit - (cap + 50_000_000)).abs() < 200_000); } } }
  #[test] fn prealigns_on_start()            // first pulse after playback starts already within 200 µs
  #[test] fn realigns_after_50ms_jump()      // presentation_offset 50→100 ms at t=10 s → report.realigned once; pulses after t=11 s within 200 µs of cap+100 ms
  #[test] fn waits_with_silence_until_data_due()   // renders before target produce exact zeros and PlayState::Waiting
  #[test] fn reports_behind_when_data_late() // offset 5 ms, arrival = capture + 20 ms → PlayState::Behind, 15 ms ≤ stats.behind_ns ≤ 25 ms, output continuous (no zero runs > 1 period after start)
  #[test] fn converts_44k1_to_48k_keeping_timing() // src 44.1 kHz, out 48 kHz → pulses within 200 µs
  #[test] fn push_computes_slack()           // arrival 30 ms before target, lead 10 ms → min_slack ≈ 20 ms ± 1 ms
  #[test] fn long_gap_realigns_without_growth() // play 5 s, no packets for 600 s of sim time, then frame_index restarts at 0 with new capture times → realigned, pulses within 200 µs again, JitterBuffer capacity unchanged
  #[test] fn layout_mapping_applied()        // src 0x3F → out 0x3: FC content appears in FL/FR at 0.7071
  ```
- [ ] **Step 3: Run** `cargo test -p soundsync-core sync player`; it should fail.
- [ ] **Step 4: Implement `SyncController::update`.** It applies the low-pass filter (alpha) to the error. Outside
  the deadband, it computes `ppm = −(kp·e_ms + ki·∫e_ms·dt)` and clamps it to ±max_ppm; inside the deadband, the
  integrator holds. |raw error| > resync means Realign. Return `Ratio(1 + ppm·1e-6)`.
- [ ] **Step 5: Implement `StreamPlayer`** with rubato `Async<f32>` using the parameters in Global Constraints,
  `max_resample_ratio_relative = 1.01`, a preallocated output FIFO, `process_into_buffer`, `ChannelMap::apply_add`,
  and 5 ms linear fades on start and realign.
- [ ] **Step 6: Run** the tests and tune kp/ki only if the drift test fails. They should pass. If the gains change,
  record the final values in the `SyncParams::default` doc comment.
- [ ] **Step 7: Checkpoint.**

### Task 6: DelayPolicy

**Files:** Create `crates/core/src/delay.rs`. Tests go inline.

**Interfaces:**
- Produces:
  ```rust
  pub enum DelaySetting { Auto, Manual(f64) }   // ms; serde: "auto" | number (custom impl)
  pub const MIN_DELAY_NS: Nanos = 20_000_000; pub const MAX_DELAY_NS: Nanos = 2_000_000_000; pub const DELAY_QUANTUM_NS: Nanos = 5_000_000;
  impl DelayPolicy { pub fn new(setting: DelaySetting, margin_ns: Nanos) -> Self;
      pub fn on_stream_start(&mut self, requirements: &[Nanos]) -> Nanos;
      pub fn update(&mut self, requirements: &[Nanos]) -> Option<Nanos>;   // Some only when D changed
      pub fn set_setting(&mut self, s: DelaySetting) -> Nanos; pub fn current(&self) -> Option<Nanos>; }
  ```
- Auto D = clamp(ceil to 5 ms of (max(req) + margin), 20 ms, 2000 ms). `update` only increases D in Auto mode.
  Manual D = ms × 1e6, clamped.

- [ ] **Step 1: Write the failing tests:**
  - `auto_takes_max_requirement_plus_margin_quantized`: [12 ms, 37.2 ms], margin 5 ms → 45 ms.
  - `auto_never_decreases_in_update`
  - `auto_increases_when_requirement_grows`: returns `Some(160 ms)` for [150 ms].
  - `stream_start_recomputes_downward`
  - `manual_overrides`
  - `clamped_to_min_and_max`: [] → 20 ms; [5 s] → 2000 ms.
  - `delay_setting_serde`: TOML `"auto"`, `80`, `80.5` all parse; `"fast"` is an error.
- [ ] **Step 2: Run** `cargo test -p soundsync-core delay`; it should fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests; they should pass.
- [ ] **Step 5: Checkpoint.**

### Task 7: Config and paths

**Files:**
- Create: `crates/core/src/config.rs`, `crates/core/src/paths.rs`
- Modify: `crates/core/Cargo.toml` (add toml, directories, rand)
- Test: inline (use temp dirs under `std::env::temp_dir()` with unique names)

**Interfaces:**
- Produces:
  ```rust
  #[serde(default)] pub struct Config { pub node: NodeSection, pub network: NetworkSection, pub master: MasterSection, pub playback: PlaybackSection, pub sync: SyncSection }
  pub struct NodeSection { pub id: u64 /*0 = generate*/, pub name: String /*"" = hostname*/ }
  pub struct NetworkSection { pub group: String /*"default"*/, pub key: String /*""*/, pub port: u16 /*47810*/, pub interface: String /*"auto"*/, pub static_peers: Vec<String> }
  pub struct MasterSection { pub enabled: bool /*false*/, pub mode: MasterMode /*Virtual*/, pub capture_device: String /*"" = "CABLE Input" in Virtual, default output in Direct*/,
      pub monitor_device: String /*""=default*/, pub switch_default: bool /*false*/, pub keepalive_low_latency: bool /*true*/ }
  pub struct PlaybackSection { pub output_device: String, pub volume: f32 /*1.0, 0..=1*/, pub muted: bool, pub follow_master_volume: bool /*true*/,
      pub play_remote: bool /*true*/, pub extra_latency_ms: f64 /*0, -1000..=2000*/, pub low_latency: bool /*false*/ }
  pub struct SyncSection { pub delay_ms: DelaySetting /*Auto*/, pub margin_ms: f64 /*5.0, 0..=100*/ }
  pub struct LoadOutcome { pub config: Config, pub created: bool, pub warnings: Vec<String> }
  impl Config { pub fn load_or_create(path: &Path) -> io::Result<LoadOutcome>; pub fn save(&self, path: &Path) -> io::Result<()>;
      pub fn validate(&mut self) -> Vec<String>; pub fn regenerate_id(&mut self); pub fn group_key(&self) -> GroupKey; }
  pub struct Paths { pub config_file: PathBuf, pub log_dir: PathBuf, pub state_file: PathBuf }
  impl Paths { pub fn for_instance(instance: Option<&str>) -> Paths; }
  ```
- `save` writes atomically: write a temp file, then rename. `load_or_create` strips a UTF-8 BOM. When a file
  doesn't parse, it renames it to `config.toml.bad-<unix secs>` and writes defaults, with a warning.

- [ ] **Step 1: Write the failing tests:**
  - `missing_file_creates_defaults_with_random_id`: created=true, id ≠ 0, name non-empty, and the file exists.
  - `roundtrip_preserves_values`
  - `delay_auto_and_number_parse`
  - `bom_unknown_keys_and_bad_values_are_tolerated`: `"\u{FEFF}[network]\nport = 0\nfoo = 1\n[playback]\nvolume = 5.0\nextra_latency_ms = -50000\n"`
    → port 47810, volume 1.0, extra_latency_ms −1000.0, and warnings mention `network.port`, `playback.volume` and
    `playback.extra_latency_ms`.
  - `invalid_toml_falls_back_and_backs_up`
  - `regenerate_id_changes_and_persists`
  - `paths_with_instance_suffix`: the directory name ends with `SoundSync-b`.
  - `group_key_depends_on_group_and_secret`
- [ ] **Step 2: Run** `cargo test -p soundsync-core config paths`; it should fail.
- [ ] **Step 3: Implement.** Paths use `directories::BaseDirs`: `config_dir()` on Windows (Roaming) and
  `data_dir()` on macOS for config; `data_local_dir()` for state on Windows; `~/Library/Logs` on macOS.
- [ ] **Step 4: Run** the tests; they should pass.
- [ ] **Step 5: Checkpoint.**

### Task 8: Calibration DSP

**Files:**
- Create: `crates/core/src/calib.rs`
- Modify: `crates/core/Cargo.toml` (add realfft)
- Test: inline

**Interfaces:**
- Produces:
  ```rust
  pub fn log_sweep(sample_rate: u32, duration_s: f64, f0_hz: f64, f1_hz: f64) -> Vec<f32>;  // amplitude 0.5, 5 ms raised-cosine fades
  pub fn click(sample_rate: u32, freq_hz: f64) -> Vec<f32>;                                   // 10 ms Hann-windowed sine, amplitude 0.5
  pub struct Detection { pub offset_frames: f64, pub snr_db: f32 }
  pub fn find_signal(recording: &[f32], reference: &[f32]) -> Option<Detection>;  // FFT xcorr + parabolic peak; None if snr < 12 dB
  pub enum CalibReference { Median, Node(NodeId) }
  pub fn solve_offsets(residuals: &[(NodeId, Nanos)], reference: CalibReference) -> Vec<(NodeId, Nanos)>; // delta_n = residual_n − residual_ref
  ```
- SNR is the peak against the RMS of the correlation, excluding ±10 ms around the peak. The standard sweep is
  `log_sweep(rate, 0.1, 200.0, 8000.0)`.

- [ ] **Step 1: Write the failing tests:**
  - `finds_sweep_delay_within_half_ms_in_noise`: 48 kHz, sweep gain 0.1 at 12_345.6 frames (fractional, via
    resampled insertion), white noise 0.01 (seed 11) → |offset − 12_345.6| < 24 frames, snr > 12.
  - `returns_none_when_absent`
  - `solve_offsets_median_reference`: residuals {1: 10 ms, 2: 45 ms, 3: 190 ms} → {1: −35 ms, 2: 0, 3: +145 ms}.
  - `solve_offsets_node_reference`
  - `click_is_short_and_bounded`: len 480 at 48 kHz, max |x| ≤ 0.5.
- [ ] **Step 2: Run** `cargo test -p soundsync-core calib`; it should fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests; they should pass.
- [ ] **Step 5: Checkpoint.**

---

## Phase 2 — Audio I/O (`soundsync-audio`)

### Task 9: The AudioHost API, device selection, NullOutput and FakeHost

**Files:**
- Create: `crates/audio/Cargo.toml`, `crates/audio/src/{lib.rs,select.rs,null.rs,fake.rs}`
- Test: inline in `select.rs` and `fake.rs`

**Interfaces:**
- Produces (`lib.rs`):
  ```rust
  pub use soundsync_core::{Nanos, StreamFormat, player::RenderTiming};
  pub enum Direction { Render, Capture }
  #[derive(Clone, Debug)] pub struct DeviceInfo { pub id: String, pub name: String, pub direction: Direction, pub is_default: bool,
      pub active: bool, pub mix_format: StreamFormat, pub default_period_ns: Nanos, pub min_period_ns: Nanos }
  pub trait RenderCallback: Send + 'static { fn render(&mut self, out: &mut [f32], format: StreamFormat, t: RenderTiming); }
  #[derive(Clone, Copy)] pub struct CaptureInfo { pub time_ns: Nanos, pub raw_time_ns: Nanos, pub frames: usize, pub silent: bool, pub discontinuity: bool }
  pub trait CaptureSink: Send + 'static { fn on_capture(&mut self, data: &[f32], format: StreamFormat, info: CaptureInfo); }
  pub struct OutputOptions { pub low_latency: bool }   pub struct LoopbackOptions { pub keepalive_low_latency: bool }
  pub trait StreamHandle: Send { fn format(&self) -> StreamFormat; fn latency_ns(&self) -> Nanos; fn is_alive(&self) -> bool; }
  pub type Stream = Box<dyn StreamHandle>;   // Drop stops the stream and joins its thread
  pub enum DeviceEvent { DefaultChanged(Direction), Added(String), Removed(String), StateChanged(String) }
  pub enum AudioError { NotFound(String), Ambiguous(Vec<String>), Unsupported(&'static str), Os(String) }  // thiserror
  pub trait AudioHost: Send + Sync {
      fn now_ns(&self) -> Nanos;
      fn devices(&self, dir: Direction) -> Result<Vec<DeviceInfo>, AudioError>;
      fn open_output(&self, sel: &DeviceSelector, opts: OutputOptions, cb: Box<dyn RenderCallback>) -> Result<Stream, AudioError>;
      fn open_loopback(&self, sel: &DeviceSelector, opts: LoopbackOptions, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError>;
      fn open_input(&self, sel: &DeviceSelector, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError>;
      fn probe_loopback_latency(&self, sel: &DeviceSelector) -> Result<Nanos, AudioError>;   // loopback timestamp → DAC
      fn endpoint_volume(&self, sel: &DeviceSelector) -> Result<(f32, bool), AudioError>;
      fn active_sessions(&self, sel: &DeviceSelector) -> Result<usize, AudioError>;   // other processes' active render sessions on the endpoint
      fn watch(&self, on_event: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Result<Box<dyn Send>, AudioError>; }
  pub fn default_host() -> Result<Arc<dyn AudioHost>, AudioError>;   // WinHost / MacHost; Err(Unsupported) elsewhere
  ```
- Produces (`select.rs`):
  ```rust
  pub enum DeviceSelector { Default, Null, Id(String), Name(String) }
  impl DeviceSelector { pub fn parse(s: &str) -> Self }   // ""|"default"→Default, "null"→Null, "id:…"→Id, else Name
  pub fn select_device<'a>(devices: &'a [DeviceInfo], sel: &DeviceSelector) -> Result<&'a DeviceInfo, AudioError>;
  ```
  Matching order: an exact case-insensitive name match among active devices, then among all devices, then a unique
  case-insensitive substring among active devices. Several substring matches → `Ambiguous(names)`.
- Produces (`null.rs`): `pub fn open_null_output(now: Arc<dyn Fn() -> Nanos + Send + Sync>, format: StreamFormat,
  cb: Box<dyn RenderCallback>) -> Stream`.
  - A thread with a 10 ms period calls `cb` with frames computed from elapsed time and
    `dac_time = now + 10 ms`. The default format is 48 kHz stereo.
  - Every host's `open_output(DeviceSelector::Null, ..)` delegates here.
- Produces (`fake.rs`):
  ```rust
  pub struct FakeClock { pub offset_ns: Nanos, pub skew_ppm: f64 }
  pub struct FakeOutputSpec { pub name: String, pub format: StreamFormat, pub ppm: f64, pub reported_latency_ns: Nanos, pub hidden_latency_ns: Nanos, pub period_ns: Nanos }
  pub enum FakeSignal { Silence, Pulses { every_ms: u32, amplitude: f32 }, Sine { hz: f32, amplitude: f32 } }
  pub struct FakeSourceSpec { pub name: String, pub format: StreamFormat, pub ppm: f64, pub signal: FakeSignal, pub probe_offset_ns: Nanos, pub hidden_latency_ns: Nanos }
  impl FakeHost { pub fn new(clock: FakeClock) -> Arc<FakeHost>;
      pub fn add_output(&self, s: FakeOutputSpec); pub fn add_source(&self, s: FakeSourceSpec);
      pub fn onsets(&self, device: &str) -> Vec<Nanos>;          // true-time emissions (rising edge > 0.25 on ch0)
      pub fn source_pulses(&self, source: &str) -> Vec<Nanos>; } // true capture times of generated pulses
  pub fn true_now() -> Nanos;                                     // process-wide reference clock
  impl AudioHost for FakeHost                                     // now_ns = true_now + offset + skew·(true_now − t0)
  ```
- **Fake output:** a thread wakes every `period_ns`.
  - The device consumes frames at rate × (1 + ppm) in true time and keeps 2 periods buffered.
  - `dac_time` of frame W = host-clock(t_start + W/r_dev) + reported_latency.
  - Emission true time = t_start + W/r_dev + reported + hidden.
- **Fake source:** delivers 10 ms buffers. A pulse is a single sample at +0.8 every `every_ms`, whose capture true
  time is recorded. `CaptureInfo.time_ns` is in the host clock.
  - The source also records its own emissions at capture + probe_offset + hidden, standing in for "Windows plays
    it directly" in direct mode.
  - `probe_loopback_latency` returns `probe_offset_ns`; `endpoint_volume` returns (1.0, false).
  - `active_sessions` returns the value set with `FakeHost::set_active_sessions(name, n)` (default 0).

- [ ] **Step 1: Write the failing tests:**
  - `select_prefers_active_exact_match`: two devices named "LG TV SSCR2 (NVIDIA High Definition Audio)", only the
    second active → returns the second.
  - `select_substring_unique`: "steam" → "Speakers (Steam Streaming Speakers)".
  - `select_ambiguous_errors`: "HDMI" matching two active devices → `Ambiguous` listing both.
  - `select_default_and_parse`
  - `fake_output_dac_times_follow_device_rate`: ppm 100; the dac-time difference across 48_000 frames is
    1e9/(1+1e-4) ± 0.1 ms.
  - `fake_source_pulses_at_known_capture_times`
  - `fake_clock_offset_applies`: offset +7 ms → `host.now_ns() − true_now()` ≈ 7 ms ± 0.1 ms.
  - `null_output_calls_back_at_real_time_rate`: over 1 s, frames ≈ 48_000 ± 2%.
- [ ] **Step 2: Run** `cargo test -p soundsync-audio`; it should fail.
- [ ] **Step 3: Implement** `lib.rs`, `select.rs`, `null.rs` and `fake.rs`.
- [ ] **Step 4: Run** `cargo test -p soundsync-audio`; it should pass.
- [ ] **Step 5: Checkpoint.**

### Task 10: Windows device listing and `soundsync devices`

**Files:**
- Create: `crates/audio/src/win/{mod.rs,com.rs,enumerate.rs}`, `crates/app/Cargo.toml`,
  `crates/app/src/{main.rs,cli.rs}`
- Modify: `crates/audio/Cargo.toml` (`[target.'cfg(windows)'.dependencies]` wasapi, windows 0.62 with features
  `Win32_Media_Audio`, `Win32_System_Com`, `Win32_Devices_FunctionDiscovery`, `Win32_UI_Shell_PropertiesSystem`,
  `Win32_System_Threading`, `Win32_Foundation`, `Win32_System_Performance`)

**Interfaces:**
- Produces: `pub struct WinHost` implementing `AudioHost`. `win::now_ns()` = QPC × 1e9 / frequency, with the
  frequency cached. `default_host()` returns a `WinHost` on Windows. `active_sessions` returns `Unsupported` until
  Task 24.
- Produces (app): a clap `Cli { #[arg(long)] config: Option<PathBuf>, #[arg(long)] instance: Option<String>,
  #[arg(long)] show: bool, #[arg(long)] quit: bool, #[command(subcommand)] cmd: Option<Cmd> }` with
  `enum Cmd { Devices, Tone{..}, Measure{..}, Run{..}, GenIcon{..} }`. Only `Devices` is implemented in this task;
  the others print "not implemented yet".
- COM is initialized per thread (MTA) by a `com::ComGuard`. Device info comes from the endpoint's mix format
  (WAVEFORMATEXTENSIBLE mask and channels), `GetDevicePeriod`, and `DEVPKEY_Device_FriendlyName`. All states are
  listed, with `active` meaning DEVICE_STATE_ACTIVE.

- [ ] **Step 1: Write a hardware test** marked `#[ignore]` (CI runners have no audio):
  `enumerates_render_devices_with_formats` asserts at least 1 active render device, every active device has
  channels ≥ 1, `sample_rate ∈ {44100, 48000, 88200, 96000, 176400, 192000}`, and
  `min_period_ns ≤ default_period_ns`.
- [ ] **Step 2: Implement** `WinHost::new()` and `devices()`. The other trait methods return `Unsupported` until
  their tasks.
- [ ] **Step 3: Implement** `soundsync devices`, which prints for each direction:
  `* [active] Name — 7.1 48000 Hz, period 10.0/3.0 ms`, with `*` marking the default.
- [ ] **Step 4: Verify on this PC.**
  - Run `cargo test -p soundsync-audio -- --ignored enumerates`; it should pass.
  - Run `cargo run -p soundsync -- devices`. The output should list LG TV SSCR2 (one active, one not), Steam
    Streaming Speakers, HDMI (Live Gamer Ultra-Audio) and Digital Output (Realtek USB2.0 Audio), each with its
    layout.
- [ ] **Step 5: Checkpoint.**

### Task 11: Windows timed render and `soundsync tone`

**Files:**
- Create: `crates/audio/src/win/{render.rs,mmcss.rs}`
- Modify: `crates/audio/src/win/mod.rs`, `crates/app/src/cli.rs`

**Interfaces:**
- Produces: `WinHost::open_output`.
  - Event-driven shared mode in the device mix format (f32), on its own thread with MMCSS "Pro Audio"
    (`mmcss::ProAudioGuard`).
  - `IAudioClient3::InitializeSharedAudioStream` at the minimum period only when `opts.low_latency`.
  - Each event: available frames = buffer − padding. Call `cb.render` with a `RenderTiming` where
    `dac_time_ns(next frame W) = qpc_ns(pos) + (W − pos_frames)·1e9/rate`. Here pos comes from
    `IAudioClock::GetPosition`, divided by `GetFrequency`, with its QPC time.
  - Do **not** add `GetStreamLatency`: the IAudioClock position already tracks hardware playback, and calibration
    absorbs any remainder. `latency_ns()` reports `GetStreamLatency` (raw COM via `as_iaudioclient()`) for display
    only.
  - `AUDCLNT_E_DEVICE_INVALIDATED` → thread exits and `is_alive()` = false.
- Produces (CLI): `soundsync tone --device <sel> [--freq 440] [--seconds 3] [--pulses]` renders a −20 dBFS sine (or
  pulses every 200 ms) and prints the format, the average period and the dac-time slope.

- [ ] **Step 1: Write a hardware test** marked `#[ignore]`, `render_reports_monotonic_dac_times`, on "Steam
  Streaming Speakers": over 2 s, dac times strictly increase, and the per-callback step = frames/rate ± 1 ms.
- [ ] **Step 2: Implement** `render.rs` and `mmcss.rs`.
- [ ] **Step 3: Implement** `soundsync tone`.
- [ ] **Step 4: Verify (silent device only).**
  - Run `cargo test -p soundsync-audio -- --ignored render_reports`; it should pass.
  - Run `cargo run -p soundsync -- tone --device "Steam Streaming Speakers" --seconds 2`. It should exit 0, print a
    slope ≈ 1.000 ± 0.001, and play nothing audible.
- [ ] **Step 5: Checkpoint.**

### Task 12: Windows loopback capture, keep-alive, timing probe and `soundsync measure`

**Files:**
- Create: `crates/audio/src/win/{loopback.rs,probe.rs}`
- Modify: `crates/app/src/cli.rs`

**Interfaces:**
- Consumes: `FrameClock` (Task 2), `find_signal` (Task 8), render (Task 11).
- Produces: `WinHost::open_loopback`.
  - Loopback capture on the render endpoint, event-driven shared mode, reading into a preallocated buffer.
  - `BufferInfo.timestamp` × 100 → ns goes into `FrameClock`, which gives `CaptureInfo.time_ns`.
  - The `SILENT` flag → deliver zeros with `silent = true`.
  - `DATA_DISCONTINUITY` or `TIMESTAMP_ERROR` → `FrameClock::reset` with `discontinuity = true`.
  - A keep-alive render stream writes zeros to the same endpoint, using IAudioClient3 at the minimum period when
    `keepalive_low_latency` (falling back silently to the standard period).
- Produces: `probe::probe_loopback(host: &WinHost, sel: &DeviceSelector) -> Result<ProbeResult, AudioError>` with
  `pub struct ProbeResult { pub offset_ns: Nanos, pub bit_exact: bool, pub gain: f32 }`.
  - It renders a 4096-sample seeded PRBS at amplitude 0.25 starting at a known frame W, and finds it in loopback.
  - `offset_ns = predicted dac_time(W) − capture time of the detected burst start`.
  - `bit_exact` = the captured samples equal the rendered ones.
  - `gain` = RMS(captured)/RMS(rendered), which shows whether endpoint volume is applied before loopback.
  - `WinHost::probe_loopback_latency` returns `offset_ns`.
- Produces (CLI):
  - `soundsync measure --device <sel>` prints the ProbeResult.
  - `soundsync measure --a <sel> --b <sel> [--seconds 5]` loopback-captures both endpoints, cross-correlates
    them, and prints the relative delay of B against A.

- [ ] **Step 1: Write a hardware test** marked `#[ignore]`, `loopback_of_own_render_is_bit_exact`, on Steam
  Streaming Speakers: `probe_loopback` gives `bit_exact == true`, |gain − 1| < 1e-6, and
  |offset_ns| < 200 ms.
- [ ] **Step 2: Implement** `loopback.rs`.
- [ ] **Step 3: Implement** `probe.rs` and the `measure` CLI.
- [ ] **Step 4: Verify.** Run `cargo run -p soundsync -- measure --device "Steam Streaming Speakers"` and record
  offset, bit_exact and gain in the task report.
  - If `bit_exact` is false or gain ≠ 1, stop and tell the user. This answers the spec's open question of whether
    loopback here is pre- or post-volume.
- [ ] **Step 5: Checkpoint.**

### Task 13: Windows mic capture (RAW), endpoint volume and device notifications

**Files:**
- Create: `crates/audio/src/win/{input.rs,volume.rs,notify.rs}`
- Modify: `windows` features (`implement`)

**Interfaces:**
- Produces:
  - `WinHost::open_input`: event-driven shared capture. RAW mode is set via
    `IAudioClient2::SetClientProperties(AUDCLNT_STREAMOPTIONS_RAW)` when `IsOffloadCapable`/RAW is supported;
    otherwise it logs once and continues. Timestamps go through `FrameClock`.
  - `WinHost::endpoint_volume`: `IAudioEndpointVolume::GetMasterVolumeLevelScalar` + `GetMute`.
  - `WinHost::watch`: an `IMMNotificationClient` (`#[implement]`) that maps callbacks to `DeviceEvent` and
    unregisters on drop.

- [ ] **Step 1: Write hardware tests** marked `#[ignore]`:
  - `endpoint_volume_reads_scalar`: 0 ≤ v ≤ 1 for the default render device.
  - `input_delivers_timestamps_if_present`: skipped when there is no active capture device.
  - `watch_registers_and_drops`
- [ ] **Step 2: Implement** the three files.
- [ ] **Step 3: Run** `cargo test -p soundsync-audio -- --ignored`; every hardware test should pass, or skip with a
  printed reason.
- [ ] **Step 4: Checkpoint.**

---

## Phase 3 — Engine (`soundsync-engine`)

### Task 14: Engine skeleton: transport, peers, HELLO, clock service and status

**Files:**
- Create: `crates/engine/Cargo.toml`, `crates/engine/src/{lib.rs,command.rs,status.rs,net.rs,peers.rs,node.rs}`,
  `crates/engine/tests/{common/mod.rs,clock_sync.rs}`

**Interfaces:**
- Consumes: protocol (Task 1), clock (Task 2), Config (Task 7), AudioHost/FakeHost (Task 9).
- Produces:
  ```rust
  pub struct NetImpair { pub loss: f64, pub reorder: f64, pub jitter_ns: Nanos, pub seed: u64 }
  pub struct EngineOptions { pub config: Config, pub config_path: Option<PathBuf>, pub state_path: Option<PathBuf>,
      pub host: Arc<dyn AudioHost>, pub enable_mdns: bool, pub bind_ip: Option<Ipv4Addr>, pub impair: Option<NetImpair> }
  impl Engine { pub fn start(o: EngineOptions) -> anyhow::Result<Engine>; pub fn command(&self, c: Command);
      pub fn status(&self) -> Arc<Status>; pub fn local_addr(&self) -> SocketAddr; pub fn shutdown(self); }
  pub enum Command { SetBroadcast(bool), SetMasterMode(MasterMode), SetCaptureDevice(String), SetMonitorDevice(String),
      SetOutputDevice(String), SetVolume(f32), SetMuted(bool), SetFollowMasterVolume(bool), SetPlayRemote(bool),
      SetExtraLatency { node: NodeId, ns: Nanos }, SetDelay(DelaySetting), SetSwitchDefault(bool),
      SyncTest(bool), StartCalibration { mic_node: NodeId, mic_device: String }, ApplyCalibration, CancelCalibration }
  pub struct Status { pub node_id: NodeId, pub name: String, pub group: String, pub os: Os, pub broadcasting: bool,
      pub master_mode: MasterMode, pub capture: Option<DeviceStatus>, pub output: Option<DeviceStatus>, pub delay_ns: Option<Nanos>,
      pub peers: Vec<PeerStatus>, pub streams: Vec<StreamStatus>, pub warnings: Vec<String>, pub calibration: CalibStatus,
      pub sync_test: bool, pub inbound_ok: bool, pub volume: f32, pub muted: bool, pub extra_latency_ns: Nanos }
  pub struct DeviceStatus { pub name: String, pub format: StreamFormat, pub latency_ns: Nanos }
  pub struct PeerStatus { pub id: NodeId, pub name: String, pub os: Os, pub addr: SocketAddr, pub role: Role,
      pub rtt_ns: Option<Nanos>, pub jitter_ns: Option<Nanos>, pub clock_offset_ns: Option<f64>,
      pub extra_latency_ns: Nanos, pub output: Option<OutputInfo>, pub rx: Vec<RxStat> }
  pub struct StreamStatus { pub master: NodeId, pub stream_id: u32, pub format: StreamFormat, pub state: PlayState,
      pub error_ns: Nanos, pub behind_ns: Nanos, pub lost_frames: u64, pub layout_warning: Option<String> }
  pub enum CalibStatus { Idle, Running { step: usize, total: usize }, Done { session: u32, deltas: Vec<(NodeId, Nanos)> }, Failed(String) }
  // net.rs
  pub trait PacketSink: Send + Sync { fn send_datagram(&self, to: SocketAddr, bytes: &[u8]); }
  impl Transport { pub fn bind(ip: Ipv4Addr, port: u16, key: GroupKey, node_id: Arc<AtomicU64>, impair: Option<NetImpair>) -> io::Result<Arc<Transport>>;
      pub fn send_control(&self, to: SocketAddr, c: &Control); pub fn local_addr(&self) -> SocketAddr; }
  pub struct Inbound { pub from: SocketAddr, pub sender: NodeId, pub arrival_ns: Nanos, pub control: Control }
  pub fn spawn_rx(t: Arc<Transport>, host: Arc<dyn AudioHost>, ctrl: Sender<Inbound>, routes: Receiver<RxCmd>) -> JoinHandle<()>;
  pub enum RxCmd { AddRoute { master: NodeId, stream_id: u32, tx: rtrb::Producer<AudioChunk> }, RemoveRoute { master: NodeId, stream_id: u32 }, Stop }
  ```
  `AudioChunk` lives in `chunk.rs` (Task 16), but the routing type is declared here. Create `chunk.rs` in this task
  with the struct only.
- **Behaviour:**
  - The socket has `SO_RCVBUF` = 4 MiB and a 50 ms read timeout.
  - Peers come from `static_peers` (`host:port`) plus discovery (Task 15). An unknown address gets a HELLO;
    `reply = true` answers it.
  - A peer becomes active only after an authenticated HELLO with the same group.
  - PING every 250 ms (50 ms during the first 2 s); PONG answers with t2/t3 from `host.now_ns()`. Peers expire
    after 5 s with no packets.
  - **Duplicate id:** an authenticated packet whose sender equals our id but comes from a different HELLO `session`
    means a collision. The node with the numerically lower session regenerates its id, saves the config if
    `config_path` is set, and pushes the warning `"duplicate node id detected; regenerated"`.
  - `Status` is published through `ArcSwap` every 100 ms. In this task the audio commands only log
    `"not implemented"`.
  - Create `chunk.rs` with the `AudioChunk` struct exactly as given in Task 16's Interfaces.
  - `tests/common/mod.rs`:
    - `spawn(name, clock: FakeClock, setup: impl FnOnce(&FakeHost)) -> TestNode` and
      `spawn_linked(name, clock, peers: &[&TestNode], setup) -> TestNode`.
    - A `TestNode` holds `{ engine, host: Arc<FakeHost>, dir: TempDir-like PathBuf }`. The engine is bound to
      127.0.0.1 on port 0 with mDNS off.
    - `spawn_linked` puts each earlier node's `local_addr()` into `static_peers`. The reverse direction comes from
      HELLO replies.
    - Also `wait_until(timeout, cond) -> bool`.

- [ ] **Step 1: Write the failing integration tests** (`tests/clock_sync.rs`):
  ```rust
  #[test] fn two_engines_handshake_and_sync_clocks() {
      let a = spawn("a", FakeClock { offset_ns: 0, skew_ppm: 0.0 }, |_| {});
      let b = spawn_linked("b", FakeClock { offset_ns: 3_700_000, skew_ppm: 60.0 }, &[&a], |_| {});
      wait_until(Duration::from_secs(6), || a.engine.status().peers.iter().any(|p| p.name == "b" && p.clock_offset_ns.is_some()));
      sleep(Duration::from_secs(3));
      let off = a.engine.status().peers[0].clock_offset_ns.unwrap();
      let truth = (b.host.now_ns() - a.host.now_ns()) as f64;   // sampled back-to-back
      assert!((off - truth).abs() < 100_000.0);
  }
  #[test] fn mismatched_group_key_never_peers()   // group "home" vs "office" → both peer lists empty after 3 s
  #[test] fn duplicate_node_id_regenerates()      // same node.id in both configs → after 4 s ids differ; exactly one has the warning; its config file on disk holds the new id
  #[test] fn peer_expires_after_5s()              // b.shutdown() → a.peers empty within 6.5 s
  ```
- [ ] **Step 2: Run** `cargo test -p soundsync-engine --test clock_sync`; it should fail.
- [ ] **Step 3: Implement** `net.rs`, `peers.rs`, `node.rs`, `status.rs`, `command.rs` and `lib.rs`.
- [ ] **Step 4: Run** the tests; all 4 should pass.
- [ ] **Step 5: Checkpoint.**

### Task 15: Discovery (mDNS and interface filtering)

**Files:**
- Create: `crates/engine/src/discovery.rs`
- Modify: `node.rs` (feed `found` addresses into peers; filter peer addresses against local nets)

**Interfaces:**
- Produces:
  ```rust
  pub struct LocalNet { pub name: String, pub ip: Ipv4Addr, pub prefix: u8 }
  pub fn iface_allowed(name: &str, ip: Ipv4Addr) -> bool;
  pub fn local_networks(cfg_interface: &str) -> Vec<LocalNet>;   // "auto" → allowed ifaces; else name substring or IP that matches exactly (bypasses filter)
  pub fn addr_on_local_net(addr: Ipv4Addr, nets: &[LocalNet]) -> bool;
  pub fn inbound_suspect(now: Nanos, first_contact_attempt: Option<Nanos>, last_authenticated_rx: Option<Nanos>) -> bool; // attempt ≥ 10 s ago and nothing received since

  impl Discovery { pub fn start(nets: &[LocalNet], node_id: NodeId, name: &str, group: &str, port: u16, role: Role) -> anyhow::Result<Discovery>;
      pub fn found(&self) -> &Receiver<SocketAddr>; pub fn update_role(&self, role: Role); pub fn shutdown(self); }
  ```
- **Filter:** reject names containing (case-insensitive) `vethernet`, `wsl`, `hyper-v`, `docker`, `virtualbox`,
  `vmware`, `tailscale`, `zerotier`, `wireguard`, `openvpn`, `tap-`, `tun`, `loopback` or `npcap`. Reject IPs in
  127/8, 169.254/16 and 100.64/10.
- **mDNS:**
  - `ServiceDaemon::new()`, then `disable_interface(IfKind::IPv6)` and disable each rejected interface by name.
  - Register instance `<name>-<id:016x>` with TXT `id`, `group`, `ver`, `os`, `role`.
  - Browse the service type. A `ServiceResolved` with matching `group` and an address on a local net goes to
    `found`.
- `EngineOptions.enable_mdns = false` in all automated tests.
- `node.rs` sets `Status.inbound_ok = !inbound_suspect(..)`. When it's false, it adds the warning "No replies from
  other SoundSync devices — Windows Firewall may be blocking SoundSync (run install-windows.ps1 without
  -NoFirewall)". On macOS the text is "…allow Local Network access".

- [ ] **Step 1: Write the failing tests:**
  - `inbound_suspect_after_10s_without_rx`: true at attempt + 10 s with no rx; false if rx came after the attempt;
    false with no attempt.
  - `filters_virtual_adapters`: table cases — "vEthernet (WSL)" false, "Ethernet" true, "Tailscale" false,
    "Wi-Fi" true, 100.101.1.2 false.
  - `allows_named_interface_override`: `local_networks("vEthernet (WSL)")` keeps it when it exists. Write it as a
    pure helper test over a supplied iface list: `filter_networks(ifaces, cfg)`.
  - `local_net_membership`: 192.168.1.50/24 contains .7, not 192.168.2.7.
- [ ] **Step 2: Run** `cargo test -p soundsync-engine discovery`; it should fail.
- [ ] **Step 3: Implement**, with `local_networks` built on `filter_networks(if_addrs::get_if_addrs(), cfg)`.
- [ ] **Step 4: Run** the tests; they should pass. Manual mDNS verification happens in Task 20.
- [ ] **Step 5: Checkpoint.**

### Task 16: Sender (capture to packets to subscribers)

**Files:**
- Create: `crates/engine/src/{leases.rs,sender.rs}`
- Modify: `crates/engine/src/chunk.rs`
- Test: inline (`RecordingSink: PacketSink`)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Copy)] pub struct AudioChunk { pub header: AudioHeader, pub arrival_ns: Nanos, pub len: u16, pub samples: [f32; 350] }  // len 0 = silent span
  impl Leases { pub fn new(ttl_ns: Nanos) -> Self; pub fn renew(&mut self, a: SocketAddr, now: Nanos); pub fn active(&self, now: Nanos) -> Vec<SocketAddr>; }  // ttl 3 s
  pub struct SenderShared { pub subscribers: ArcSwap<Vec<SocketAddr>>, pub presentation_offset_ns: AtomicI64,
      pub volume_bits: AtomicU32, pub muted: AtomicBool, pub stream_id: AtomicU32, pub format: ArcSwap<Option<StreamFormat>> }
  impl CaptureSender { pub fn new(node_id: Arc<AtomicU64>, sink: Arc<dyn PacketSink>, shared: Arc<SenderShared>,
      local: Option<rtrb::Producer<AudioChunk>>, now: Arc<dyn Fn() -> Nanos + Send + Sync>) -> Self; }
  impl CaptureSink for CaptureSender
  ```
- **Behaviour (on the capture thread, no allocation after the first buffer):**
  - Split incoming frames into `frames_per_packet(ch)` packets, carrying the remainder across calls.
  - `frame_index` runs continuously. `capture_time = info.time_ns + offset_in_buffer·1e9/rate`.
  - `seq` increments for each packet.
  - Encode once, then `send_datagram` to every subscriber, and push an identical `AudioChunk` to `local` when it
    is present. The `arrival_ns` stamp uses the `now` closure.
  - **Silence:** after 2 s of continuous `silent` or all-zero data, send `Silent` packets (header only, `frames` =
    10 ms worth) instead of audio. Real audio resumes on the first non-zero buffer.
  - **Format change:** when a buffer's format differs from `shared.format`, pick a new random stream_id, reset
    frame_index to 0 and store the new format.

- [ ] **Step 1: Write the failing tests:**
  - `leases_expire_after_3s`
  - `packetizes_8ch_into_43_frame_packets`: 480 frames → 11 packets of 43, and 7 frames carried over. The next
    call starts with `frame_index == 473` (11 × 43). All frames total match and payload bits are exact.
  - `capture_times_follow_info_time`
  - `silence_suppressed_after_2s`: 2.5 s of zeros → the last ~0.5 s go out as `Silent` packets whose frames sum to
    the real frame count.
  - `audio_resumes_immediately_after_silence`
  - `format_change_starts_new_stream`: stereo then 8ch → the stream_id changes and frame_index restarts at 0.
  - `local_producer_receives_same_chunks`
- [ ] **Step 2: Run** `cargo test -p soundsync-engine sender leases`; it should fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests; they should pass.
- [ ] **Step 5: Checkpoint.**

### Task 17: Receiver mixer (render callback)

**Files:**
- Create: `crates/engine/src/receiver.rs`
- Test: inline (synthetic chunks, identity clock)

**Interfaces:**
- Consumes: `StreamPlayer`, `SoftLimiter`, `AudioChunk`.
- Produces:
  ```rust
  pub struct MixerShared { pub volume_bits: AtomicU32, pub muted: AtomicBool, pub extra_latency_ns: AtomicI64,
      pub follow_master_volume: AtomicBool, pub paused: AtomicBool, pub lead_ns: AtomicI64 }
  pub struct SlotStats { pub error_ns: AtomicI64, pub behind_ns: AtomicI64, pub lost_frames: AtomicU64, pub min_slack_ns: AtomicI64,
      pub state: AtomicU8, pub format_changed: AtomicBool, pub layout_kind: AtomicU8, pub presentation_offset_ns: AtomicI64 }
  pub struct StreamSlot { pub master: NodeId, pub stream_id: u32, pub player: StreamPlayer, pub rx: rtrb::Consumer<AudioChunk>,
      pub clock: Arc<ArcSwap<ClockMap>>, pub stats: Arc<SlotStats>, pub gain: f32 }
  pub struct ScheduledSignal { pub samples: Vec<f32>, pub emit_at_ns: Nanos }   // mono → all channels; emit_at in local clock, before extra latency
  pub enum MixerCmd { AddStream(Box<StreamSlot>), RemoveStream { master: NodeId, stream_id: u32 }, Play(Box<ScheduledSignal>) }
  impl Mixer { pub fn new(out: StreamFormat, shared: Arc<MixerShared>, cmds: rtrb::Consumer<MixerCmd>, garbage: rtrb::Producer<MixerGarbage>) -> Self; }
  pub enum MixerGarbage { Slot(Box<StreamSlot>), Signal(Box<ScheduledSignal>) }   // engine thread drops these
  impl RenderCallback for Mixer
  ```
- **Each render:**
  1. Drain commands; `AddStream` with an existing key replaces the old slot, which goes to garbage.
  2. Store `lead_ns`.
  3. Zero the output.
  4. For each slot: drain `rx` into `player.push` (empty samples when `len == 0`).
     - A chunk whose format ≠ `player.src_format()` sets `format_changed`, and the slot outputs nothing until it is
       replaced.
     - Unless `paused`, call `render_add` with gain × (header volume, or 0 if muted, when following master volume).
  5. Mix scheduled signals at `emit_at − extra_latency`, aligned by dac time to ±1 frame, then send them to garbage
     once finished.
  6. Apply node volume and mute.
  7. Run `SoftLimiter`.

- [ ] **Step 1: Write the failing tests:**
  - `mixes_two_streams_sum`: constants 0.25 and 0.5 → steady-state output 0.75 ± 1e-3.
  - `limiter_engages_on_overload`: 0.8 + 0.8 → ≤ 1.0.
  - `follow_master_volume`: header volume 0.5 → output halved when following, unchanged when not; `muted` → zero.
  - `format_change_flags_slot`
  - `paused_outputs_silence_but_keeps_timing`: after unpausing, the next pulse is still within 200 µs.
  - `scheduled_signal_plays_at_target`: the first non-zero sample lands at the frame whose dac_time is closest to
    `emit_at − extra_latency`, ±1 frame.
  - `replaced_slot_goes_to_garbage`
- [ ] **Step 2: Run** `cargo test -p soundsync-engine receiver`; it should fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests; they should pass.
- [ ] **Step 5: Checkpoint.**

### Task 18: Streaming orchestration (virtual and direct modes, gossip, delay policy, offsets)

**Files:**
- Create: `crates/engine/src/streams.rs`, `crates/engine/tests/streaming.rs`
- Modify: `node.rs`, `status.rs`

**Interfaces:**
- Consumes: Tasks 14, 16 and 17, `DelayPolicy` (Task 6) and `ChannelMap::is_mismatch` (Task 4).
- **Behaviour:**
  - **Output:** each node opens one output with the `Mixer`. Its device is `master.monitor_device` while
    broadcasting in Virtual mode, and `playback.output_device` otherwise. The output is reopened when the
    configured device or mode changes.
    - It is opened with `OutputOptions { low_latency: playback.low_latency }`.
    - If the device returns `NotFound` or `Ambiguous`, open `Default` instead, with the warning
      `"<name> not found — using default output"`. Retrying it is Task 27.
  - **`SetBroadcast(true)`** (only when `open_loopback` is supported):
    - Virtual mode opens loopback on `capture_device`, or Name("CABLE Input") when that is empty. If it's missing,
      add the warning `"VB-CABLE (CABLE Input) not found — install it or switch to Direct mode"` and leave
      `capture` as None.
    - Direct mode opens loopback on `capture_device`, or Default.
    - A `CaptureSender` is created with `local = Some(..)` in Virtual mode only, which plays our own stream through
      the mixer.
  - **Presentation offset:**
    - Virtual: `DelayPolicy`, whose requirements are this node's own requirement plus `required_delay_ns` from the
      STATE of every peer subscribed to us.
    - Direct: `host.probe_loopback_latency(capture)` (probed once per capture open) + our own extra latency.
  - **Receivers:**
    - Every 1 s, send `Subscribe` to each active peer whose STATE has `stream: Some`, when `play_remote` is set.
      Skip it when this node direct-captures the same device it outputs to; that adds the warning
      `"Remote streams muted here: this PC is capturing its own output device (echo guard)"`.
    - Create a slot when the first STATE or chunk for the stream appears (a `RxCmd::AddRoute` with a
      `rtrb::RingBuffer::<AudioChunk>::new(512)` pair).
    - Remove it when the peer stops advertising the stream, the stream id changes, or `format_changed` is set
      (then rebuild it).
    - A slot's clock is the master peer's `ClockMap` (an identity map for the local stream), refreshed in its
      `ArcSwap` every 250 ms.
  - **`required_delay_ns`:**
    - When the node has slack data: `max over slots(presentation_offset − min_slack) + 2 ms`.
    - Otherwise: `output latency + lead_ns + extra_latency + 10 ms`.
  - **STATE** every 1 s carries role, stream, output, extra latency, required delay and rx stats.
  - **Status streams:** `layout_warning = Some(format!("{} → {}", describe(src), describe(out)))` when
    `is_mismatch`; it is also pushed to the warnings once per stream.
  - **`SetExtraLatency{node}`:** our own id updates `MixerShared` and the config. Another id sends `SetOffset` to
    that peer, and the receiving node applies and saves it.
  - **`SetVolume`/`SetMuted`/`SetFollowMasterVolume`/`SetPlayRemote`/`SetDelay`/`SetMasterMode`/`Set*Device`**
    update shared state, persist to config, and reopen streams where needed.
    - `SetPlayRemote(false)` sends `Unsubscribe` for every subscribed stream and removes their slots.
  - In the direct-mode test, M's FakeHost has both `add_source("Speakers")` and `add_output("Speakers")`. The echo
    guard compares the resolved device names.
  - The master polls `endpoint_volume(capture)` every 250 ms into `SenderShared`.
- Test helper in `tests/common`: `assert_in_sync(nodes, device_names, window, tol_ns)`. It pairs each source pulse
  (true capture time) with each output's onsets and checks |emission − (capture + D)| < tol, after the warm-up.

- [ ] **Step 1: Write the failing integration tests** (`tests/streaming.rs`; FakeHost sources are 48 kHz stereo,
  with pulses every 200 ms):
  - `virtual_mode_three_outputs_in_sync`:
    - M has source "CABLE Input" and monitor "TV" (ppm +50, reported 15 ms).
    - R1 has output "Laptop" (ppm −120, reported 40 ms).
    - R2 has "Speaker" (ppm +80, reported 5 ms, clock offset +7 ms).
    - Pulses between 6 and 10 s on all three outputs are within 1 ms of capture + D.
    - `status.delay_ns == Some(D)` with D % 5 ms == 0 and D ≥ 45 ms.
  - `virtual_mode_survives_loss_and_reorder`: R1 impair loss 0.01, reorder 0.02, jitter 2 ms. The same 1 ms bound
    holds for every detected pulse; R1 `lost_frames > 0`; no panics.
  - `direct_mode_reports_behind`:
    - M is direct with source "Speakers" (probe_offset 20 ms) as its capture and output.
    - R1 reported latency 60 ms → `behind_ns ∈ [40, 60] ms`.
    - R2 reported 5 ms → R2 onsets within 1 ms of M's source emissions.
    - M's `status.warnings` contains the echo-guard text.
  - `auto_delay_increases_when_slow_receiver_joins`: start M + R2, then spawn R1 with 150 ms latency. Within 4 s,
    D ≥ 155 ms and all outputs are back in sync (1 ms bound for pulses after the increase plus 1 s).
  - `master_leaving_removes_stream`: `SetBroadcast(false)` → receivers' `status.streams` is empty within 4 s.
  - `remote_set_offset_applies_and_persists`: from M, `SetExtraLatency{node: R1, ns: 25 ms}` → R1's status and R1's
    config file both show 25 ms within 2 s.
  - `layout_mismatch_warns`: source 0x3F into a stereo output → `layout_warning` is "5.1 → Stereo".
- [ ] **Step 2: Run** `cargo test -p soundsync-engine --test streaming`; it should fail.
- [ ] **Step 3: Implement** `streams.rs` and extend `node.rs` and `status.rs`.
- [ ] **Step 4: Run** the tests; they should pass. If a timing test is flaky on CI, widen only the warm-up, never
  the 1 ms tolerance.
- [ ] **Step 5: Checkpoint.**

### Task 19: Calibration, sync test and the fake room

**Files:**
- Create: `crates/engine/src/{calibration.rs,synctest.rs}`, `crates/engine/tests/calibration.rs`
- Modify: `crates/audio/src/fake.rs`

**Interfaces:**
- Produces (fake.rs):
  ```rust
  pub struct FakeRoom;  impl FakeRoom { pub fn new(sample_rate: u32) -> Arc<FakeRoom>; }
  impl FakeHost { pub fn attach_room(&self, room: Arc<FakeRoom>); pub fn add_mic(&self, name: &str, hidden_input_latency_ns: Nanos); }
  ```
  - Every output on a host with a room adds its channel-0 emission samples into the room at their true emission
    time. A mic reads the room sum delayed by its hidden input latency.
- **Calibration flow (`calibration.rs`):**
  - `StartCalibration{mic_node == self}` makes this node the coordinator. Otherwise it sends
    `Calib::Request{mic_device}` to `mic_node`.
  - The coordinator opens the mic, picks a session id, and sends `Begin{until}` to all active peers, which set
    `paused`.
  - Node k (sorted by id, self included) gets `Play{at: t0 + 1.5 s + k·1.0 s, Sweep}` in the coordinator's clock.
    Each node converts it with its clock map of the coordinator and sends `MixerCmd::Play` with the 0.1 s sweep.
  - The coordinator records until the last slot + 1 s. For each k it runs `find_signal` over the window
    [T_k − 50 ms, T_k + 700 ms], with times taken from the mic `CaptureInfo` FrameClock times.
  - residual_k = arrival − T_k. Deltas come from `solve_offsets` with reference Median (Virtual) or the current
    master's node (Direct).
  - It broadcasts `Result`, sends `End`, and unpauses. Status becomes `Done`, or `Failed("no signal from
    <name>")` if any detection fails, and streams are always unpaused.
  - `ApplyCalibration` sends `SetExtraLatency{node, current + delta}` for each delta.
  - Warn in status if the mic device name contains "Hands-Free" or "Headset", since a Bluetooth HFP mic changes
    the output's latency.
- **Sync test (`synctest.rs`):** `SyncTest(true)` broadcasts `SyncTest{active, epoch_ns}`, with the epoch at now +
  1 s in the sender's clock.
  - Every node pauses streams and, while active, schedules a click at each epoch + n·1 s. The pitch is
    600 + 200·(rank of node id among peers mod 6) Hz.
  - Clicks are scheduled ≤ 2 s ahead.
  - `SyncTest(false)` stops it and unpauses.

- [ ] **Step 1: Write the failing integration tests:**
  - `calibration_recovers_hidden_latencies`:
    - Three nodes share one `FakeRoom`, with hidden output latencies 0 ms, 35 ms and 180 ms.
    - The mic sits on node 0 with a hidden input latency of 12 ms.
    - After calibration and apply, every pairwise difference of extra latencies matches the difference of hidden
      latencies within ±0.5 ms.
    - A following streaming run passes `assert_in_sync` using true emission times, 1 ms tolerance.
  - `calibration_fails_cleanly_without_signal`: node 2's output volume is 0 → `Failed("no signal from …")` and
    every node's `paused == false`.
  - `remote_mic_node_runs_calibration`: `StartCalibration{mic_node: node0}` issued on node 2 → node 2's status
    reaches `Done`.
  - `sync_test_clicks_coincide`: with correct extra latencies, click onsets across the nodes fall within 1 ms.
- [ ] **Step 2: Run** `cargo test -p soundsync-engine --test calibration`; it should fail.
- [ ] **Step 3: Implement** the FakeRoom, `calibration.rs` and `synctest.rs`.
- [ ] **Step 4: Run** the tests; they should pass.
- [ ] **Step 5: Checkpoint.**

### Task 20: Headless `soundsync run` and a real check on this PC

**Files:**
- Create: `crates/app/src/headless.rs`
- Modify: `crates/app/src/cli.rs`

**Interfaces:**
- Produces: `soundsync run [--broadcast] [--mode virtual|direct] [--capture SEL] [--monitor SEL] [--output SEL]
  [--port N] [--no-mdns] [--seconds N]`.
  - It loads the config for `--instance`/`--config`, applies the CLI overrides in memory only (never saving them),
    and starts the engine with `default_host()`.
  - Every 2 s it prints one line per peer and per stream: name, rtt, offset, state, error, behind, lost.
  - It exits on Ctrl+C or after `--seconds`, shutting the engine down cleanly.

- [ ] **Step 1: Implement** `headless.rs` and the `run` CLI.
- [ ] **Step 2: Real check, silent only.** Run three terminals from the release build:
  - A: `soundsync run --instance a --broadcast --mode virtual --capture "Steam Streaming Speakers" --monitor null --seconds 40`
  - B: `soundsync run --instance b --port 47811 --output null --seconds 40`
  - C: `soundsync tone --device "Steam Streaming Speakers" --pulses --seconds 30`

  Expected:
  - A and B find each other through **mDNS** on the LAN interface (no static peers), and each shows the other's rtt.
  - B shows A's stream as `Playing` with |error| < 1 ms, behind 0 and lost 0 after warm-up.
  - A reports D ≥ 20 ms.
  - Record the output in the task report. If mDNS fails, rerun with `--no-mdns` plus `static_peers` to separate
    discovery problems from streaming ones, then fix discovery.
- [ ] **Step 3: Checkpoint.**

---

## Phase 4 — App (`soundsync`)

### Task 21: Logs and single instance

**Files:**
- Create: `crates/app/src/{logging.rs,single_instance.rs}`
- Modify: `main.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct RotateOutcome { pub previous_crashed: bool, pub archived: Option<PathBuf> }
  pub fn rotate_logs(dir: &Path, now: SystemTime) -> io::Result<RotateOutcome>;  // soundsync.log → soundsync-<UTC yyyymmdd-hhmmss>[-recovered].log, keep newest 5 archives
  pub fn init_logging(dir: &Path) -> anyhow::Result<LogGuard>;  // creates `.running` marker; LogGuard::drop writes clean exit + removes marker
  pub enum Instance { Primary(InstanceGuard), Secondary }
  pub fn acquire(config_path: &Path) -> anyhow::Result<Instance>;   // Windows: mutex + events; macOS: File::try_lock on <state dir>/soundsync.lock
  impl InstanceGuard { pub fn on_show(&self, f: impl Fn() + Send + 'static); pub fn on_quit(&self, f: impl Fn() + Send + 'static); }
  pub fn signal_show(config_path: &Path) -> bool; pub fn signal_quit(config_path: &Path) -> bool;
  ```
- `main`: parse the CLI, then:
  - `--quit` → `signal_quit`, then exit.
  - Otherwise `acquire`. Secondary → `signal_show`, then exit 0. Primary → `init_logging`, then the GUI (Task 23)
    or a subcommand.
- Subcommands (`devices`/`tone`/`measure`/`run`/`gen-icon`) skip single instance.

- [ ] **Step 1: Write the failing tests:**
  - `rotation_keeps_five_archives`
  - `crashed_session_gets_recovered_suffix`: a `.running` marker exists → the archive name ends `-recovered.log`.
  - `clean_session_plain_archive`
  - `#[cfg(windows)] second_acquire_is_secondary_and_signals_show`: a unique temp config path; `acquire` →
    Primary with an `on_show` counter; a second `acquire` → Secondary; `signal_show` → the counter reaches 1
    within 1 s.
- [ ] **Step 2: Run** `cargo test -p soundsync logging single_instance`; it should fail.
- [ ] **Step 3: Implement.** On Windows: `CreateMutexW`/`CreateEventW`, a waiter thread on both events, and the
  names from Global Constraints.
- [ ] **Step 4: Run** the tests; they should pass.
- [ ] **Step 5: Checkpoint.**

### Task 22: Autostart and the Windows build and install scripts

**Files:**
- Create: `crates/app/src/autostart/{mod.rs,windows.rs}`, `scripts/build-windows.ps1`, `scripts/install-windows.ps1`
- Modify: app `windows` features (`Win32_UI_Shell`, `Win32_System_Com_StructuredStorage`)

**Interfaces:**
- Produces:
  ```rust
  pub enum AutostartError { ForeignShortcut(PathBuf), Io(io::Error), Os(String) }
  pub fn is_enabled() -> Result<bool, AutostartError>;
  pub fn set_enabled(enabled: bool) -> Result<(), AutostartError>;
  #[cfg(windows)] pub fn set_enabled_in(startup_dir: &Path, exe: &Path, enabled: bool) -> Result<(), AutostartError>;
  #[cfg(windows)] pub fn is_enabled_in(startup_dir: &Path, exe: &Path) -> Result<bool, AutostartError>;
  ```
- **Windows behaviour:**
  - The shortcut is `<FOLDERID_Startup>\SoundSync.lnk`, created through IShellLinkW + IPersistFile. Its target is
    the current exe with no arguments, so it starts hidden in the tray, and the working directory is the exe's
    directory.
  - An existing `.lnk` whose target is a different, still-existing file → `ForeignShortcut`. A target that no
    longer exists is overwritten.
- **`build-windows.ps1`:**
  - Runs `cargo build --release -p soundsync`, refusing while `soundsync.exe` from `dist` is running.
  - Stages `dist\windows\SoundSync\{soundsync.exe, install-windows.ps1, README.md}` and zips it to
    `dist\SoundSync-windows-x64.zip`.
- **`install-windows.ps1`:**
  - Parameters: `-Remove`, `-NoStartup`, `-NoStartMenu`, `-NoFirewall`, `-Start`, `-SourceExe <path>` (default:
    next to the script), `-InstallRoot <dir>`. `-InstallRoot` redirects the install dir *and* the shortcut dirs, for
    testing.
  - Installs to `%LOCALAPPDATA%\Programs\SoundSync\soundsync.exe`.
  - Shortcuts via `WScript.Shell`: the Start Menu gets `SoundSync.lnk --show` and Startup gets `SoundSync.lnk`. An
    `Assert-OwnedShortcut` refuses foreign targets, following the Toolbox-Apps `install-startup.ps1` pattern.
  - Unless `-NoFirewall`, it runs one elevated `Start-Process powershell -Verb RunAs` that removes, then re-adds,
    `New-NetFirewallRule -DisplayName "SoundSync (UDP in)" -Direction Inbound -Program <installed exe>
    -Protocol UDP -LocalPort 47810,5353 -Profile Private,Domain -Action Allow`.
  - `-Remove` deletes the owned shortcuts, the installed exe and (elevated, unless `-NoFirewall`) the rule. It keeps
    config and logs.

- [ ] **Step 1: Write the failing tests:**
  - `#[cfg(windows)] create_detect_and_remove_shortcut_in_temp_dir`
  - `#[cfg(windows)] refuses_foreign_shortcut`: point the `.lnk` at an existing other exe (`cmd.exe`) →
    `ForeignShortcut`.
  - `#[cfg(windows)] overwrites_stale_shortcut`
- [ ] **Step 2: Run** `cargo test -p soundsync autostart`; it should fail.
- [ ] **Step 3: Implement** `autostart` and both scripts.
- [ ] **Step 4: Verify the scripts** without touching the real Startup folder or the firewall:
  - `pwsh scripts/build-windows.ps1` → the zip exists.
  - `powershell -File dist\windows\SoundSync\install-windows.ps1 -InstallRoot $env:TEMP\ss-install -NoFirewall`
    → the exe and both `.lnk` exist under that root.
  - Rerun it with `-Remove` → both are gone.
- [ ] **Step 5: Checkpoint.**

### Task 23: GUI and tray

**Files:**
- Create: `crates/app/src/ui/{mod.rs,app.rs,panels.rs,tray.rs,format.rs}`, `crates/app/src/icon.rs`,
  `crates/app/build.rs`
- Modify: `main.rs`, app `Cargo.toml` (deps: eframe, tray-icon, raw-window-handle; build-deps: winresource, ico)

**Interfaces:**
- Consumes: Engine/Command/Status (Tasks 14–19), autostart (Task 22), single instance (Task 21).
- Produces:
  ```rust
  pub fn icon_rgba(size: u32) -> Vec<u8>;   // procedural: rounded square #1E88E5, white speaker glyph + 2 arcs; used by tray, window, build.rs .ico, gen-icon
  pub fn run_gui(ctx: AppContext) -> anyhow::Result<()>;
  pub struct AppContext { pub paths: Paths, pub config: Config, pub host: Arc<dyn AudioHost>, pub engine: Engine, pub instance: InstanceGuard, pub start_visible: bool }
  // format.rs (pure)
  pub fn fmt_ms(ns: Nanos) -> String;          // 12.3 ms ("—" for None via fmt_opt_ms)
  pub fn sync_label(s: &StreamStatus) -> String; // "Waiting…" | "In sync (±0.2 ms)" | "Behind 45 ms"
  pub fn role_label(r: Role) -> &'static str;  // "Listener" | "Master (VB-CABLE)" | "Master (direct)"
  pub fn tray_status_line(st: &Status) -> String; // e.g. "Broadcasting · 2 listeners" | "Playing from Living Room" | "Idle · 3 nodes"
  ```
- **Window** (eframe 0.37, 960×640, title "SoundSync"):
  - **Header:** node name, group, and a `tray_status_line`.
  - **"This PC"** (Windows only):
    - A Broadcast toggle.
    - A Mode radio: "VB-CABLE (full sync)" / "Direct capture (lowest latency, partial sync)".
    - Capture device combo: render devices, with VB-CABLE first. Monitor device combo: render devices + "None
      (null)".
    - The switch-default checkbox and the VB-CABLE helper (Task 24).
    - Layout labels and warnings.
  - **"Playback":** output device combo (render devices + "Default"), volume slider, mute, "Follow master volume",
    "Play other masters".
  - **"Group":** a Grid with columns Name | OS | Role | RTT | Jitter | Latency (reported) | Extra latency
    [DragValue ms, −1000..2000, sends `SetExtraLatency`] | Sync (from that peer's rx stats).
  - **"Sync":**
    - Delay: Auto checkbox + DragValue ms.
    - A "Sync test" toggle.
    - A calibration section: mic node combo (peers + self), mic device text/combo when self, Start, Apply and
      Cancel, plus a results table.
  - **"Settings":** "Start with Windows/at login" checkbox (autostart), group/key/port/interface fields, Save &
    restart network (drop `Engine`, save config, `Engine::start`), and "Open config" and "Open logs" buttons.
  - Warnings show as a yellow banner list at the top.
  - A stream with a `layout_warning` gets a "Configure speakers…" button. On Windows it runs
    `control mmsys.cpl sounds`; on macOS, `open -a "Audio MIDI Setup"`.
  - Repaints every 250 ms while visible.
  - Device lists refresh when the window opens and every 5 s.
- **Tray** (created inside the `eframe::run_native` creator closure, on the main thread):
  - Items: a disabled status `MenuItem`, "Open SoundSync", a "Broadcast this PC" `CheckMenuItem` (Windows),
    "Mute" (Check), "Sync test" (Check), a separator, "Quit".
  - Tooltip: "SoundSync — <status line>".
  - Double-click (`TrayIconEvent::DoubleClick`) or "Open" → show.
  - Handlers use `TrayIconEvent::set_event_handler` and `MenuEvent::set_event_handler`, and forward into a
    crossbeam channel, then call `ctx.request_repaint()`.
  - Show runs from the handler thread. On Windows: `ShowWindow(hwnd, SW_SHOW)` + `SetForegroundWindow`, with the
    HWND stored from `cc` via `HasWindowHandle`. On macOS: `send_viewport_cmd(Visible(true))` +
    `Focus`.
  - The status item and checks refresh every 1 s.
  - `InstanceGuard.on_show` uses the same show path; `on_quit` → Quit.
- **Close:** `close_requested` → `CancelClose` + `Visible(false)`. Only Quit → engine shutdown, restores (Task 24),
  then `ViewportCommand::Close` and exit.
- **Start:** visible when `--show` was passed or the config was newly created, otherwise hidden.
- **build.rs (Windows):** writes `OUT_DIR/soundsync.ico` (16/32/48/256 from `icon_rgba`, shared via
  `include!("src/icon.rs")`) and embeds it with winresource, plus a manifest with per-monitor-v2 DPI awareness.
  The binary gets `#![windows_subsystem = "windows"]` in release builds, which allocates a console for
  subcommands via `AttachConsole(ATTACH_PARENT_PROCESS)`.

- [ ] **Step 1: Write the failing tests** for `format.rs`:
  - `fmt_ms_rounds_to_tenths`
  - `sync_label_states`: Waiting, in sync at 0.2 ms, Behind 45 ms.
  - `tray_status_line_variants`
  - `icon_rgba_has_expected_size`: 32 → 4096 bytes, with alpha 0 at the corners.
- [ ] **Step 2: Run** `cargo test -p soundsync format icon`; it should fail.
- [ ] **Step 3: Implement** `format.rs`, `icon.rs` and `build.rs`.
- [ ] **Step 4: Implement** `app.rs`, `panels.rs` and `tray.rs`.
- [ ] **Step 5: Manual verification on this PC.** Use release build, `--instance ui`, and keep the output on `null`
  so nothing is audible.
  - Launching with `--show` makes the window appear and the tray icon show up.
  - Closing the window hides it, and the process stays: `Get-Process soundsync` lists it with
    `MainWindowHandle` hidden.
  - Tray "Open" shows the window.
  - A second `soundsync.exe --instance ui` exits and the first window appears.
  - The autostart checkbox creates `Startup\SoundSync.lnk`; unchecking it removes the `.lnk`. Confirm the file is
    gone.
  - Quit leaves no process behind.
  - The log file exists in `%LOCALAPPDATA%\SoundSync-ui\logs\`.
- [ ] **Step 6: Checkpoint.**

### Task 24: VB-CABLE helper and default-device switching

**Files:**
- Create: `crates/audio/src/win/policy.rs`, `crates/engine/src/defaults.rs`, `crates/app/src/ui/vbcable.rs`
- Modify: `node.rs` (call the guard), `panels.rs`

**Interfaces:**
- Produces:
  ```rust
  // audio::win::policy
  #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] pub struct DefaultDevices { pub console: String, pub multimedia: String, pub communications: String }  // render endpoint ids
  pub fn read_defaults() -> Result<DefaultDevices, AudioError>;
  pub fn set_default_render(id: &str) -> Result<(), AudioError>;   // all three roles via IPolicyConfig::SetDefaultEndpoint
  // engine::defaults
  pub trait DefaultDeviceApi: Send + Sync { fn read(&self) -> Result<DefaultDevices, String>; fn set(&self, id: &str) -> Result<(), String>; fn exists(&self, id: &str) -> bool; }
  impl DefaultSwitchGuard { pub fn new(api: Arc<dyn DefaultDeviceApi>, state_path: PathBuf) -> Self;
      pub fn recover_on_start(&self) -> Option<String>;          // restore saved defaults left by a crash; returns warning text
      pub fn switch_to(&mut self, target_id: &str) -> Result<(), String>;  // save to state.toml [saved_defaults] first, then set
      pub fn restore(&mut self) -> Result<(), String>; }         // restore + clear state; skips ids that no longer exist (warning)
  // app::ui::vbcable
  pub fn detect_cable(devices: &[DeviceInfo]) -> Option<&DeviceInfo>;  // active render device whose name contains "CABLE Input"
  pub enum SetupState { Installed { channels: u16 }, NotInstalled, WaitingForInstall }
  ```
- **IPolicyConfig:** declared with `#[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]`, with 10
  placeholder methods before `SetDefaultEndpoint(PCWSTR, ERole)`. CLSID `870af99c-171d-4f9e-af0d-e63df40c2bc9`.
  `read_defaults` uses `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, role)`.
- **`WinHost::active_sessions`:** `IAudioSessionManager2::GetSessionEnumerator` on the endpoint counts sessions in
  `AudioSessionStateActive` whose process id ≠ our own.
- **Silence watchdog** (`engine::defaults::SilenceWatchdog { pub fn update(&mut self, now: Nanos, capture_silent:
  bool, active_sessions: usize) -> bool }`):
  - It returns true once capture has been silent for ≥ 5 s while `active_sessions > 0`.
  - The engine polls `active_sessions` every 1 s while broadcasting.
  - While true, it shows the warning "Apps are playing to <capture> but SoundSync hears silence — an app may be
    using exclusive mode or protected (DRM) audio".
- **Engine:**
  - On start, `recover_on_start()`.
  - On `SetBroadcast(true)` in Virtual mode with `switch_default`, `switch_to(cable id)`.
  - On broadcast off, mode change, `SetSwitchDefault(false)` or shutdown, `restore()`.
  - On non-Windows hosts the guard is absent.
- **UI helper:**
  - NotInstalled → a "Set up VB-CABLE…" button. It saves the defaults into state `[vbcable_setup]` through the same
    guard API, opens `https://vb-audio.com/Cable/` with the text "VB-CABLE is a free product by VB-Audio Software;
    install it, then return here", and enters WaitingForInstall.
  - It polls devices every 2 s. Once the cable appears, it restores the saved defaults and shows Installed.
  - Installed with `channels < monitor channels` → the banner "CABLE Input is <describe>, your monitor is
    <describe> — set CABLE Input to <describe> in Configure speakers" and a button running `control mmsys.cpl
    sounds`.

- [ ] **Step 1: Write the failing tests** (`defaults.rs`, using a `MockApi`):
  - `saves_then_switches_then_restores`
  - `crash_recovery_restores_on_start`: state written, a new guard calls `recover_on_start` → mock set to the
    saved ids, and the state is cleared.
  - `restore_skips_missing_device`
  - `no_state_means_no_action`
  - `detect_cable_matches_vb_names`: "CABLE Input (VB-Audio Virtual Cable)" → Some.
  - `silence_watchdog_trips_after_5s_with_sessions`: silent with 1 session → false at 4.9 s, true at 5 s. With
    0 sessions it never trips. Audio returning resets it.
- [ ] **Step 2: Run** `cargo test -p soundsync-engine defaults` and `cargo test -p soundsync vbcable`; they should
  fail.
- [ ] **Step 3: Implement** `policy.rs` (with a `#[ignore]` read-only test `read_defaults_returns_ids`; **no** test
  ever calls `set_default_render` on this PC), the guard, the engine wiring and the UI helper.
- [ ] **Step 4: Run** the tests, plus `cargo test -p soundsync-audio -- --ignored read_defaults`; they should pass.
- [ ] **Step 5: Checkpoint.**

---

## Phase 5 — macOS

### Task 25: CoreAudio backend

**Files:**
- Create: `crates/audio/src/mac/{mod.rs,enumerate.rs,output.rs,input.rs,clock.rs}`
- Modify: `crates/audio/Cargo.toml` (`[target.'cfg(target_os="macos")'.dependencies]` coreaudio-rs,
  objc2-core-audio, objc2-core-audio-types)

**Interfaces:**
- Produces: `pub struct MacHost` implementing `AudioHost`. `default_host()` returns it on macOS.
  - `open_loopback`, `probe_loopback_latency` and `endpoint_volume` → `Unsupported("receive-only on macOS")`.
- **Clock:** `now_ns` = `mach_absolute_time` × numer/denom (timebase cached). `host_ns(mHostTime)` uses the same
  conversion.
- **Output:**
  - An AUHAL unit bound to the device, with the client format set to the device's nominal rate, f32 interleaved,
    and the device's channel count.
  - `dac_time_ns = host_ns(ts.mHostTime) + (kAudioDevicePropertyLatency[output] + kAudioStreamPropertyLatency of
    the first output stream object) × 1e9/rate`. No safety offset and no buffer size.
  - `latency_ns()` returns that sum.
  - The channel mask comes from `kAudioDevicePropertyPreferredChannelLayout`, mapped through
    `mask_from_coreaudio_labels` (Task 4); no layout → `default_mask`.
- **Input:** AUHAL input on the mic device, with timestamps via `FrameClock(host_ns(mHostTime))`.
- **Watch:** `AudioObjectAddPropertyListener` on `kAudioHardwarePropertyDevices` and
  `kAudioHardwarePropertyDefaultOutputDevice`.

- [ ] **Step 1: Add an early compile check on this PC.** Run `rustup target add aarch64-apple-darwin x86_64-apple-darwin`.
  `cargo check -p soundsync-audio --target aarch64-apple-darwin` must succeed (objc2 bindings need no SDK to
  check). If a dependency needs the SDK, note it and rely on CI.
- [ ] **Step 2: Write tests** marked `#[ignore]` (hardware) and `#[cfg(target_os = "macos")]`:
  - `mac_lists_output_devices`
  - `mac_render_reports_monotonic_dac_times`: on the default output, at volume 0 by writing zeros.
- [ ] **Step 3: Implement** the mac backend.
- [ ] **Step 4: Verify.** `cargo check --target aarch64-apple-darwin --workspace` should be clean on this PC. CI's
  macos job runs `cargo test --workspace` green (ignored tests are skipped).
- [ ] **Step 5: Checkpoint.**

### Task 26: macOS app integration, bundle, CI artifacts and README

**Files:**
- Create: `crates/app/src/autostart/macos.rs`, `assets/Info.plist.in`, `scripts/bundle-macos.sh`, `README.md`
- Modify: `.github/workflows/build.yml`, `crates/app/src/ui/panels.rs` (receive-only on macOS)

**Interfaces:**
- Produces: `pub fn launch_agent_plist(app_bundle: &Path) -> String` and `pub fn info_plist(version: &str) -> String`
  (fills `Info.plist.in`).
- **Mac autostart:**
  - Enabling writes `~/Library/LaunchAgents/com.soundsync.app.plist` with Label `com.soundsync.app`,
    ProgramArguments `["/usr/bin/open", "-a", "<bundle path>", "--args", "--background"]` and RunAtLoad true.
  - Disabling runs `launchctl bootout gui/<uid>/com.soundsync.app` (ignoring errors) and deletes the file.
  - The bundle path is three parents up from `current_exe()`.
- **Info.plist:**

  | Key | Value |
  |---|---|
  | CFBundleIdentifier | `com.soundsync.app` |
  | CFBundleName | `SoundSync` |
  | CFBundleExecutable | `soundsync` |
  | CFBundleShortVersionString | `{version}` |
  | LSUIElement | true |
  | LSMinimumSystemVersion | 12.0 |
  | NSMicrophoneUsageDescription | "SoundSync uses the microphone only during speaker sync calibration." |
  | NSLocalNetworkUsageDescription | "SoundSync finds and streams audio to other SoundSync devices on your local network." |
  | NSBonjourServices | `[_soundsync._udp]` |
  | CFBundleIconFile | `SoundSync` |

- **Mac UI:**
  - The "This PC" broadcast section is replaced by "This Mac plays audio from Windows masters".
  - If there are no peers and no mDNS results for 10 s after start, show the banner "No devices found — allow
    SoundSync in System Settings ▸ Privacy & Security ▸ Local Network".
- **`bundle-macos.sh`:**
  - Builds both targets in release, then `lipo -create`.
  - Assembles `dist/macos/SoundSync.app/Contents/{MacOS/soundsync, Info.plist, Resources/SoundSync.icns}`. The icns
    is made with `soundsync gen-icon --png <tmp>/1024.png`, then `sips` sizes and `iconutil -c icns`.
  - Signs with `codesign --force --deep -s -` and zips with `ditto -c -k --keepParent` →
    `dist/SoundSync-macos-universal.zip`.
- **CI:** the windows job adds `pwsh scripts/build-windows.ps1` and uploads the zip artifact. The macos job adds
  both targets, runs `scripts/bundle-macos.sh`, and uploads the zip artifact.
- **README.md:**
  - What SoundSync does and the master modes.
  - Installing VB-CABLE (helper), the firewall step, per-PC config, and copying `group`/`key` (but not `node.id`)
    to other PCs.
  - Calibration how-to.
  - Mac first run: right-click → Open, or `xattr -dr com.apple.quarantine SoundSync.app`. Mention Local Network
    permission and that ad-hoc signing may reset it after updates.
  - Troubleshooting: "inbound blocked", "VB-CABLE not found", layout mismatch.

- [ ] **Step 1: Write the failing tests** (they run on every OS, since they're pure string builders):
  - `launch_agent_plist_uses_open_a`: contains `/usr/bin/open`, `-a`, the bundle path, `RunAtLoad`.
  - `info_plist_contains_required_keys`: every key above appears.
- [ ] **Step 2: Run** `cargo test -p soundsync plist`; it should fail.
- [ ] **Step 3: Implement** the plists, mac autostart, the UI changes, the script, the CI changes and the README.
- [ ] **Step 4: Verify.**
  - `cargo test -p soundsync plist` passes.
  - `cargo check --target aarch64-apple-darwin -p soundsync` is clean.
  - The workflow YAML validates (`gh workflow view` is not possible without a remote, so review it by eye).
  - The full run happens when the user pushes to GitHub. Report it as pending user action.
- [ ] **Step 5: Checkpoint.**

---

## Phase 6 — Hardening

### Task 27: Device hot-plug, sleep/resume and reopen

**Files:**
- Create: `crates/engine/src/{devices.rs,power.rs}`, `crates/engine/tests/hotplug.rs`
- Modify: `crates/audio/src/fake.rs`, `node.rs`, `streams.rs`

**Interfaces:**
- Produces (fake.rs): `FakeHost::remove_device(&self, name: &str)` (streams on it die: `is_alive() == false`),
  `FakeHost::readd_device(&self, name: &str)`, and `FakeHost::jump_clock(&self, ns: Nanos)` (a sudden host-clock
  jump that simulates sleep).
- Produces: `devices::Reopener { pub fn new() -> Self; pub fn due(&mut self, now: Nanos) -> bool; pub fn failed(&mut self, now: Nanos); pub fn succeeded(&mut self); }`
  with a backoff of 0.5, 1, 2, 5, 5… s.
  `power::GapDetector { pub fn tick(&mut self, now: Nanos) -> bool }`, which returns true when the gap since the
  last tick exceeds 2 s.
- **Behaviour:**
  - On a `DeviceEvent` for an open device, `DefaultChanged` while using `Default`, or `!is_alive()`: close the
    stream, warn `"<device> disconnected — retrying"`, and reopen with `Reopener`.
  - A configured device that is missing at reopen → use `Default` with the warning
    `"<name> not found — using default output"` and keep retrying the configured one every 5 s.
  - A gap → reset all `ClockEstimator`s, realign every slot (replace the slots), reopen every stream, and log
    `"resumed after <n> s"`.
  - When the output reopens on a **different** device than before, or re-attaches after a removal (for example a
    Bluetooth reconnect), add the hint "Output changed — run Sync ▸ Auto-calibrate again for best sync". Clear the
    hint when calibration is applied.

- [ ] **Step 1: Write the failing tests:**
  - `reopener_backoff_sequence`
  - `gap_detector_trips_over_2s`
  - `output_removed_then_readded_resumes_playback`: in the `virtual_mode` setup, remove R1's output at 4 s and
    re-add it at 6 s → R1 is back in sync (1 ms) by 9 s, the warning was shown, and the recalibration hint is
    present.
  - `capture_device_removed_stops_stream_and_recovers`
  - `tick_gap_resets_clocks`: R2 `jump_clock(30 s)` → R2's peers re-sync and R2's stream is back in sync within 4 s.
- [ ] **Step 2: Run** `cargo test -p soundsync-engine --test hotplug devices power`; it should fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the full suite: `cargo test --workspace`, plus `cargo clippy --workspace --all-targets -- -D warnings`.
  Everything should be green.
- [ ] **Step 5: Final real check on this PC.** Repeat the Task 20 three-terminal run with the GUI build
  (`soundsync --instance a --show` as master on Steam Streaming Speakers + null monitor, and `soundsync run
  --instance b --output null`). Then ask the user before any audible test, e.g. B on "Digital Output (Realtek
  USB2.0 Audio)".
- [ ] **Step 6: Checkpoint.** Report to the user what was verified on hardware and what still needs them:
  VB-CABLE end-to-end, Mac, and audible multi-room calibration.
