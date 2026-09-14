use serde_json::{Value, json};

use crate::decision::{PROFILE_ORDER, ProfileState};

pub const DISABLED_COLOR: &str = "#6b7280";

const SAVER_COLOR: &str = "#22c55e";
const BALANCED_COLOR: &str = "#eab308";
const PERFORMANCE_COLOR: &str = "#ef4444";

/// Human-readable name for a known profile index (e.g. "power-saver" -> "Power Saver").
fn display_name(idx: usize) -> &'static str {
    match PROFILE_ORDER[idx] {
        "power-saver" => "Power Saver",
        "balanced" => "Balanced",
        "performance" => "Performance",
        other => unreachable!("PROFILE_ORDER has no display name for {other}"),
    }
}

fn color_for_index(idx: usize) -> &'static str {
    match idx {
        0 => SAVER_COLOR,
        1 => BALANCED_COLOR,
        _ => PERFORMANCE_COLOR,
    }
}

/// 0/50/100 - one of three fixed positions on the touch-strip bar's 0-100
/// scale, since there are only three profiles, not a continuous reading.
fn bar_value_for_index(idx: usize) -> f64 {
    idx as f64 * 50.0
}

/// Converts a `ProfileState` into the Encoder's `setFeedback` payload: a flat
/// object keyed by each layout item's `key`
/// (see assets/layouts/power-profile.json).
pub fn feedback_for_state(state: ProfileState) -> Value {
    let (name, color, bar_value) = match state {
        ProfileState::Known(idx) => {
            (display_name(idx), color_for_index(idx), bar_value_for_index(idx))
        }
        ProfileState::Unknown => ("Unknown", DISABLED_COLOR, 0.0),
        ProfileState::Unavailable => ("Unavailable", DISABLED_COLOR, 0.0),
    };
    json!({
        "bar": { "value": bar_value, "bar_fill_c": color },
        "name": name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_profiles_render_their_display_name_and_color() {
        let f = feedback_for_state(ProfileState::Known(0));
        assert_eq!(f["name"], "Power Saver");
        assert_eq!(f["bar"]["bar_fill_c"], "#22c55e");
        assert_eq!(f["bar"]["value"], 0.0);

        let f = feedback_for_state(ProfileState::Known(1));
        assert_eq!(f["name"], "Balanced");
        assert_eq!(f["bar"]["bar_fill_c"], "#eab308");
        assert_eq!(f["bar"]["value"], 50.0);

        let f = feedback_for_state(ProfileState::Known(2));
        assert_eq!(f["name"], "Performance");
        assert_eq!(f["bar"]["bar_fill_c"], "#ef4444");
        assert_eq!(f["bar"]["value"], 100.0);
    }

    #[test]
    fn unknown_state_renders_a_clear_unknown_display() {
        let f = feedback_for_state(ProfileState::Unknown);
        assert_eq!(f["name"], "Unknown");
        assert_eq!(f["bar"]["bar_fill_c"], DISABLED_COLOR);
    }

    #[test]
    fn unavailable_state_renders_a_clear_unavailable_display() {
        let f = feedback_for_state(ProfileState::Unavailable);
        assert_eq!(f["name"], "Unavailable");
        assert_eq!(f["bar"]["bar_fill_c"], DISABLED_COLOR);
    }

    #[test]
    fn feedback_keys_match_the_shipped_layout() {
        let layout: Value =
            serde_json::from_str(include_str!("../assets/layouts/power-profile.json")).unwrap();
        let keys: Vec<&str> = layout["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["key"].as_str().unwrap())
            .collect();
        let feedback = feedback_for_state(ProfileState::Unavailable);
        for k in feedback.as_object().unwrap().keys() {
            assert!(keys.contains(&k.as_str()), "layout has no item keyed {k}");
        }
    }
}
