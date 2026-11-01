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

1. Download `SoundSync-windows-x64.zip` from the newest release on the repository's GitHub *Releases*
   page and extract it.
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

Neither the installer nor SoundSync writes to the registry, and there is no *Apps & features* entry. What
SoundSync changes, only at your request: the Windows default output (only with **Switch Windows default output
to VB-CABLE while broadcasting**; restored when broadcasting stops, on exit, when Windows shuts down and after
a crash), the Startup shortcut, and the output's own volume and mute when you (or another device in the
group, or volume matching) move SoundSync's **Volume** slider or tick **Mute**.

### Volume

SoundSync's **Volume** slider and **Mute** are the output device's own volume, the same slider as in Windows'
sound settings (or macOS's): moving either moves both. With **Follow master volume** (on by default), streams
also get quieter when the volume is turned down on the PC that broadcasts them, by the same amount in decibels
as on that PC (in VB-CABLE mode that is CABLE Input's volume, which the volume keys change while CABLE Input
is the default output).

Every device's Volume and mute can also be changed from any other device, in the **Volume** column of the
**Overview** group table (like extra latency). Microphone calibration can match the speakers' volumes for you
(see [Calibration](#calibration)).

### Updating

SoundSync checks for a newer release by itself, 10 seconds after it starts and then every 6 hours. It asks
GitHub (`api.github.com`) for the latest release and sends nothing else. When there is one, the window shows
*SoundSync build 9 is available* and the tray menu gets **Update to build 9**. Click **Update and restart**:
SoundSync downloads the release, checks its SHA-256 digest, puts the new `soundsync.exe` in place of the one
that's running (wherever it is) and starts it again. Settings, logs, shortcuts and the firewall rule (tied to
the program's path) stay as they are. **Settings ▸ Updates** shows the version and has **Check for
updates**.

If SoundSync can't write to its own folder (for example a copy in `Program Files`), it says so. Update by hand
instead: run the new release's `install-windows.ps1` (add `-NoFirewall` if the rule already exists, to skip
the administrator prompt). It quits the running SoundSync, replaces the program and starts it again.
Replacing the file works too: **Quit** SoundSync from the tray (a running exe cannot be replaced), copy the
new `soundsync.exe` over `%LOCALAPPDATA%\Programs\SoundSync\soundsync.exe` and start it.

Update every PC in a group to the same release: versions with a different network protocol cannot talk to
each other. Each release is one build, e.g. *SoundSync 0.1.0 (build 42)*. **Settings ▸ Updates** shows the
build a device runs, and hovering over a device's name in the group table shows that device's build.

### Uninstalling

```powershell
powershell -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\SoundSync\uninstall-windows.ps1"
```

(or the `uninstall-windows.ps1` in the release folder). It quits SoundSync, puts back the default output
devices if SoundSync left them switched, removes the shortcuts, the program (and what an interrupted update
left next to it) and the firewall rule (asking for administrator rights only if the rule exists). Settings and logs are kept unless you add `-Purge`;
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

1. Download `SoundSync-macos-universal.zip` from the newest release on the *Releases* page, unzip it and
   move `SoundSync.app` to `/Applications`.
2. The app is ad-hoc signed, not notarised. On first launch, right-click it and choose **Open**, or run:

   ```sh
   xattr -dr com.apple.quarantine /Applications/SoundSync.app
   ```

3. Allow **Local Network** access when macOS asks. Without it SoundSync cannot find other devices and
   shows "No devices found — allow SoundSync in System Settings ▸ Privacy & Security ▸ Local Network".
   Because the app is ad-hoc signed, macOS may forget this permission after you install an update; allow it
   again in the same place.

SoundSync appears in the menu bar. **Settings ▸ Start at login** adds a login item that starts it hidden.

SoundSync.app updates itself as on Windows (**Update and restart**), as long as it runs from Applications: a
copy opened straight from Downloads runs from a read-only place, and SoundSync asks you to move it first.
After an update macOS may ask again for microphone and Local Network access.

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
volume = 1.0          # only for outputs without their own volume control ("null")
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
- **Auto-calibrate with a microphone** (Sync tab): choose any device in the group, then one of its
  microphones, and place it where you listen (speakers are aligned for that spot; sound travels about 34 cm
  per millisecond). **Start**: every speaker plays a soft "shh" first (TVs and soundbars mute after a while of
  silence and would swallow the first sweep), then the speakers take turns playing a short sweep, three times
  each. SoundSync finds when each sweep arrived (the earliest strong arrival, so a louder reflection off a wall
  does not count) and how loud it was, takes each speaker's median, and shows the corrections, each speaker's
  **Level** (how much louder than the quietest speaker it arrived) and its signal strength.
  **Apply** adds the corrections to the extra latencies and, with **Also match volumes** (on by default),
  turns every louder speaker down by its Level using that speaker's own volume, so all of them are as loud as
  the quietest one. **Check** then plays one sweep per speaker and shows what is left (within 1 ms and 1 dB is
  in sync); **Correct these** applies that too.

  A speaker is flagged when its sweeps disagree by more than 1 ms, when one was not heard, when the signal is
  weak (below 30 dB), when the microphone was overloaded (peaks above -6 dBFS), or when a speaker's level
  varied by more than 2 dB between sweeps. The last one is typical of webcams and cameras: their microphones
  adjust their own gain, which squeezes level differences (one measured a 10.5 dB step as 7 dB). Matching then
  turns speakers down too little, never too much: run **Check** and **Correct these** again until the levels
  are within 1 dB, or use a microphone without automatic gain. The microphone's own delay and level cancel out,
  since every speaker is measured through it. Bluetooth headset microphones change the headset's own latency
  while they are in use; use another one.

Bluetooth delays drift a little between connections. Rerun the calibration if a Bluetooth speaker sounds out
of sync after reconnecting.

## Troubleshooting

| Message | What to do |
|---|---|
| No replies from other SoundSync devices — Windows Firewall may be blocking SoundSync | Run `install-windows.ps1` again without `-NoFirewall`, or allow `soundsync.exe` for inbound UDP on private networks. Make sure the network is set to *Private* in Windows. |
| No devices found / No replies … Local Network (macOS) | System Settings ▸ Privacy & Security ▸ Local Network ▸ enable SoundSync. |
| VB-CABLE (CABLE Input) not found — install it or switch to Direct mode | Use **Set up VB-CABLE…**, or switch the master to Direct mode. |
| *device*: 7.1 → 5.1 (channels remapped; match the speaker layouts in Sound settings) | The source and this output have different speaker layouts. SoundSync up- or downmixes, but for exact playback set both to the same layout (**Configure speakers…**). |
| ℹ CABLE Input is silent: apps have it open but nothing is playing … | Usually nothing is playing (a paused video keeps its app's audio open). If something *is* playing, that app uses exclusive mode or protected (DRM) audio, which Windows does not let anyone capture: turn off exclusive mode for that app or device. |
| The SoundSync at *address* runs a different version — install the same version on every device | Devices only talk to the same SoundSync version. Update the older one (both the Windows and the Mac builds come from the same release). |
| Behind N ms (Direct mode) | That device cannot play as early as the master's speaker. Switch the master to VB-CABLE mode for full sync. |
| *name* not found — using default output | The configured device is missing; pick another one in the window. |
| A device you just plugged in is not in a list | Lists refresh by themselves when Windows or macOS reports a change; the 🔄 button next to each list refreshes them by hand. |

Logs are in the folders listed under [Configuration](#configuration); a session that crashed is kept with a
`-recovered` suffix.

## Building

Rust (stable, MSVC on Windows) is all that is needed.

```sh
cargo test --workspace
pwsh scripts/build-windows.ps1     # → dist/SoundSync-windows-x64.zip
bash scripts/bundle-macos.sh       # on macOS → dist/SoundSync-macos-universal.zip
```

Every push to `main` that builds and passes its tests on both Windows and macOS is published as a GitHub
release: tagged `v<version>-build.<n>`, with both zips attached and notes listing the commits since the
previous release (`.github/workflows/build.yml`, `scripts/publish-release.sh`). CI builds show their build
number and commit as their version; local builds show `(dev)`.

For local testing, `--config <file>` (or `--instance <name>`) runs a second instance with its own settings;
`soundsync devices` lists audio devices with their formats and layouts, and `soundsync run` runs a node
without a window.
