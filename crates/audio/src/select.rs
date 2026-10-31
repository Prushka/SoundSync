//! Resolving a configured device string to a concrete device.

use crate::{AudioError, DeviceInfo};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DeviceSelector {
    Default,
    /// Discard audio on a timer (headless nodes, tests).
    Null,
    Id(String),
    Name(String),
}

impl DeviceSelector {
    /// `""`/`"default"` → Default, `"null"` → Null, `"id:…"` → Id, anything else → Name.
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("default") {
            DeviceSelector::Default
        } else if s.eq_ignore_ascii_case("null") {
            DeviceSelector::Null
        } else if let Some(id) = s.strip_prefix("id:") {
            DeviceSelector::Id(id.to_string())
        } else {
            DeviceSelector::Name(s.to_string())
        }
    }
}

impl std::fmt::Display for DeviceSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceSelector::Default => f.write_str("default"),
            DeviceSelector::Null => f.write_str("null"),
            DeviceSelector::Id(id) => write!(f, "id:{id}"),
            DeviceSelector::Name(n) => f.write_str(n),
        }
    }
}

/// Order: exact (case-insensitive) name among active devices, exact among all devices, then a
/// unique case-insensitive substring among active devices.
pub fn select_device<'a>(devices: &'a [DeviceInfo], sel: &DeviceSelector) -> Result<&'a DeviceInfo, AudioError> {
    match sel {
        DeviceSelector::Default => devices
            .iter()
            .find(|d| d.active && d.is_default)
            .or_else(|| devices.iter().find(|d| d.active))
            .ok_or_else(|| AudioError::NotFound("default device".into())),
        DeviceSelector::Null => Err(AudioError::NotFound("null is not a hardware device".into())),
        DeviceSelector::Id(id) => devices.iter().find(|d| &d.id == id).ok_or_else(|| AudioError::NotFound(format!("id:{id}"))),
        DeviceSelector::Name(name) => {
            let exact = |d: &&DeviceInfo| d.name.eq_ignore_ascii_case(name);
            if let Some(d) = devices.iter().filter(|d| d.active).find(exact).or_else(|| devices.iter().find(exact)) {
                return Ok(d);
            }
            let needle = name.to_lowercase();
            let matches: Vec<&DeviceInfo> = devices.iter().filter(|d| d.active && d.name.to_lowercase().contains(&needle)).collect();
            match matches.as_slice() {
                [one] => Ok(one),
                [] => Err(AudioError::NotFound(name.clone())),
                many => Err(AudioError::Ambiguous(many.iter().map(|d| d.name.clone()).collect())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceInfo, Direction, StreamFormat};

    fn dev(id: &str, name: &str, active: bool, is_default: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: name.into(),
            direction: Direction::Render,
            is_default,
            active,
            mix_format: StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 3 },
            default_period_ns: 10_000_000,
            min_period_ns: 3_000_000,
        }
    }

    fn this_pc() -> Vec<DeviceInfo> {
        vec![
            dev("{1}", "LG TV SSCR2 (NVIDIA High Definition Audio)", false, false),
            dev("{2}", "LG TV SSCR2 (NVIDIA High Definition Audio)", true, true),
            dev("{3}", "Speakers (Steam Streaming Speakers)", true, false),
            dev("{4}", "HDMI (Live Gamer Ultra-Audio)", true, false),
            dev("{5}", "Digital Output (Realtek USB2.0 Audio)", true, false),
            dev("{6}", "HDMI 2 (Other)", true, false),
        ]
    }

    #[test]
    fn select_prefers_active_exact_match() {
        let devs = this_pc();
        let sel = DeviceSelector::Name("lg tv sscr2 (nvidia high definition audio)".into());
        assert_eq!(select_device(&devs, &sel).unwrap().id, "{2}");
    }

    #[test]
    fn select_exact_inactive_when_no_active_match() {
        let devs = vec![dev("{1}", "Old TV", false, false), dev("{2}", "Speakers", true, true)];
        assert_eq!(select_device(&devs, &DeviceSelector::Name("Old TV".into())).unwrap().id, "{1}");
    }

    #[test]
    fn select_substring_unique() {
        let devs = this_pc();
        assert_eq!(select_device(&devs, &DeviceSelector::Name("steam".into())).unwrap().name, "Speakers (Steam Streaming Speakers)");
    }

    #[test]
    fn select_ambiguous_errors() {
        let devs = this_pc();
        match select_device(&devs, &DeviceSelector::Name("HDMI".into())) {
            Err(AudioError::Ambiguous(names)) => {
                assert_eq!(names, vec!["HDMI (Live Gamer Ultra-Audio)".to_string(), "HDMI 2 (Other)".to_string()]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        assert!(matches!(select_device(&devs, &DeviceSelector::Name("nothing".into())), Err(AudioError::NotFound(_))));
    }

    #[test]
    fn select_default_and_parse() {
        let devs = this_pc();
        assert_eq!(select_device(&devs, &DeviceSelector::Default).unwrap().id, "{2}");
        assert_eq!(select_device(&devs, &DeviceSelector::Id("{5}".into())).unwrap().id, "{5}");
        assert_eq!(DeviceSelector::parse(""), DeviceSelector::Default);
        assert_eq!(DeviceSelector::parse("Default"), DeviceSelector::Default);
        assert_eq!(DeviceSelector::parse("null"), DeviceSelector::Null);
        assert_eq!(DeviceSelector::parse("id:{5}"), DeviceSelector::Id("{5}".into()));
        assert_eq!(DeviceSelector::parse(" CABLE Input "), DeviceSelector::Name("CABLE Input".into()));
        // No default flagged: first active device.
        let devs = vec![dev("{1}", "A", false, false), dev("{2}", "B", true, false)];
        assert_eq!(select_device(&devs, &DeviceSelector::Default).unwrap().id, "{2}");
    }
}
