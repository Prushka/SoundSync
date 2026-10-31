//! Group presentation delay D for virtual-mode streams: the time from capture to emission that
//! every node aims for. Auto mode sizes D to the slowest node; it only grows while a stream runs
//! (shrinking would need a glitch), and is recomputed freely when a stream starts.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::Nanos;

pub const MIN_DELAY_NS: Nanos = 20_000_000;
pub const MAX_DELAY_NS: Nanos = 2_000_000_000;
pub const DELAY_QUANTUM_NS: Nanos = 5_000_000;

/// `"auto"` or a fixed delay in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum DelaySetting {
    #[default]
    Auto,
    Manual(f64),
}

impl Serialize for DelaySetting {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            DelaySetting::Auto => s.serialize_str("auto"),
            DelaySetting::Manual(ms) => s.serialize_f64(*ms),
        }
    }
}

impl<'de> Deserialize<'de> for DelaySetting {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = DelaySetting;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("\"auto\" or a number of milliseconds")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<DelaySetting, E> {
                if v.eq_ignore_ascii_case("auto") {
                    Ok(DelaySetting::Auto)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<DelaySetting, E> {
                Ok(DelaySetting::Manual(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<DelaySetting, E> {
                Ok(DelaySetting::Manual(v as f64))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<DelaySetting, E> {
                Ok(DelaySetting::Manual(v as f64))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Clone, Debug)]
pub struct DelayPolicy {
    setting: DelaySetting,
    margin_ns: Nanos,
    current: Option<Nanos>,
}

fn clamp(ns: Nanos) -> Nanos {
    ns.clamp(MIN_DELAY_NS, MAX_DELAY_NS)
}

impl DelayPolicy {
    pub fn new(setting: DelaySetting, margin_ns: Nanos) -> Self {
        Self { setting, margin_ns, current: None }
    }

    fn compute(&self, requirements: &[Nanos]) -> Nanos {
        match self.setting {
            DelaySetting::Manual(ms) => clamp((ms * 1e6).round() as Nanos),
            DelaySetting::Auto => {
                let need = requirements.iter().copied().max().unwrap_or(0).max(0) + self.margin_ns;
                let quantized = (need + DELAY_QUANTUM_NS - 1) / DELAY_QUANTUM_NS * DELAY_QUANTUM_NS;
                clamp(quantized)
            }
        }
    }

    /// Picks D for a new stream; may be lower than before.
    pub fn on_stream_start(&mut self, requirements: &[Nanos]) -> Nanos {
        let d = self.compute(requirements);
        self.current = Some(d);
        d
    }

    /// Re-evaluates D while a stream runs. In auto mode D only grows. Returns the new value when
    /// it changed.
    pub fn update(&mut self, requirements: &[Nanos]) -> Option<Nanos> {
        let d = self.compute(requirements);
        let changed = match (self.setting, self.current) {
            (_, None) => true,
            (DelaySetting::Auto, Some(cur)) => d > cur,
            (DelaySetting::Manual(_), Some(cur)) => d != cur,
        };
        changed.then(|| {
            self.current = Some(d);
            d
        })
    }

    /// Switches mode. Manual values apply immediately; auto keeps the current D until requirements
    /// push it up (or the next stream start recomputes it).
    pub fn set_setting(&mut self, s: DelaySetting) -> Nanos {
        self.setting = s;
        if let DelaySetting::Manual(_) = s {
            self.current = Some(self.compute(&[]));
        }
        self.current.unwrap_or(MIN_DELAY_NS)
    }

    pub fn setting(&self) -> DelaySetting {
        self.setting
    }

    pub fn current(&self) -> Option<Nanos> {
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Nanos = 1_000_000;

    fn auto() -> DelayPolicy {
        DelayPolicy::new(DelaySetting::Auto, 5 * MS)
    }

    #[test]
    fn auto_takes_max_requirement_plus_margin_quantized() {
        assert_eq!(auto().on_stream_start(&[12 * MS, 37_200_000]), 45 * MS);
        assert_eq!(auto().on_stream_start(&[35 * MS]), 40 * MS, "exact multiples are not rounded up");
    }

    #[test]
    fn auto_never_decreases_in_update() {
        let mut p = auto();
        p.on_stream_start(&[100 * MS]);
        assert_eq!(p.update(&[10 * MS]), None);
        assert_eq!(p.current(), Some(105 * MS));
    }

    #[test]
    fn auto_increases_when_requirement_grows() {
        let mut p = auto();
        p.on_stream_start(&[30 * MS]);
        assert_eq!(p.update(&[150 * MS]), Some(155 * MS));
        assert_eq!(p.update(&[150 * MS]), None, "unchanged requirement reports no change");
        assert_eq!(p.current(), Some(155 * MS));
    }

    #[test]
    fn update_before_start_initializes() {
        let mut p = auto();
        assert_eq!(p.current(), None);
        assert_eq!(p.update(&[30 * MS]), Some(35 * MS));
    }

    #[test]
    fn stream_start_recomputes_downward() {
        let mut p = auto();
        p.on_stream_start(&[150 * MS]);
        assert_eq!(p.on_stream_start(&[40 * MS]), 45 * MS);
    }

    #[test]
    fn manual_overrides() {
        let mut p = DelayPolicy::new(DelaySetting::Manual(80.0), 5 * MS);
        assert_eq!(p.on_stream_start(&[200 * MS]), 80 * MS);
        assert_eq!(p.update(&[300 * MS]), None);
        assert_eq!(p.set_setting(DelaySetting::Manual(120.5)), 120_500_000);
        assert_eq!(p.current(), Some(120_500_000));
        p.set_setting(DelaySetting::Auto);
        assert_eq!(p.update(&[300 * MS]), Some(305 * MS));
    }

    #[test]
    fn clamped_to_min_and_max() {
        assert_eq!(auto().on_stream_start(&[]), MIN_DELAY_NS);
        assert_eq!(auto().on_stream_start(&[5_000 * MS]), MAX_DELAY_NS);
        assert_eq!(DelayPolicy::new(DelaySetting::Manual(1.0), 0).on_stream_start(&[]), MIN_DELAY_NS);
        assert_eq!(DelayPolicy::new(DelaySetting::Manual(99_999.0), 0).on_stream_start(&[]), MAX_DELAY_NS);
    }

    #[derive(serde::Deserialize, serde::Serialize, Debug, PartialEq)]
    struct Wrapper {
        delay_ms: DelaySetting,
    }

    #[test]
    fn delay_setting_serde() {
        let parse = |s: &str| toml::from_str::<Wrapper>(s).map(|w| w.delay_ms);
        assert_eq!(parse("delay_ms = \"auto\"").unwrap(), DelaySetting::Auto);
        assert_eq!(parse("delay_ms = \"Auto\"").unwrap(), DelaySetting::Auto);
        assert_eq!(parse("delay_ms = 80").unwrap(), DelaySetting::Manual(80.0));
        assert_eq!(parse("delay_ms = 80.5").unwrap(), DelaySetting::Manual(80.5));
        assert!(parse("delay_ms = \"fast\"").is_err());
        assert_eq!(toml::to_string(&Wrapper { delay_ms: DelaySetting::Auto }).unwrap().trim(), "delay_ms = \"auto\"");
        assert_eq!(toml::to_string(&Wrapper { delay_ms: DelaySetting::Manual(80.0) }).unwrap().trim(), "delay_ms = 80.0");
    }
}
