//! Read-only endpoint probes and an explicit default-device snapshot/restore utility for the controlled driver test.
use windows::Win32::Media::Audio::{AUDCLNT_SHAREMODE_EXCLUSIVE, ISpatialAudioClient, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_DIGITAL_PLUS, KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_DIGITAL_PLUS_ATMOS, KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_MLP, KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_MAT20, KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_MAT21};
use windows::Win32::System::Com::CLSCTX_INPROC_SERVER;
use windows::core::GUID;

#[repr(C, packed(1))]
struct Iec61937 {
    format: WAVEFORMATEXTENSIBLE,
    encoded_rate: u32,
    encoded_channels: u32,
    encoded_bytes_per_sec: u32,
}

fn format(subformat: GUID, mat: bool, rate: u32) -> Iec61937 {
    let channels = if mat { 8 } else { 2 };
    Iec61937 {
        format: WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xfffe,
                nChannels: channels,
                nSamplesPerSec: 192_000,
                nAvgBytesPerSec: 192_000 * u32::from(channels) * 2,
                nBlockAlign: channels * 2,
                wBitsPerSample: 16,
                cbSize: 34,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 16 },
            dwChannelMask: if mat { 0xff } else { 0x3f },
            SubFormat: subformat,
        },
        encoded_rate: rate,
        encoded_channels: if mat { 8 } else { 6 },
        encoded_bytes_per_sec: 0,
    }
}

fn probe() -> Result<(), Box<dyn std::error::Error>> {
    wasapi::initialize_mta().ok()?;
    assert_eq!(std::mem::size_of::<Iec61937>(), 52);
    let devices = wasapi::DeviceEnumerator::new()?.get_device_collection(&wasapi::Direction::Render)?;
    for device in devices.into_iter() {
        let device = device?;
        if device.get_state()? != wasapi::DeviceState::Active { continue; }
        println!("{}", device.get_friendlyname()?);
        let client = device.get_iaudioclient()?;
        let mix = client.get_mixformat()?;
        println!("  PCM mix: {} channels, {} Hz, mask 0x{:X}", mix.get_nchannels(), mix.get_samplespersec(), mix.get_dwchannelmask());
        let spatial = unsafe { device.as_immdevice().Activate::<ISpatialAudioClient>(CLSCTX_INPROC_SERVER, None) };
        match spatial {
            Ok(spatial) => {
                println!("  Spatial client available; dynamic object count: {:?}; static mask: {:?}",
                    unsafe { spatial.GetMaxDynamicObjectCount() }, unsafe { spatial.GetNativeStaticObjectTypeMask() });
            }
            Err(e) => println!("  Spatial client unavailable: {e}"),
        }
        for (label, guid, mat) in [
            ("Dolby Digital Plus", KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_DIGITAL_PLUS, false),
            ("Dolby Digital Plus Atmos", KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_DIGITAL_PLUS_ATMOS, false),
            ("Dolby TrueHD / MAT 1.0", KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_MLP, true),
            ("Dolby MAT 2.0", KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_MAT20, true),
            ("Dolby MAT 2.1", KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_MAT21, true),
        ] {
            for rate in if mat { &[48_000, 96_000][..] } else { &[48_000][..] } {
                let encoded = format(guid, mat, *rate);
                let hr = unsafe { client.as_iaudioclient().IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &raw const encoded.format.Format, None) };
                println!("  {label}, content {rate} Hz: {} (0x{:08X})", if hr.0 == 0 { "supported" } else { "not accepted" }, hr.0 as u32);
            }
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use soundsync_audio::{DefaultDevices, DefaultRole, win::policy};
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => probe(),
        [command, path] if command == "--snapshot-defaults" => {
            let defaults = policy::read_defaults()?;
            let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
            serde_json::to_writer_pretty(file, &defaults)?;
            println!("Saved all three Windows default output roles to {path}");
            Ok(())
        }
        [command, path] if command == "--restore-defaults" => {
            let saved: DefaultDevices = serde_json::from_reader(std::fs::File::open(path)?)?;
            let current = policy::read_defaults()?;
            let roles = [
                (&saved.console, &current.console, DefaultRole::Console),
                (&saved.multimedia, &current.multimedia, DefaultRole::Multimedia),
                (&saved.communications, &current.communications, DefaultRole::Communications),
            ];
            for (id, _, _) in &roles {
                if !id.starts_with("{0.0.0.") || !policy::endpoint_active(id) {
                    return Err("A saved render endpoint is unavailable; no default output was changed".into());
                }
            }
            for (id, now, role) in roles {
                if id != now { policy::set_default_render(id, role)?; }
            }
            if policy::read_defaults()? != saved { return Err("Default-output restoration did not persist".into()); }
            println!("All three default output roles match the saved snapshot");
            Ok(())
        }
        _ => Err("Usage: soundsync-atmos-endpoint-probe [--snapshot-defaults FILE | --restore-defaults FILE]".into()),
    }
}
