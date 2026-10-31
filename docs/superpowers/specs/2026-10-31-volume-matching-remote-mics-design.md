# Volume matching, remote volume and microphones, device refresh — design

Approved in conversation on 2026-10-31 (the user waived the written-spec review).

## Goals (from the request)

1. Microphone calibration also matches speaker volumes: louder speakers are turned down to the
   quietest one. Every node's volume is adjustable from every node, like extra latency.
2. Any node can calibrate with any node's microphone, chosen from a list (today: own list only).
3. Device changes (microphones, outputs) show up without restarting the app.
4. "Apps are playing to CABLE Input but SoundSync hears silence" is information, not a warning.

## Findings that shaped the design (real hardware, OsmoPocket3 mic, LG TV soundbar, MacBook)

- Timing calibration repeats to ±0.01 ms run to run; sweeps agree within 0.2 ms.
- The TV mutes the start of sound after a few minutes of digital silence: the first sweep of a
  run was lost after 4.5 min of silence, never after 42 s. (Likely also the cause of an earlier
  "mic only worked after restarting the app" incident.)
- The camera microphone has a limiter: loud sweeps all peak at −3.3 dBFS, and a 10.5 dB
  volume step measured as 7.4 dB.

## Design

### Volume is the device volume

The "volume" of a speaker is its Volume slider: the output device's own volume (Windows endpoint
volume, macOS device volume), or SoundSync's software level for outputs without one. Calibration
and remote edits change exactly that.

### Level measurement

- Per sweep: energy of the recording from the detected arrival to the end of the sweep (+20 ms),
  minus the noise power measured before the arrival in the same window. A log sweep has equal
  energy per octave, so this is a pink-weighted level over 200 Hz–8 kHz (close to how music is
  heard). A speaker's level is the median of its sweeps.
- `CalibResult` gains `level_db` (this speaker's level relative to the quietest; ≥ 0) and
  `peak_dbfs` (loudest sample of its sweeps). A peak above −6 dBFS flags "microphone overloaded"
  (an overloaded microphone understates differences, so a correction never overshoots; Check
  measures what is left).
- Apply (and "Correct these" after Check) turns each speaker down by its `level_db` when it is at
  least 0.5 dB and the "Also match volumes" box (default on) is ticked.
- Turning down is relative ("by X dB"), applied by the speaker itself against its current volume:
  Windows `SetMasterVolumeLevel`, macOS `VolumeDecibels`, software level × 10^(−X/20).

### TV wake-up

Every speaker plays a soft noise burst before the first sweep, so sinks that mute after silence are
awake when the sweeps start. The lead time is set from a measurement on the TV.

### Protocol (version 2)

- `NodeState` gains `volume: f32` (slider 0..=1), `muted: bool`, `device_volume: bool`.
- `Control::SetVolume { target, change }` with `VolumeChange::{Set(f32), Muted(bool), TurnDown { db }}`.
- `Control::ListInputs` → `Control::Inputs { default: Option<String>, devices: Vec<String> }`
  (connected microphones only; trimmed to fit one datagram).
- `PROTOCOL_VERSION` = 2. A datagram with our magic and another version raises a warning naming
  the sender's address ("runs a different version of SoundSync — update every device").

### UI

- Group table: a Volume column (slider in % plus mute) for every node, including this PC.
- Calibration results: a Level column; overload and other problems flagged as today.
- Remote microphone node: a dropdown of that node's microphones (requested when chosen and every
  5 s while shown), instead of a text box.
- Device pickers get a ⟳ refresh button; lists also refresh immediately when the engine reports a
  device event (`Status.device_events` counter).
- The silence condition is a grey ℹ note under This PC (`Status.notes`), not a warning.

### macOS

The HAL's notification run loop is set to NULL at host start, so CoreAudio tracks device changes
on its own thread instead of depending on the main run loop (compile-checked only here).

## Implementation order

1. Core: level measurement in `calib.rs` (`level_db`, `peak_dbfs`, problems), protocol v2 types and
   validation, version-mismatch decode error.
2. Audio: `set_endpoint_volume_db` on the trait (Windows, macOS, fake), macOS run-loop fix.
3. Engine: volume in `NodeState`; `SetVolume` handling (local + remote); calibration level
   matching and wake-up; `ListInputs`/`Inputs`; device-event counter; notes; version warning.
4. App: group-table volume column, results Level column and checkbox, remote mic dropdown,
   refresh button and event-driven refresh, notes line.
5. Real-hardware check: TV wake-up lead time, a calibration with level matching on the TV and Mac.
6. README, full test suite (normal and `SS_FAKE_MIN_SLEEP_MS=20`), clippy for Windows and macOS,
   commit.

## Testing

- Unit: level measurement (known gains, noise subtraction), overload flag, protocol round trips
  and validation, version mismatch.
- Engine (fake host): remote volume/mute set and turn-down, calibration matching ends with equal
  levels, remote microphone list, device-event counter, version warning.
- Real hardware as in step 5.
