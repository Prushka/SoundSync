# Self-update Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** SoundSync checks GitHub releases, and on one click replaces the running exe (Windows) or app
bundle (macOS) with the newer build and restarts it.

**Architecture:** A new `crates/app/src/update/` module:

- pure release logic (`release.rs`);
- an HTTP client (`fetch.rs`);
- file-system staging, swap and relaunch (`install.rs`);
- a background `Updater` (`mod.rs`) whose state the egui UI reads.

The swap happens on the UI thread. The relaunch happens in `on_exit`, after the engine and the
single-instance guard are dropped.

**Tech Stack:** Rust 2024, `ureq` 3 (`native-tls-no-default`), `zip` 9 (`deflate-flate2-zlib-rs`),
`serde_json`, `sha2`, eframe 0.37, tray-icon 0.26.

**Spec:** `docs/superpowers/specs/2026-11-01-self-update-design.md`

## Global Constraints

- **Code style:**
  - No rustfmt; lines up to about 160 characters.
  - Every new module starts with a `//!` doc.
  - Comments say why; files use LF line endings.
- **Tests:** names are sentences. For a test that passes at once, do a mutation check (remove the feature,
  watch the test fail).
- **Must stay green:**
  - `cargo test --workspace`, also with `SS_FAKE_MIN_SLEEP_MS=20`;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - the same with `--target aarch64-apple-darwin`.
- **No C-compiled dependencies:** the macOS cross-check from Windows must keep working. Use
  `ureq = { version = "3.5", default-features = false, features = ["native-tls-no-default"] }` and
  `zip = { version = "9.2", default-features = false, features = ["deflate-flate2-zlib-rs"] }`.
- **Exact values from the spec:**
  - API URL `https://api.github.com/repos/Prushka/SoundSync/releases/latest`;
  - assets `SoundSync-windows-x64.zip` / `SoundSync-macos-universal.zip`;
  - staging folder `.soundsync-update` (with `new/`, `old/`);
  - first check after 10 s, then every 6 h;
  - `--version` timeout 30 s;
  - cleanup retries every 500 ms for up to 20 s.
- **Exact text from the spec:**
  - "This is a development build; it doesn't update itself."
  - "Only SoundSync.app updates itself."
  - "Move SoundSync.app to Applications and open it from there to update it."
  - "SoundSync build {n} is available. Update every device in the group: different versions may not be able
    to talk."
  - "Updated to SoundSync {version}."
  - "SoundSync can't write to {dir} ({error}). Download build {n} from the release page instead."
- **Machine rules:** commit to `main` after each task, never push. The guide and README change in the
  same commit as the work (Task 6).

## Review Focus

1. **Offline or rate-limited (403) at an automatic check:** nothing is shown and the next check retries.
   A manual check shows the error. (Task 4 test: `failed_automatic_check_stays_quiet`.)
2. **The exe's folder can't be written** (for example a copy in Program Files): the spec's "can't write"
   text, and the app keeps running. (Task 3 test: `staging_where_nothing_can_be_written_says_so`.)
3. **A `.soundsync-update` left by an interrupted update** (only `new/`): it is cleaned at start, with no
   "Updated" notice. (Task 3 test: `leftovers_without_an_old_copy_are_cleaned_quietly`.)
4. **An update started from the tray while the window is hidden:** the new copy starts hidden too.
   (Task 3 test: `relaunch_keeps_the_window_hidden_when_it_was`.)
5. **Clicking Update twice, or while nothing is available:** only one download.
   (Task 4 test: `install_without_an_available_release_does_nothing`.)

---

### Task 1: Release metadata (`release.rs`) and dependencies

**Files:**
- Modify: `Cargo.toml` (workspace deps: `ureq`, `zip`, `serde_json`, as in Global Constraints)
- Modify: `crates/app/Cargo.toml` (add `serde`, `serde_json`, `sha2`, `ureq`, `zip`)
- Create: `crates/app/src/update/mod.rs` (`pub mod release;` for now), `crates/app/src/update/release.rs`
- Create: `crates/app/src/update/testdata/latest-build-8.json` (the real API reply for
  `v0.1.0-build.8`, trimmed to the fields used)
- Modify: `crates/app/src/main.rs` (`mod update;`)

**Interfaces:**
- Produces:
  - `pub struct Asset { pub name: String, pub url: String, pub size: u64, pub sha256: [u8; 32] }`
  - `pub struct Release { pub tag: String, pub title: String, pub build: u32, pub page_url: String, pub asset: Asset }`
  - `pub fn asset_name(os: Os) -> Option<&'static str>`
  - `pub fn build_of_tag(tag: &str) -> Option<u32>`
  - `pub fn parse_latest(json: &str, asset: &str) -> Result<Release, String>`
  - `impl Release { pub fn newer_than(&self, build: u32) -> bool }`

  Both structs derive `Clone, Debug, PartialEq`.

- [ ] **Step 1: Write failing tests** in `release.rs`:
  - `build_numbers_come_from_release_tags`:
    - `build_of_tag("v0.1.0-build.8") == Some(8)`;
    - `build_of_tag("v2.3.0-build.120") == Some(120)`;
    - `None` for `"v0.1.0"`, `"build.8"`, `"v0.1.0-build.x"`.
  - `the_latest_release_names_its_asset_and_checksum`: on the fixture with `SoundSync-windows-x64.zip`,
    expect:
    - build 8;
    - title `"SoundSync 0.1.0 (build 8)"`;
    - page url `https://github.com/Prushka/SoundSync/releases/tag/v0.1.0-build.8`;
    - asset size 7782375;
    - sha256 hex `6a98aa5e…04f4`;
    - the url ends with `/SoundSync-windows-x64.zip`.

    The macOS name gives size 13522724.
  - `releases_without_our_asset_or_checksum_are_refused`: `parse_latest` errors for an unknown asset name,
    for the fixture with `digest` removed, and for a digest that isn't `sha256:` + 64 hex digits.
  - `only_higher_builds_are_newer`: build 8 is `newer_than(7)`, but not `newer_than(8)` or `newer_than(9)`.
  - `each_os_has_its_asset`: `Os::Windows` → `SoundSync-windows-x64.zip`, `Os::MacOs` →
    `SoundSync-macos-universal.zip`, `Os::Other` → `None`.
- [ ] **Step 2:** `cargo test -p soundsync --bin soundsync update::release` fails to compile (missing items).
- [ ] **Step 3:** Implement with `serde_json::Value` or small `#[derive(Deserialize)]` structs.
  - `page_url` is `html_url`, `title` is `name`.
  - A release whose tag has no build number is an error ("not a SoundSync build").
- [ ] **Step 4:** Tests pass; clippy is clean for both targets.
- [ ] **Step 5:** Commit "Updates: parse GitHub release metadata".

### Task 2: Fetching (`fetch.rs`)

**Files:** Create `crates/app/src/update/fetch.rs`; Modify `update/mod.rs` (`pub mod fetch;`)

**Interfaces:**
- Consumes: `Release`, `Asset`, `parse_latest` (Task 1).
- Produces:
  - `pub const LATEST_URL: &str`
  - `pub struct Client`
  - `Client::new() -> Client`
  - `Client::latest(&self, url: &str, asset: &str) -> Result<Release, String>`
  - `Client::download(&self, asset: &Asset, progress: &mut dyn FnMut(u64)) -> Result<Vec<u8>, String>`

- [ ] **Step 1: Write failing tests.** Use a helper `serve(responses: Vec<(u16, Vec<u8>)>) -> String` that
  answers one HTTP/1.1 request per response on `127.0.0.1:0` and returns its base URL.
  - `latest_release_is_read_from_the_api`: serve the fixture; `latest(url, WINDOWS)` gives build 8.
  - `downloads_are_checked_against_the_release_checksum`:
    - serve bytes `b"abc"` with an `Asset` whose sha256 is SHA-256("abc") and size 3; the result is
      `b"abc"`, and progress saw 3;
    - with the wrong sha256, the error contains "checksum";
    - with size 4, the error contains "size".
  - `api_errors_are_reported`: a 403 reply gives an error containing "403".
- [ ] **Step 2:** Tests fail (no `fetch` module).
- [ ] **Step 3: Implement.**
  - **Agent:** `TlsConfig` with `TlsProvider::NativeTls` and `RootCerts::PlatformVerifier`,
    `user_agent("SoundSync/<version::version()>")`, a global timeout of 120 s, and `http_status_as_error`
    left on. Send `Accept: application/vnd.github+json` on the API call.
  - **Download:** read in 64 KiB chunks, calling `progress(received)`, and refuse above 256 MiB.
- [ ] **Step 4:** Tests pass; clippy is clean for both targets.
- [ ] **Step 5:** Commit "Updates: download releases with checksum checks".

### Task 3: Staging, swapping and relaunching (`install.rs`)

**Files:** Create `crates/app/src/update/install.rs`; Modify `update/mod.rs` (`pub mod install;`)

**Interfaces:**
- Consumes: `bundle_of` (`crate::macos_bundle`), `version::build_number`.
- Produces:
  - **Types and constants:**
    - `pub enum Target { Exe(PathBuf), Bundle(PathBuf) }` (Clone, Debug, PartialEq) with
      `fn path(&self) -> &Path` and `fn staging(&self) -> PathBuf` (`<parent>/.soundsync-update`);
    - `pub const STAGING: &str = ".soundsync-update"`;
    - `pub struct Staged { pub target: Target, pub new: PathBuf }` (Clone, Debug, PartialEq);
    - `pub struct Swapped { pub target: Target, pub old: PathBuf }` (Debug).
  - **Finding the target:**
    - `pub fn target_for(exe: &Path, build: Option<&str>, os: Os) -> Result<Target, String>` (pure);
    - `pub fn target() -> Result<Target, String>`.
  - **Staging and checking:**
    - `pub fn extract_exe(zip: &[u8], dest: &Path) -> Result<(), String>`;
    - `pub fn reports_build(version_output: &str, build: u32) -> bool`;
    - `pub fn stage(target: &Target, zip: &[u8], build: u32) -> Result<Staged, String>`.
  - **Swapping:** `pub fn swap(staged: &Staged) -> Result<Swapped, String>` and
    `Swapped::undo(&self) -> Result<(), String>`.
  - **Relaunching:**
    - `pub fn relaunch_args(args: &[OsString], visible: bool, os: Os) -> Vec<OsString>`;
    - `pub fn launch(target: &Target, args: &[OsString]) -> std::io::Result<()>`.
  - **After restart:**
    - `pub fn was_just_updated(target: &Target) -> bool` (`staging/old` exists);
    - `pub fn clean_up(target: &Target, give_up_after: Duration) -> bool`.

- [ ] **Step 1: Write failing tests:**
  - `dev_builds_and_loose_mac_binaries_do_not_update`:
    - `target_for(exe, None, Windows)` errors with the dev-build text;
    - for macOS, `/Users/x/bin/soundsync` gives "Only SoundSync.app updates itself.";
    - `/private/var/folders/a/AppTranslocation/B/d/SoundSync.app/Contents/MacOS/soundsync` gives the
      Applications text;
    - `/Applications/SoundSync.app/Contents/MacOS/soundsync` gives `Bundle("/Applications/SoundSync.app")`;
    - for Windows, `C:\x\soundsync.exe` with build `"8"` gives `Exe`.
  - `the_new_binary_must_report_the_build`:
    - `reports_build("soundsync 0.1.0 (build 9, abc1234)\n", 9)` and `"… (build 9)"` are true;
    - `"(build 90, …)"` with 9 and `"(dev)"` are false.
  - `the_exe_is_taken_from_the_release_zip`: build a zip in memory with `zip::ZipWriter`, holding
    `README.md` and `soundsync.exe` (bytes `b"MZ…"`). `extract_exe` writes exactly those bytes. A zip
    without `soundsync.exe` errors with "soundsync.exe".
  - `swapping_moves_the_old_copy_aside_and_undo_puts_it_back`:
    - in a temp dir, with `soundsync.exe` = "old" and `.soundsync-update/new/soundsync.exe` = "new",
      `swap` leaves "new" at the target and "old" at `.soundsync-update/old/soundsync.exe`;
    - `undo` restores "old";
    - a swap whose staged file is missing errors and leaves "old" in place.
  - `relaunch_keeps_the_window_hidden_when_it_was`: from args `--instance b --show --bind 127.0.0.1`:
    - Windows hidden → `--instance b --bind 127.0.0.1`;
    - Windows visible → the same plus `--show`;
    - macOS hidden → plus `--background`;
    - macOS visible → no flag;
    - an existing `--background` is dropped first.
  - `leftovers_without_an_old_copy_are_cleaned_quietly`:
    - only `.soundsync-update/new/x` exists: `was_just_updated` is false, and `clean_up` removes the
      folder and returns true;
    - with `old/` present, `was_just_updated` is true.
  - `staging_where_nothing_can_be_written_says_so`: a `Target::Exe` whose parent "folder" is a regular file
    makes `stage` fail with a message starting "SoundSync can't write to".
- [ ] **Step 2:** Tests fail (no module).
- [ ] **Step 3: Implement.**
  - **`stage`:**
    - Create `staging/new` (mapping errors to the can't-write text, with the build).
    - **Windows:** `extract_exe` into `new/<exe file name>`.
    - **macOS:** write the zip to `staging/update.zip`, then run `ditto -x -k`. Then run
      `codesign --verify --deep --strict new/SoundSync.app`.
    - Run the staged binary's `--version` (stdin null, captured output, killed after 30 s). Require
      `reports_build`.
    - On any error after creating the folder, remove `staging` and return the error.
  - **`swap`:** remove a stale `staging/old`, then do the two renames. If the second rename fails, rename
    back.
  - **`launch`:**
    - **Windows:** `Command::new(exe).args(args).current_dir(parent).stdin/stdout/stderr(null).spawn()`.
    - **macOS:** `/usr/bin/open -n <bundle> --args …`.
  - **`clean_up`:** `remove_dir_all(staging)` in a loop every 500 ms until it is gone or `give_up_after`
    has passed.
  - Code used only on one OS gets `#[cfg]` or `cfg_attr(…, allow(dead_code))`, as in `macos_bundle.rs`.
- [ ] **Step 4:** Tests pass. Mutation-check the rollback test (remove the rename-back and see it fail).
  Clippy is clean for both targets.
- [ ] **Step 5:** Commit "Updates: stage, swap and relaunch the running copy".

### Task 4: The updater (`mod.rs`)

**Files:** Modify `crates/app/src/update/mod.rs`

**Interfaces:**
- Consumes: Tasks 1–3.
- Produces:
  - `pub enum UpdateState`, with variants:
    - `Unavailable(String)`, `Idle`, `Checking`, `UpToDate { checked: SystemTime }`;
    - `Available(Release)`, `Downloading { release: Release, received: u64, total: u64 }`;
    - `Ready { release: Release, staged: Staged }`, `Restarting(Release)`;
    - `Failed { error: String, release: Option<Release> }`.

    It derives Clone, Debug and PartialEq. `Restarting` is shown after the UI took `Ready`; this adds
    to the spec's list.
  - `pub struct Updater`, with:
    - `Updater::start(target: Result<Target, String>, current_build: Option<u32>, api_url: String,
      first_check: Duration, every: Duration, on_change: Box<dyn Fn() + Send>) -> Updater`;
    - `state(&self) -> UpdateState`;
    - `check_now(&self)`;
    - `install(&self)`;
    - `take_ready(&self) -> Option<Staged>` (it moves `Ready` to `Restarting`);
    - `swap_failed(&self, error: String)`.

- [ ] **Step 1: Write failing tests** (all with the `serve` helper from Task 2, made `pub(super)` under
  `#[cfg(test)]`):
  - `a_newer_release_becomes_available`: start with build 7 and `first_check` 0; `wait_until` the state is
    `Available` with build 8.
  - `the_same_build_is_up_to_date`: current build 8 gives `UpToDate`.
  - `failed_automatic_check_stays_quiet`: the server answers 403 and current build 7. After the check the
    state is `Idle`. Then `check_now()` against a second failing server gives `Failed` with an error
    containing "403".
  - `install_without_an_available_release_does_nothing`: `install()` in `Idle` leaves `Idle`, and no
    request reaches the server (the server saw only the API call).
  - `copies_that_cannot_update_never_check`: a `target` error gives `Unavailable(that text)`, and the
    server sees no request.
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3: Implement.**
  - **Thread:** one `soundsync-update` thread that receives `Check` / `Install` over a crossbeam channel,
    with `recv_timeout` until the next periodic check.
  - **Shared state:** `Arc<Mutex<UpdateState>>`; call `on_change()` after each change.
  - **Install:** taken only from `Available` or `Failed { release: Some(..) }`. It runs
    `download` → `Downloading` progress (at most about 10 updates a second) → `stage` → `Ready`.
  - **Automatic check failure:** restore the previous state and log a warning.
- [ ] **Step 4:** Tests pass; clippy is clean for both targets.
- [ ] **Step 5:** Commit "Updates: background checks and installs".

### Task 5: UI, tray and restart

**Files:**
- Modify: `crates/app/src/ui/format.rs` (text plus tests)
- Modify: `crates/app/src/ui/panels.rs` (header line, Settings ▸ Updates)
- Modify: `crates/app/src/ui/tray.rs` (update item, `Action::InstallUpdate`)
- Modify: `crates/app/src/ui/app.rs` (owns the `Updater`, swap on `Ready`, relaunch in `on_exit`,
  "Updated to" notice and cleanup at start, `_instance` becomes `Option<InstanceGuard>`)

**Interfaces:**
- Consumes: `Updater`, `UpdateState`, `install::{target, swap, relaunch_args, launch, was_just_updated,
  clean_up}`.
- Produces:
  - `pub fn update_line(s: &UpdateState) -> Option<String>`;
  - `pub fn update_status(s: &UpdateState) -> String`;
  - `Tray::set_update(&mut self, build: Option<u32>)`.

- [ ] **Step 1: Write failing tests** in `format.rs`:
  - `update_line_names_the_build_and_the_group`:
    - `Available` (build 9) gives exactly the spec's sentence;
    - `Downloading` 5 of 10 MB gives "Downloading build 9… 50%";
    - `Failed` with a release gives "Update to build 9 failed: <error>";
    - `Idle`, `UpToDate` and `Unavailable` give `None`.
  - `update_status_says_what_the_updater_is_doing`:
    - `UpToDate` at a fixed time gives "Up to date (checked HH:MM)";
    - `Unavailable(x)` gives x;
    - `Checking` gives "Checking for updates…".
- [ ] **Step 2:** Tests fail.
- [ ] **Step 3: Implement.**
  - **`format.rs`:** local time via `SystemTime`. Use UTC-free formatting. If no local-time helper
    exists, show the minutes since the check: "Up to date (checked 3 min ago)". Adjust the test to match.
  - **Header:** after the notice line, the `update_line` in the notice blue, plus:
    - [Update and restart] and [What's new] (`open_url(page_url)`, moved to `panels.rs` and shared with
      `vbcable.rs`) when `Available`;
    - `egui::ProgressBar` when `Downloading`;
    - [Retry] and [Release page] when `Failed`.
  - **Settings ▸ Updates group:** the version label (moved from the footer), `update_status`,
    [Check for updates] (not while `Checking`/`Downloading`/`Unavailable`), and [Update and restart] when
    `Available`.
  - **Tray:** a `MenuItem` "Update to build {n}" inserted before the last separator when available and
    removed otherwise. The click sends `Action::InstallUpdate`, which calls `updater.install()`.
  - **App:**
    - `logic()`: every 200 ms sync the tray. If `take_ready()` gives `Staged`, `swap` it. On success
      store `relaunch = Some((swapped, relaunch_args(args_os().skip(1), visible, Os::current())))`, set
      quitting and close the viewport. On error call `swap_failed`.
    - `on_exit`: drop the engine. If `relaunch`, drop `_instance`, then `launch`. If that fails, log it,
      `undo`, and `launch` the old copy.
    - **At start:** `install::target()`. If `was_just_updated`, set the notice. Spawn `clean_up(…, 20 s)`
      on a thread. Start the `Updater` with `fetch::LATEST_URL`, 10 s, 6 h, and `request_repaint`.
- [ ] **Step 4:** Tests pass, the full suite passes, and clippy is clean for both targets.
- [ ] **Step 5:** Commit "Updates: header, settings and tray; restart into the new copy".

### Task 6: Uninstall, docs, end-to-end run

**Files:**
- Modify: `scripts/uninstall-windows.ps1` (remove `$InstallDir\.soundsync-update` before the
  empty-folder check)
- Modify: `README.md` (Updating: automatic check, Update and restart, contacts api.github.com every 6 h,
  the protocol note; macOS permissions may be asked again)
- Modify: `docs/AGENTS.md`:
  - Repository layout row for `update/`;
  - Architecture ▸ Updates (a short section);
  - Decision log rows (native TLS for the cross-check; in-place rename swap; relaunch after dropping the
    instance guard; install on click);
  - Open items: macOS updates unverified;
  - a History row named by summary line.

- [ ] **Step 1: End to end on Windows**, against the real release:
  1. `SOUNDSYNC_BUILD=7 cargo build --release -p soundsync`, then copy the exe to a temp folder.
  2. Write `%APPDATA%\SoundSync-u\config.toml` with `port = 47812` and
     `output_device = "Speakers (Steam Streaming Speakers)"`.
  3. Run `<temp>\soundsync.exe --instance u --bind 127.0.0.1 --no-mdns --show`.
  4. Wait for "SoundSync build 8 is available…", then invoke "Update and restart" via `click-tab.ps1`.
  5. Expect:
     - `<temp>\soundsync.exe --version` prints `(build 8`;
     - a soundsync process from that path runs with the window visible (`printwindow.ps1`);
     - `<temp>\.soundsync-update` is gone within 20 s.
  6. Quit it (`--instance u --quit`), and delete the temp folder and both `SoundSync-u` folders.
- [ ] **Step 2:** The full suite (both ways) and clippy (both targets) are green.
- [ ] **Step 3:** Commit "Updates: uninstall cleanup, README and guide", with the docs and the History
  row in it.
