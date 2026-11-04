# Experimental Dolby MAT capture input

This is a **built research prototype without a production signature**, not Atmos support in SoundSync.
It has not been loaded or tested with Dolby Access. SoundSync still transports
PCM and cannot forward the captured stream to receivers. Do not install this as
a replacement for VB-CABLE on the assumption that multi-room Atmos now works.

**Current owner constraint (2026-11-03):** Atmos capture must work system-wide,
including games, with Secure Boot and all BIOS settings unchanged. Do not load
this test-signed prototype on the owner's PC or weaken driver-signing or memory
integrity protection to make it load. Earlier controlled-test authorization does
not override this later restriction. Source/player integration alone does not
meet the requested scope. Further host deployment requires a tested driver
accepted by normal Windows signing policy; none has been verified for this task.

The immediate experiment is to establish whether Windows' installed Dolby
renderer will send intact MAT to a virtual HDMI endpoint on this machine. Only
after that succeeds can native transport and actual receiver decoding be tested.

## Why a different input is needed

The installed VB-CABLE endpoints reject the tested Dolby IEC 61937 formats.
WASAPI shared-mode loopback returns the audio-engine mix, not the Dolby MAT
stream sent to HDMI. Enabling Atmos at a later endpoint does not recover the
original object metadata from that PCM. The Windows spatial audio API lets an
application submit objects for rendering; it is not a system-wide object capture API.

An experimental virtual sink can instead advertise the encoded formats and
capture bytes at the driver's render buffer, after the Dolby renderer. The
[SpatialAudioLab research project](https://github.com/barralutz/spatial-audio-lab/blob/main/driver/README.md)
reports this working with MAT 2.1. That establishes a possible approach, not
compatibility with this Windows build or a supported commercial virtual cable.
No code from that project is incorporated here.

The [VirtualDrivers 25.7.14 release](https://github.com/VirtualDrivers/Virtual-Audio-Driver/releases/tag/25.7.14)
was also checked. Its `.sys` and catalog have valid SignPath Authenticode signatures,
but the actual downloaded package failed `signtool verify /kp /c` on this PC.
Its tagged `Source/Filters/speakerwavtable.h` advertises stereo PCM and no Dolby
MAT formats. A release labelled "signed" therefore does not establish either
native Atmos capture or acceptance under this PC's kernel-signing policy.

This prototype applies a small patch to
[Microsoft SysVAD](https://github.com/microsoft/Windows-driver-samples/tree/f4c1ff842b6a0001152f44cb61788b216a1c7627/audio/sysvad).
The pinned upstream repository is licensed under **MS-PL**, reproduced in
`UPSTREAM-LICENSE.txt`; the upstream portions of `sysvad.patch` and the derived
driver retain that license and Microsoft copyright notices. Do not infer an MIT
license from descriptions in other projects. The WDK/SDK packages retain their
own licenses and are downloaded as build dependencies, not redistributed.

## Build and checks

From the SoundSync repository root:

```powershell
.\experiments\atmos-capture\build.ps1
```

The script uses an existing x64 MSVC compiler and downloads SHA-256-pinned
Microsoft NuGet SDK/WDK packages to `target/atmos-driver-source`. It pins SysVAD
to commit `f4c1ff842b6a0001152f44cb61788b216a1c7627`, applies `sysvad.patch`,
and rejects unrelated tracked source edits. It builds the driver and reader,
runs native regressions, validates the INF, and generates an unsigned catalog.
It installs no software, certificates or devices and changes no Windows settings.

Outputs:

- `dist/atmos-capture-probe/`: `.sys`, `.inf`, `.cat`, `capture-reader.exe`,
  SHA-256 manifest, this document and the upstream license.
- `dist/SoundSync-Atmos-capture-probe-unsigned.zip`: the same package.
- `target/atmos-driver-source/local-build/`: native test executables and objects.

The native tests exercise actual shared capture-buffer code, including byte
fidelity, wrap, overflow, overwritten DMA data, reader lifetime, protected audio,
stream ownership and draining. Reader tests reject duplicate or missing bytes,
invalid lengths, changing formats/generations and protected or conflicting streams.
The initial buffer stub failed the byte-fidelity test; a mutation removing the
reader's sequence check failed the duplicate-byte test.

Compilation and INF/catalog validation cannot prove kernel runtime safety,
Dolby activation, Atmos metadata preservation, HDMI compatibility or synchronization.

Local checks on 2026-11-03 passed: native compilation with warnings as errors,
both native regression programs, INF validation, catalog generation, 294 SoundSync
tests with normal timers and again with `SS_FAKE_MIN_SLEEP_MS=20`, and Clippy on
Windows and `aarch64-apple-darwin`. The driver has CFG, NX and integrity-check flags
and no section that is both writable and executable. Windows reports the `.sys`
and `.cat` as `NotSigned`. Opening the unloaded capture device correctly returned
Windows error 2; no live capture test was performed.

## What the driver does

- Exposes only `SoundSync Atmos Probe (experimental)`, hardware ID
  `Root\SoundSyncAtmosProbe`. It does not replace the installed VB-CABLE driver.
- Adds PCM 7.1 at 48 kHz and the surround-mask variants of MAT 2.0/2.1 to the
  sample's existing HDMI formats. It does not implement a Dolby encoder.
- Captures only Dolby MLP/MAT carriers at 8 channels, 192 kHz, 16 bits. It keeps
  those bytes unchanged in a 4 MiB nonpaged buffer and never decodes in kernel.
- An administrator must open `\\.\SoundSyncAtmosProbe` to enable capture.
  Closing the reader disables capture and discards unread data. IOCTL reads are
  bounded at 64 KiB and use buffered I/O; kernel pointers are never returned.
- Copy-protected streams close the capture gate and discard buffered data.
  The driver does not bypass content protection.
- Conflicting encoded streams are reported instead of mixed. Buffer overflow
  and overwritten DMA data are reported instead of hidden.
- SysVAD's file writer and Bluetooth/USB sideband discovery are disabled.
  SysVAD's sample WASAPI loopback remains a generated test tone: **do not use
  this endpoint as SoundSync's existing loopback input**.

## Preparing a research test package

After reviewing the unsigned package, create a local test-signed copy with:

```powershell
.\experiments\atmos-capture\prepare-test-package.ps1
```

This writes `dist/atmos-capture-test/`, including a SHA-256 manifest and a public
test certificate valid for seven days. It embeds the test signature in the driver,
regenerates and signs the catalog, then deletes the temporary private key. It does
not trust the certificate, install the driver or change boot policy. It refuses
to overwrite a pending test package. A test signature is not Microsoft production
signing and cannot make the driver load under ordinary Secure Boot policy.

`soundsync-atmos-endpoint-probe.exe` queries endpoint capabilities without opening
audio streams. Its explicit `--snapshot-defaults FILE` and `--restore-defaults FILE`
modes save and restore all three Windows output roles, using SoundSync's existing
policy implementation. Snapshot refuses to overwrite a file; restore checks all
saved endpoints before changing any role and skips roles already correct.

The owner initially authorized a controlled test on this PC on 2026-11-03. A local
test-signed package and default-output snapshot were prepared. The read-only
administrator preflight completed: Secure Boot and Memory Integrity are enabled,
and Windows test-signing mode is off. No certificate has been trusted and no
driver or boot policy installed/changed. The later requirement to preserve BIOS
and Windows security settings excludes loading this prototype on the owner's PC.

After a report of intermittent white noise, the owner confirmed clean sustained
playback on the existing LG output with Dolby Atmos for home theater enabled.
A separate generated 7.1.4 PCM test used Windows Spatial Audio with 12 static
objects; the owner heard clean audio when only the four height positions played.
All five active outputs' original mute/volume settings were restored, and all
three default-output roles remained unchanged. The noise's cause was not
identified. This validates local height playback, not native Atmos capture,
preserved moving-object metadata, network distribution or receiver synchronization.

## First live test in a suitable development environment

Installation and test-boot/signing changes require separate approval under
`docs/AGENTS.md` ("Don't install drivers or software"). A development driver can
crash Windows. Use a disposable Windows test machine when possible. The prepared
package is unsigned and normal Windows driver enforcement will reject it.
Do not silently disable Secure Boot, memory integrity, BitLocker protection or
driver signature enforcement to make it load. No such setting has been changed.

These research steps do not authorize installation on the owner's current PC.
Once a separate test environment and installation are explicitly authorized, the test is:

1. Install only this package, with the required test signing handled explicitly.
   Keep ordinary Windows default outputs and the existing SoundSync route unchanged.
2. Select the probe in Dolby Access and try Dolby Atmos for Home Theater. Verify
   spatial endpoint capabilities. Advertising MAT alone may be insufficient;
   activation on Windows build 26300 has not been verified. No private endpoint
   registry patches or copied Dolby provider registrations are included.
3. Use a permitted, unprotected Dolby test source. Run the reader from an elevated
   terminal, with a new filename:

   ```powershell
   .\capture-reader.exe --status
   .\capture-reader.exe --capture .\dolby-test.ssatm 10
   ```

4. Require sustained encoded bytes, zero lost bytes, a stable format/generation
   and valid MAT bursts. The reader detects MAT/MLP preambles; it does **not**
   decode or authenticate Atmos object metadata. A successful recording still
   requires an independent decoder or verified AVR playback test.
5. Remove only this experiment's device/package after the test, restore any
   explicitly changed source selection, and return Windows to normal boot.

The `.ssatm` file starts with `SSATMOS1`, then repeated 96-byte `CaptureHeader`
records followed by their `data_bytes` payloads. It is a diagnostic container,
not a WAV file and not something to play as PCM. Failed captures are retained
for diagnosis and the reader exits nonzero.

## Remaining work before native Atmos can be claimed

The existing network protocol, mixer, limiter and drift resampler operate on
f32 PCM. An encoded path must bypass them, preserve packet ordering and burst
boundaries, and handle independent receiver clocks without modifying encoded
samples. Every receiver needs a tested encoded HDMI output path; eARC only
covers the TV-to-audio-system connection. Windows HDMI output, macOS output,
object metadata preservation and multi-device synchronization are all unverified.

Changing endpoint labels, upmixing PCM, or showing an Atmos indicator is not
evidence that the original Atmos scene survived the chain.

References: [Windows spatial sound](https://learn.microsoft.com/en-us/windows/win32/coreaudio/spatial-sound),
[WASAPI loopback](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording),
[IEC 61937 formats](https://learn.microsoft.com/en-us/windows/win32/coreaudio/representing-formats-for-iec-61937-transmissions).
