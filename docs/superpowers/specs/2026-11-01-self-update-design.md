# Self-update from GitHub releases — design

Date: 2026-11-01. Owner's choices: Windows **and** macOS; check automatically, **install on click**.

## Goal

SoundSync finds out by itself when a newer build is published on
[GitHub releases](https://github.com/Prushka/SoundSync/releases). It replaces the copy the user is running,
wherever it is, and starts the new one, in one click. Every device in a group should run the same release,
so the prompt also says to update the others.

**Not in scope:**

- Updating other devices remotely.
- An opt-out setting.
- Downgrades.
- Pre-releases.
- Linux.

## What is known

- **Releases:** CI publishes every passing push to `main` as `v<version>-build.<n>` (marked Latest), with
  `SoundSync-windows-x64.zip` (`soundsync.exe`, scripts, README) and `SoundSync-macos-universal.zip`
  (`SoundSync.app`, made with `ditto`).
- **Metadata:** `GET https://api.github.com/repos/Prushka/SoundSync/releases/latest` works without
  authentication (60 requests per hour per IP). Each asset carries `digest: "sha256:<hex>"`, `size` and
  `browser_download_url`, which redirects to a CDN.
- **Build numbers:** a binary knows its own build number (`version::build_number()`, from `SOUNDSYNC_BUILD`).
  Local builds have none and say "(dev)".
- **Probe** (2026-11-01, throwaway):
  - `ureq` 3 with `native-tls-no-default` (SChannel on Windows, Security.framework on macOS), `zip` 9 with
    `deflate-flate2-zlib-rs` and `sha2` fetched build 8.
  - It verified the digest and extracted `soundsync.exe`.
  - It still passes `cargo check --target aarch64-apple-darwin` from Windows. rustls would need ring or
    aws-lc, whose C build breaks that cross-check.
- **Windows file paths:** the firewall rule and both shortcuts point at the exe's path, so replacing the
  file in place keeps them. A running exe can't be overwritten, but it can be renamed within its volume.
- **Downloads aren't marked:** files our own process writes get no Mark of the Web (Windows) and no
  quarantine attribute (macOS), so there is no SmartScreen or Gatekeeper prompt. On macOS the ad-hoc
  signature changes, so microphone and Local Network permissions may be asked again (the README already
  says so).

## Design

### Units (all in `crates/app/src/update/`)

| File | Purpose | Depends on |
|---|---|---|
| `release.rs` | Pure logic:<br>• parse the release JSON into `Release { tag, title, build, page_url, asset }`, where the asset holds name, url, size and sha256<br>• parse a tag into a build number<br>• `newer_than(current)`<br>• the asset name per OS<br>• relaunch arguments | serde, serde_json |
| `fetch.rs` | One `ureq::Agent` (native TLS, platform roots, `User-Agent: SoundSync/<version>`, timeouts):<br>• `latest(api_url)` returns a `Release`<br>• `download(asset, progress)` returns bytes after checking the size and sha256 | ureq, sha2 |
| `install.rs` | The file system:<br>• where this copy lives (`Target::Exe(path)` / `Target::Bundle(path)`, or why it can't update)<br>• staging, verifying the staged binary, swapping with rollback, relaunching, cleaning up leftovers | zip, std, platform tools |
| `mod.rs` | `Updater`: the background thread with periodic checks and install requests, and the shared `UpdateState` the UI reads | the three above |

### Where this copy lives (`install::target()`)

- **Dev builds** (no build number): *"This is a development build; it doesn't update itself."* No checks.
- **Windows:** `Target::Exe(current_exe)`.
- **macOS:** `Target::Bundle(bundle_of(current_exe))`, unless:
  - the exe isn't inside a `.app` (*"Only SoundSync.app updates itself."*);
  - the path contains `/AppTranslocation/`: *"Move SoundSync.app to Applications and open it from there to
    update it."*

### States (`UpdateState`, shown in the UI)

`Unavailable(why)` · `Idle` · `Checking` · `UpToDate` · `Available(Release)` ·
`Downloading { release, received, total }` · `Ready { release, staged }` · `Restarting(Release)` (the UI
took `Ready` and is swapping and quitting) · `Failed { error, release: Option<Release> }`

### Checking

- **When:** the updater thread checks 10 s after start and then every 6 h, plus on **Check for updates**.
- **What:**
  - Uses `/releases/latest` (no drafts, no pre-releases).
  - A release is newer when its tag's build number is greater than ours.
  - A release without our OS's asset, or without a sha256 digest, is skipped. The installer fails closed.
- **Errors:** errors from automatic checks are only logged, and the state goes back to what it was. Errors
  from a manual check are shown.

### Installing (click **Update and restart**)

1. **Download** (updater thread): download the asset, showing progress. Check the size and SHA-256 against
   the release metadata.
2. **Stage** in `<dir>/.soundsync-update/new/`, where `<dir>` holds the exe (Windows) or the bundle (macOS),
   so renames stay on one volume:
   - **Windows:** extract `soundsync.exe` from the zip.
   - **macOS:** `ditto -x -k <zip> new/`, then `codesign --verify --deep --strict new/SoundSync.app`.
3. **Verify** that the staged binary runs. `<staged exe> --version` must print `(build <n>` within 30 s
   (Defender may scan it first).

   If any of steps 1–3 fails, the state becomes `Failed` (keeping the release, so the user can retry or open
   the release page) and the staging folder is removed. If the folder can't be written: *"SoundSync can't
   write to <dir> (<os error>). Download build <n> from the release page instead."*
4. **Swap** (UI thread, renames only, on `Ready`):
   1. Move the running exe or bundle to `.soundsync-update/old/`.
   2. Move the staged one into its place.
   3. If step 2 fails, move the old one back and show the error. Nothing has changed, and the app keeps
      running.
5. **Restart** (`on_exit`, once the window closes):
   1. Stop the engine. This restores switched default devices, as on any quit.
   2. Drop the single-instance guard (mutex or lock file), so the new process becomes the primary instance.
   3. Start the new copy with the same arguments, minus `--show`/`--background`. Add `--show` if the window
      was open. On macOS, add `--background` if it was hidden.
      - **Windows:** `Command::new(exe)` in its own folder.
      - **macOS:** `/usr/bin/open -n <bundle> --args …`, through LaunchServices, as the login item does.

   This needs no new flag, so any target build starts this way. If the new copy can't be started, swap
   back and start the old one.
6. **After restart:** at start, a copy whose `.soundsync-update/old/` exists shows the notice *"Updated to
   SoundSync <version>."* A background thread then deletes `.soundsync-update` in a retry loop: 500 ms for
   up to 20 s, while the old process finishes exiting and its exe is locked. Leftovers from a failed or
   abandoned run are cleaned the same way.

### UI

- **Header (every tab, like the notice line, blue):**
  > SoundSync build 9 is available. Update every device in the group: different versions may not be able
  > to talk. [Update and restart] [What's new]
  - While downloading, the line shows a progress bar and percentage instead.
  - If the update fails, the line shows the error with Retry and Release page buttons.
- **Settings ▸ Updates:**
  - The current version.
  - The state ("Up to date (checked 14:02)", "Checking…", the reason for `Unavailable`, …).
  - **Check for updates** and, when available, **Update and restart**.
- **Tray:** while a build is available, an item *"Update to build 9"* (above Quit) installs it. SoundSync
  usually lives in the tray, so without it the header line would rarely be seen.

### Uninstall

`uninstall-windows.ps1` also removes `<install dir>\.soundsync-update`, so the empty-folder removal still
works after an interrupted update.

## Testing

- **Unit (pure, `release.rs`):**
  - tag parsing (`v0.1.0-build.8` → 8; junk → none);
  - newer/older/same;
  - parsing a fixture of the real build 8 JSON (asset per OS, digest; a missing digest or asset means no
    release);
  - relaunch arguments (strip, add, per OS and visibility).
- **Unit (`fetch.rs`) against a local one-shot HTTP server on 127.0.0.1:**
  - latest → `Release`;
  - download → bytes with progress;
  - a wrong sha256 or size → error.
- **Unit (`install.rs`), in temp dirs, with dummy files and folders:**
  - extracting `soundsync.exe` from an in-memory zip;
  - the swap moves the old copy aside and the new one in;
  - a failed second rename rolls back;
  - cleanup removes `.soundsync-update` and reports "was updated";
  - `target()` reasons (dev build, not a bundle, translocated) via a pure helper.
- **End to end on this PC (Windows), against the real release:**
  1. Build the exe with `SOUNDSYNC_BUILD=7` into a temp folder.
  2. Run it as `--instance u --bind 127.0.0.1 --no-mdns --show`, with output on the silent Steam Streaming
     Speakers and port 47812.
  3. Click **Update and restart** through UIA.
  4. Expect: the exe on disk reports build 8, a process from that path runs with the window shown, and
     `.soundsync-update` is gone.
  5. Afterwards, quit it and delete the temp folder and both `SoundSync-u` folders.

  Build 8 has no updater, but it starts fine because no new flag is used.
- **macOS:** compile-checked only (clippy `--target aarch64-apple-darwin`). The owner verifies it on the
  MacBook with the first release that has the updater.

## Remote updates from Overview (2026-11-02)

The group table exposes each device's build, its update status, an individual **Update to build …**
button, **Check devices for updates**, and **Update all outdated devices**. The owner chose the latest
published release. Bulk installation targets other online devices advertising an available update;
the controller's own **Update and restart** stays separate. Clicking an update authorizes download and
restart on the selected devices, with no second confirmation on them. The UI explains that audio pauses.

Management uses authenticated `SSUP` v1 datagrams on the existing UDP socket. This schema is independent
of the audio protocol and remains stable when the latter changes. Update peers therefore remain visible
and actionable even when their audio versions cannot communicate. Existing releases without this
management receiver require one local update; the UI says so instead of offering a nonworking button.

The receiver only accepts requests from an authenticated, admitted update peer at its advertised
address. Requests target a particular running session and carry increasing request numbers; retries
reuse the number and cannot enqueue another restart. The receiver remembers handled requests for its
own session, even if the requesting peer expires. Acknowledgements and status carry errors back to the
controller. The app handles requests even while its window is hidden.

Each receiver rechecks the official latest-release metadata and uses the existing OS-specific updater,
including size, checksum, unpacking, version verification, rollback and relaunch. No request supplies a
download URL, executable, shell command or installation path. If the latest release changed since the
button was shown, the receiver reports that and requires another click for the newly available build.

The controller retries unacknowledged requests for 8 seconds and retains accepted operations for up to
6 minutes. A restart is reported successful only after the device returns with the requested or newer
build. Read-only folders, development builds and unavailable Mac bundles remain visible as errors or
unavailable statuses. Tests cover authentication, incompatible protocols, replay/session checks, lost
acknowledgements, restart confirmation and reuse of the existing verified download path.
