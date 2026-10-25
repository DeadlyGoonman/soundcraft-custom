//! The audio device's sample rate against the session's: what the host reports about its device,
//! the device rate changes commands ask the host for, and whether to offer switching the device
//! to a 44.1 kHz session's rate (playing at the session rate skips the resampler).
//!
//! The engine does not own the device: the host reports [`DeviceStatus`] each frame and drains
//! [`Engine::device_requests`](crate::Engine) to reopen the device.

/// The rate the switch prompt is about: a session at 44.1 kHz ...
pub const SESSION_RATE: u32 = 44_100;
/// ... while the device runs at 48 kHz.
pub const DEVICE_RATE: u32 = 48_000;

/// The open output device, as the host reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct DeviceStatus {
    /// The rate the device runs at; `None` with no device (silent clock).
    pub rate: Option<u32>,
    /// The rates the device accepts; `None` until the host has asked it.
    pub rates: Option<Vec<u32>>,
}

/// A device change for the host to apply (reopening the device), drained each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum DeviceRequest {
    /// Reopen the output device at this rate.
    SetRate(u32),
}

/// The user's answer to the prompt for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RateChoice {
    /// Switch the device to the session rate.
    Switch,
    /// Keep the device rate (the mix is resampled).
    Keep,
}

/// Whether to ask, kept with the preferences ("always" in the prompt).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RatePolicy {
    #[default]
    Ask,
    AlwaysSwitch,
    AlwaysKeep,
}

/// What to do about the device rate for the open session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// Nothing to ask: the rates match, the case is not 44.1 on 48 kHz, or it was answered.
    Nothing,
    /// Not yet known whether the device accepts the session rate.
    Wait,
    /// Ask. `can_switch`: the device accepts `session`; when false, the prompt says so and
    /// offers only to keep `device`.
    Ask { session: u32, device: u32, can_switch: bool },
    /// Switch without asking ("always switch").
    Switch(u32),
}

/// Decide for a session at `session_rate` on `device`, given this session's earlier `answer`
/// and the saved `policy`.
pub fn decide(session_rate: u32, device: &DeviceStatus, answer: Option<RateChoice>, policy: RatePolicy) -> RateDecision {
    if session_rate != SESSION_RATE || device.rate != Some(DEVICE_RATE) || answer.is_some() || policy == RatePolicy::AlwaysKeep {
        return RateDecision::Nothing;
    }
    let Some(rates) = &device.rates else { return RateDecision::Wait };
    let can_switch = rates.contains(&session_rate);
    if can_switch && policy == RatePolicy::AlwaysSwitch {
        return RateDecision::Switch(session_rate);
    }
    RateDecision::Ask { session: session_rate, device: DEVICE_RATE, can_switch }
}

/// Why the device cannot run at `rate`, if it cannot (or might not: rates not known yet).
pub fn check_rate(device: &DeviceStatus, rate: u32) -> std::result::Result<(), String> {
    if device.rate.is_none() {
        return Err("there is no audio device".into());
    }
    match &device.rates {
        None => Err("the audio device has not reported its sample rates yet".into()),
        Some(rates) if !rates.contains(&rate) => Err(format!("the audio device does not support {rate} Hz")),
        Some(_) => Ok(()),
    }
}

/// The device rate to keep in the audio settings after switching the device to `target`: none
/// chosen stays none (the device follows the session) when that already gives `target`.
pub fn config_rate(chosen: Option<u32>, session_rate: u32, target: u32) -> Option<u32> {
    if chosen.is_none() && session_rate == target { None } else { Some(target) }
}

/// A rate in kHz as the prompt shows it ("44.1", "48").
pub fn khz(hz: u32) -> String {
    format!("{}", f64::from(hz) / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(rate: u32, rates: Option<&[u32]>) -> DeviceStatus {
        DeviceStatus { rate: Some(rate), rates: rates.map(<[u32]>::to_vec) }
    }

    #[test]
    fn asks_for_a_44k_session_on_a_48k_device() {
        let d = dev(48_000, Some(&[44_100, 48_000]));
        assert_eq!(decide(44_100, &d, None, RatePolicy::Ask), RateDecision::Ask { session: 44_100, device: 48_000, can_switch: true });
    }

    #[test]
    fn says_when_the_device_cannot_switch() {
        let d = dev(48_000, Some(&[48_000, 96_000]));
        assert_eq!(decide(44_100, &d, None, RatePolicy::Ask), RateDecision::Ask { session: 44_100, device: 48_000, can_switch: false });
        // "Always switch" cannot switch either: the user is told instead.
        assert_eq!(decide(44_100, &d, None, RatePolicy::AlwaysSwitch), RateDecision::Ask { session: 44_100, device: 48_000, can_switch: false });
    }

    #[test]
    fn nothing_when_rates_match_or_other_rates() {
        let both = Some(&[44_100, 48_000][..]);
        assert_eq!(decide(48_000, &dev(48_000, both), None, RatePolicy::Ask), RateDecision::Nothing);
        assert_eq!(decide(44_100, &dev(44_100, both), None, RatePolicy::Ask), RateDecision::Nothing);
        assert_eq!(decide(96_000, &dev(48_000, both), None, RatePolicy::Ask), RateDecision::Nothing);
        assert_eq!(decide(44_100, &dev(96_000, both), None, RatePolicy::Ask), RateDecision::Nothing);
        assert_eq!(decide(44_100, &DeviceStatus::default(), None, RatePolicy::Ask), RateDecision::Nothing, "no device");
    }

    #[test]
    fn an_answer_for_this_session_is_not_asked_again() {
        let d = dev(48_000, Some(&[44_100, 48_000]));
        assert_eq!(decide(44_100, &d, Some(RateChoice::Keep), RatePolicy::Ask), RateDecision::Nothing);
        assert_eq!(decide(44_100, &d, Some(RateChoice::Switch), RatePolicy::Ask), RateDecision::Nothing, "a switch that did not take");
    }

    #[test]
    fn policies() {
        let d = dev(48_000, Some(&[44_100, 48_000]));
        assert_eq!(decide(44_100, &d, None, RatePolicy::AlwaysSwitch), RateDecision::Switch(44_100));
        assert_eq!(decide(44_100, &d, None, RatePolicy::AlwaysKeep), RateDecision::Nothing);
    }

    #[test]
    fn waits_for_the_device_rates() {
        assert_eq!(decide(44_100, &dev(48_000, None), None, RatePolicy::Ask), RateDecision::Wait);
    }

    #[test]
    fn switching_keeps_following_the_session() {
        assert_eq!(config_rate(None, 44_100, 44_100), None);
        assert_eq!(config_rate(Some(48_000), 44_100, 44_100), Some(44_100));
        assert_eq!(config_rate(None, 48_000, 44_100), Some(44_100));
        assert_eq!((khz(44_100), khz(48_000)), ("44.1".to_string(), "48".to_string()));
    }

    #[test]
    fn check_rate_reasons() {
        assert!(check_rate(&dev(48_000, Some(&[44_100, 48_000])), 44_100).is_ok());
        assert!(check_rate(&dev(48_000, Some(&[48_000])), 44_100).unwrap_err().contains("does not support 44100 Hz"));
        assert!(check_rate(&dev(48_000, None), 44_100).is_err());
        assert!(check_rate(&DeviceStatus::default(), 44_100).is_err());
    }
}
