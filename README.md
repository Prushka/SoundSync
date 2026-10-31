# SoundSync

SoundSync plays the sound of one or more Windows PCs on every SoundSync device on your local network, with
all speakers in sync. Audio travels as-is: lossless PCM in the source's sample rate and channel layout,
including 5.1 and 7.1. Each device compensates for its own output delay (HDMI TVs, AV receivers, Bluetooth
speakers), so the slowest speaker no longer sets the pace for everyone else.

- **Windows**: master and/or receiver. Any Windows PC can broadcast; several masters can broadcast at once
  and every receiver plays the mix.
- **macOS**: receiver only (a menu bar app).

## How it works

A **master** captures audio and sends it unicast to every SoundSync device in its group. Every device,
including the master, schedules each sample to leave its speaker at the same moment:

    play time = capture time (mapped to this device's clock) + group delay − this device's extra latency

Clocks are synchronised continuously over the network, and each device adjusts its playback rate by tiny
amounts (≤ 0.03 %) to follow the master without audible artefacts.

### Master modes

| Mode | What you do | Sync | Latency |
|---|---|---|---|
| **VB-CABLE** (default) | Apps play to the virtual device *CABLE Input*. SoundSync captures it and plays it on this PC's real speakers too (*monitor output*). | Every speaker, including this PC's | Group delay (auto: just enough for the slowest device, typically 30–150 ms) |
| **Direct** | SoundSync captures a real output (e.g. your speakers) while Windows plays to it as usual. | Other devices follow this PC's speaker when they can; slower ones show "Behind N ms" | Lowest: this PC plays immediately |

In Direct mode this PC does not also play other masters on the captured output (that would be captured
again and echo).

## Install on Windows

1. Download `SoundSync-windows-x64.zip` (from the GitHub Actions *build* run, artifact
   *SoundSync-windows-x64*) and extract it.
2. In PowerShell, run:

   ```powershell
   powershell -ExecutionPolicy Bypass -File .\install-windows.ps1 -Start
   ```

   This copies `soundsync.exe` and `uninstall-windows.ps1` to `%LOCALAPPDATA%\Programs\SoundSync`, creates
   Start Menu and Startup shortcuts and asks once for administrator rights to allow SoundSync through
   Windows Firewall (inbound UDP 47810 and 5353, private and domain networks only). Nothing else needs
   administrator rights.

   | Flag | Effect |
   |---|---|
   | `-NoFirewall` | Skip the firewall rule (other devices then cannot reach this PC unless you allow it yourself) |
   | `-NoStartup` | Do not start with Windows (can be changed later in Settings) |
   | `-NoStartMenu` | No Start Menu shortcut |
   | `-Port N` | Firewall rule for a different `network.port` |
   | `-Start` | Open SoundSync when done |
   | `-Remove` | Same as `uninstall-windows.ps1` |

SoundSync lives in the tray. Closing the window hides it; use **Quit** in the tray menu to exit.
Starting SoundSync again brings up the running instance's window.

### What is installed where

| What | Where | Created by |
|---|---|---|
| Program | `%LOCALAPPDATA%\Programs\SoundSync\soundsync.exe` (+ `uninstall-windows.ps1`) | installer |
| Start Menu shortcut | `%APPDATA%\Microsoft\Windows\Start Menu\Programs\SoundSync.lnk` | installer |
| Start with Windows | `…\Start Menu\Programs\Startup\SoundSync.lnk` | installer, or **Settings ▸ Start with Windows** |
| Firewall rule *SoundSync (UDP in)* | Windows Firewall (for this exe's path) | installer (administrator) |
| Settings | `%APPDATA%\SoundSync\config.toml` | SoundSync, first start |
| Logs and state | `%LOCALAPPDATA%\SoundSync\` | SoundSync |

Neither the installer nor SoundSync writes to the registry, and there is no *Apps & features* entry. Two
things SoundSync changes only at your request and puts back: the Windows default output (only with
**Switch Windows default output to VB-CABLE while broadcasting**; restored when broadcasting stops, on exit,
when Windows shuts down and after a crash) and the Startup shortcut.

### Updating

Run the new release's `install-windows.ps1` (add `-NoFirewall` if the rule already exists, to skip the
administrator prompt). It quits the running SoundSync, replaces the program and starts it again; settings,
logs and the firewall rule (tied to the program's path) stay as they are.

Replacing the file by hand works too: **Quit** SoundSync from the tray (a running exe cannot be replaced),
copy the new `soundsync.exe` over `%LOCALAPPDATA%\Programs\SoundSync\soundsync.exe` and start it.

Update every PC in a group to the same release: versions with a different network protocol cannot talk to
each other.

### Uninstalling

```powershell
powershell -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\SoundSync\uninstall-windows.ps1"
```

(or the `uninstall-windows.ps1` in the release folder). It quits SoundSync, puts back the default output
devices if SoundSync left them switched, removes the shortcuts, the program and the firewall rule (asking for
administrator rights only if the rule exists). Settings and logs are kept unless you add `-Purge`;
`-NoFirewall` leaves the firewall rule. VB-CABLE is a separate program: uninstall it from *Apps & features*.

### VB-CABLE (for VB-CABLE mode)

VB-CABLE is a free virtual audio device by VB-Audio Software. In SoundSync, open **Overview ▸ This PC** and
click **Set up VB-CABLE…**: SoundSync saves your current default speakers, opens the VB-Audio download page,
waits for the cable to appear and then puts your default speakers back (the VB-CABLE installer may change
them).

For multichannel sources, set *CABLE Input* to the same layout as your speakers (Sound settings ▸ CABLE Input
▸ Configure speakers ▸ 5.1 or 7.1). SoundSync warns when the cable has fewer channels than the monitor output.

To get audio into the cable, either set *CABLE Input* as the output of the apps you want to share, or turn on
**Switch Windows default output to VB-CABLE while broadcasting**. SoundSync then restores your previous
default output when broadcasting stops, when it quits and, after a crash, at its next start.

## Install on macOS

1. Download `SoundSync-macos-universal.zip` (artifact *SoundSync-macos-universal*), unzip it and move
   `SoundSync.app` to `/Applications`.
2. The app is ad-hoc signed, not notarised. On first launch, right-click it and choose **Open**, or run:

   ```sh
   xattr -dr com.apple.quarantine /Applications/SoundSync.app
   ```

3. Allow **Local Network** access when macOS asks. Without it SoundSync cannot find other devices and
   shows "No devices found — allow SoundSync in System Settings ▸ Privacy & Security ▸ Local Network".
   Because the app is ad-hoc signed, macOS may forget this permission after you install an update; allow it
   again in the same place.

SoundSync appears in the menu bar. **Settings ▸ Start at login** adds a login item that starts it hidden.

## Configuration

Each device has its own config file, created on first start:

| | Config | Logs |
|---|---|---|
| Windows | `%APPDATA%\SoundSync\config.toml` | `%LOCALAPPDATA%\SoundSync\logs` |
| macOS | `~/Library/Application Support/SoundSync/config.toml` | `~/Library/Logs/SoundSync` |

Most settings are in the window. The file looks like this:

```toml
[node]
id = 4811...       # random, unique per device: never copy it to another device
name = ""          # empty = computer name

[network]
group = "default"  # devices only talk to devices with the same group and key
key = ""           # optional shared secret
port = 47810
interface = "auto" # or an interface name / IPv4 address
static_peers = []  # ["192.168.1.20:47810"] if your network blocks mDNS

[master]           # Windows only
enabled = false
mode = "virtual"   # or "direct"
capture_device = ""   # empty: CABLE Input (virtual) / default output (direct)
monitor_device = ""   # virtual mode: where this PC plays its own stream; empty = default output
switch_default = false

[playback]
output_device = ""    # empty = default output
volume = 1.0
follow_master_volume = true
play_remote = true
extra_latency_ms = 0.0

[sync]
delay_ms = "auto"     # or a number of milliseconds
```

To put several devices in one group, copy `group` and `key` (not `node.id`) to each of them. (If you copy
the whole file by mistake, the two devices notice the shared id and one of them picks a new one.)
Device names that are not found fall back to the default output, with a warning in the window.

You can edit `config.toml` while SoundSync runs: about two seconds after you save, SoundSync applies it
(restarting its network and audio). A file with a syntax error is reported in the window and not applied;
SoundSync keeps the settings it has until you fix it.

Discovery uses mDNS on your real LAN interfaces only (virtual adapters such as Hyper-V, WSL, Docker, VPNs and
Tailscale are ignored). Only devices on your local network, or listed in `static_peers`, can join, and only
after proving they belong to your group; static peers on another subnet (for example over a VPN) must list
each other. Audio is sent unicast to the address each device joined from, so other devices on your network
never receive it.

## Calibration

Some delay happens after the point the operating system can see: TV picture processing, AV receivers,
Bluetooth. Each device has an **extra latency** (ms) to cover it, editable for any device from any device in
the **Overview** group table.

- **Sync test** (Sync tab): every speaker clicks once a second at the same moment, each at its own pitch.
  Adjust the extra latencies until the clicks sound like one. A speaker that clicks late needs a larger value.
- **Auto-calibrate with a microphone** (Sync tab): choose a device with a microphone and place it where you
  listen (speakers are aligned for that spot; sound travels about 34 cm per millisecond). **Start**: the
  speakers take turns playing a short sweep, three times each. SoundSync finds when each sweep arrived (the
  earliest strong arrival, so a louder reflection off a wall does not count), takes each speaker's median and
  shows the corrections with each speaker's signal strength. A speaker is flagged when its sweeps disagree by
  more than 1 ms, when one was not heard, or when the signal is weak (below 30 dB). **Apply** adds the
  corrections to the extra latencies; **Check** then plays one sweep per speaker and shows what is left
  (within 1 ms is in sync). The microphone's own delay cancels out, since every speaker is measured through it.
  Use a built-in or USB microphone: Bluetooth headset microphones change the headset's own latency while they
  are in use.

Bluetooth delays drift a little between connections. Rerun the calibration if a Bluetooth speaker sounds out
of sync after reconnecting.

## Troubleshooting

| Message | What to do |
|---|---|
| No replies from other SoundSync devices — Windows Firewall may be blocking SoundSync | Run `install-windows.ps1` again without `-NoFirewall`, or allow `soundsync.exe` for inbound UDP on private networks. Make sure the network is set to *Private* in Windows. |
| No devices found / No replies … Local Network (macOS) | System Settings ▸ Privacy & Security ▸ Local Network ▸ enable SoundSync. |
| VB-CABLE (CABLE Input) not found — install it or switch to Direct mode | Use **Set up VB-CABLE…**, or switch the master to Direct mode. |
| *device*: 7.1 → 5.1 (channels remapped; match the speaker layouts in Sound settings) | The source and this output have different speaker layouts. SoundSync up- or downmixes, but for exact playback set both to the same layout (**Configure speakers…**). |
| Apps are playing to … but SoundSync hears silence | An app is using exclusive mode or protected (DRM) audio, which Windows does not let anyone capture. Turn off exclusive mode for that app or device. |
| Behind N ms (Direct mode) | That device cannot play as early as the master's speaker. Switch the master to VB-CABLE mode for full sync. |
| *name* not found — using default output | The configured device is missing; pick another one in the window. |

Logs are in the folders listed under [Configuration](#configuration); a session that crashed is kept with a
`-recovered` suffix.

## Building

Rust (stable, MSVC on Windows) is all that is needed.

```sh
cargo test --workspace
pwsh scripts/build-windows.ps1     # → dist/SoundSync-windows-x64.zip
bash scripts/bundle-macos.sh       # on macOS → dist/SoundSync-macos-universal.zip
```

For local testing, `--config <file>` (or `--instance <name>`) runs a second instance with its own settings;
`soundsync devices` lists audio devices with their formats and layouts, and `soundsync run` runs a node
without a window.
