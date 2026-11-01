//! Command-line interface: the GUI by default, plus diagnostic subcommands.

use std::path::PathBuf;

use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use soundsync_audio::{AudioHost, DeviceInfo, DeviceSelector, Direction, OutputOptions, RenderCallback, RenderTiming, StreamFormat};
use soundsync_core::Nanos;
use soundsync_core::layout::describe;

#[derive(Parser, Debug)]
#[command(name = "soundsync", version = soundsync_core::version::version(), about = "Synchronized multi-room audio over the LAN")]
pub struct Cli {
    /// Use this config file instead of the per-user default.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Run a separate instance (own config, logs and state), e.g. for local testing.
    #[arg(long, global = true)]
    pub instance: Option<String>,
    /// Open the window on start (otherwise SoundSync starts in the tray).
    #[arg(long)]
    pub show: bool,
    /// Ask a running instance to quit.
    #[arg(long)]
    pub quit: bool,
    /// Start hidden in the tray / menu bar (used by the login item).
    #[arg(long)]
    pub background: bool,
    /// (Testing) bind the network to this address instead of all interfaces.
    #[arg(long = "bind")]
    pub gui_bind: Option<std::net::Ipv4Addr>,
    /// (Testing) disable mDNS discovery.
    #[arg(long = "no-mdns")]
    pub gui_no_mdns: bool,
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List audio devices with their formats and periods.
    Devices,
    /// Put back Windows default output devices SoundSync switched and could not restore (it
    /// normally does that itself on exit or at its next start; the uninstaller runs this).
    RestoreDefaults,
    /// Play a test tone (or pulses) on a device.
    Tone {
        #[arg(long, default_value = "default")]
        device: String,
        #[arg(long, default_value_t = 440.0)]
        freq: f64,
        #[arg(long, default_value_t = 3.0)]
        seconds: f64,
        /// Play a click every 200 ms instead of a sine.
        #[arg(long)]
        pulses: bool,
    },
    /// Measure loopback timing of a device, or the relative delay between two devices.
    Measure {
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        a: Option<String>,
        #[arg(long)]
        b: Option<String>,
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
    },
    /// Run a node without the GUI.
    Run {
        /// Broadcast this PC's audio.
        #[arg(long)]
        broadcast: bool,
        /// "virtual" (VB-CABLE, full sync) or "direct" (capture an output device).
        #[arg(long, value_parser = parse_mode)]
        mode: Option<soundsync_core::MasterMode>,
        /// Device to capture.
        #[arg(long)]
        capture: Option<String>,
        /// Output that plays this PC's own stream in virtual mode ("null" to discard).
        #[arg(long)]
        monitor: Option<String>,
        /// Output for streams from other PCs ("null" to discard).
        #[arg(long)]
        output: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Address to bind (default all interfaces).
        #[arg(long)]
        bind: Option<std::net::Ipv4Addr>,
        /// Contact this peer directly (host:port); repeatable.
        #[arg(long = "peer")]
        peers: Vec<String>,
        /// Disable mDNS discovery.
        #[arg(long)]
        no_mdns: bool,
        /// Exit after this many seconds.
        #[arg(long)]
        seconds: Option<f64>,
    },
    /// Write the app icon as a PNG and/or Info.plist (used by the macOS bundle script).
    GenIcon {
        #[arg(long)]
        png: Option<PathBuf>,
        #[arg(long)]
        plist: Option<PathBuf>,
    },
}

fn parse_mode(s: &str) -> Result<soundsync_core::MasterMode, String> {
    match s.to_ascii_lowercase().as_str() {
        "virtual" => Ok(soundsync_core::MasterMode::Virtual),
        "direct" => Ok(soundsync_core::MasterMode::Direct),
        _ => Err("expected \"virtual\" or \"direct\"".into()),
    }
}

fn device_line(d: &DeviceInfo) -> String {
    format!(
        "{} [{}] {} — {} {} Hz, period {:.1}/{:.1} ms",
        if d.is_default { "*" } else { " " },
        if d.active { "active" } else { "inactive" },
        d.name,
        describe(d.mix_format),
        d.mix_format.sample_rate,
        d.default_period_ns as f64 / 1e6,
        d.min_period_ns as f64 / 1e6,
    )
}

pub fn devices(host: &dyn AudioHost) -> anyhow::Result<()> {
    for (title, dir) in [("Playback devices", Direction::Render), ("Recording devices", Direction::Capture)] {
        println!("{title}:");
        let list = host.devices(dir)?;
        if list.is_empty() {
            println!("  (none)");
        }
        for d in &list {
            println!("{}", device_line(d));
        }
    }
    Ok(())
}

#[derive(Default)]
struct ToneStats {
    calls: u64,
    frames: u64,
    first_dac: Option<(Nanos, u64)>,
    last_dac: Option<(Nanos, u64)>,
}

struct Tone {
    freq: f64,
    pulses: bool,
    frame: u64,
    stats: Arc<Mutex<ToneStats>>,
}

impl RenderCallback for Tone {
    fn render(&mut self, out: &mut [f32], format: StreamFormat, t: RenderTiming) {
        let ch = format.channels as usize;
        let rate = format.sample_rate as f64;
        let pulse_every = (format.sample_rate / 5) as u64;
        for frame in out.chunks_exact_mut(ch) {
            let x = if self.pulses {
                if self.frame.is_multiple_of(pulse_every) { 0.5 } else { 0.0 }
            } else {
                // -20 dBFS sine
                0.1 * (std::f64::consts::TAU * self.freq * self.frame as f64 / rate).sin() as f32
            };
            frame.fill(x);
            self.frame += 1;
        }
        let mut s = self.stats.lock().unwrap();
        let before = s.frames;
        s.first_dac.get_or_insert((t.dac_time_ns, before));
        s.last_dac = Some((t.dac_time_ns, before));
        s.calls += 1;
        s.frames += (out.len() / ch) as u64;
    }
}

pub fn tone(host: &dyn AudioHost, device: &str, freq: f64, seconds: f64, pulses: bool) -> anyhow::Result<()> {
    let stats = Arc::new(Mutex::new(ToneStats::default()));
    let sel = DeviceSelector::parse(device);
    let stream = host.open_output(&sel, OutputOptions::default(), Box::new(Tone { freq, pulses, frame: 0, stats: stats.clone() }))?;
    let fmt = stream.format();
    println!(
        "Playing {} on {sel}: {} {} Hz, reported latency {:.1} ms",
        if pulses { "pulses" } else { "tone" },
        describe(fmt),
        fmt.sample_rate,
        stream.latency_ns() as f64 / 1e6
    );
    std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    drop(stream);
    let s = stats.lock().unwrap();
    let period_ms = if s.calls > 0 { s.frames as f64 / s.calls as f64 / fmt.sample_rate as f64 * 1e3 } else { 0.0 };
    println!("{} callbacks, average period {period_ms:.2} ms", s.calls);
    if let (Some((d0, f0)), Some((d1, f1))) = (s.first_dac, s.last_dac)
        && f1 > f0
    {
        let nominal = (f1 - f0) as f64 * 1e9 / fmt.sample_rate as f64;
        println!("dac-time slope {:.6} (1.0 = device clock matches the host clock)", (d1 - d0) as f64 / nominal);
    }
    Ok(())
}

pub fn measure(host: &dyn AudioHost, device: Option<String>, a: Option<String>, b: Option<String>, seconds: f64) -> anyhow::Result<()> {
    use soundsync_audio::measure::{measure_pair, probe_loopback};
    match (device, a, b) {
        (Some(d), _, _) => {
            let sel = DeviceSelector::parse(&d);
            let r = probe_loopback(host, &sel)?;
            println!("{sel}:");
            println!("  loopback timestamp -> DAC: {:+.2} ms", r.offset_ns as f64 / 1e6);
            println!("  bit-exact loopback: {}", if r.bit_exact { "yes" } else { "NO" });
            let note = if (r.gain - 1.0).abs() < 1e-6 {
                "unity (loopback is pre-volume, or the endpoint volume is 100%)"
            } else {
                "endpoint volume is applied before loopback"
            };
            println!("  loopback gain: {:.6} ({note})", r.gain);
            Ok(())
        }
        (None, Some(a), Some(b)) => {
            let (sa, sb) = (DeviceSelector::parse(&a), DeviceSelector::parse(&b));
            match measure_pair(host, &sa, &sb, seconds)? {
                Some(d) => println!("{sb} plays {:.2} ms {} {sa}", (d as f64 / 1e6).abs(), if d >= 0 { "after" } else { "before" }),
                None => println!("no common signal found on {sa} and {sb} (is something playing on both?)"),
            }
            Ok(())
        }
        _ => anyhow::bail!("use --device <name>, or --a <name> --b <name>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_defaults_command_parses() {
        // The uninstaller runs this before deleting SoundSync.
        let cli = Cli::try_parse_from(["soundsync", "restore-defaults"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::RestoreDefaults)));
        let cli = Cli::try_parse_from(["soundsync", "--instance", "x", "restore-defaults"]).unwrap();
        assert_eq!(cli.instance.as_deref(), Some("x"));
    }
}
