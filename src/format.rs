use serde_json::{Value, json};

use crate::decision::{PROFILE_ORDER, ProfileState};
use crate::icons;

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

/// Converts a `ProfileState` into the Encoder's `setFeedback` payload: a flat
/// object keyed by each layout item's `key`
/// (see assets/layouts/power-profile.json).
///
/// The side icons hint at what rotating does, and dim when the dial is
/// clamped at that end (or when there's no daemon to talk to at all).
pub fn feedback_for_state(state: ProfileState) -> Value {
    let last = PROFILE_ORDER.len() - 1;
    let (name, gauge, can_go_left, can_go_right) = match state {
        ProfileState::Known(idx) => (
            display_name(idx),
            icons::gauge(Some((idx, color_for_index(idx))), "#ffffff"),
            idx > 0,
            idx < last,
        ),
        // Rotating from Unknown still works (it resets to Balanced).
        ProfileState::Unknown => ("Unknown", icons::gauge(None, DISABLED_COLOR), true, true),
        ProfileState::Unavailable => (
            "Unavailable",
            icons::gauge(None, DISABLED_COLOR),
            false,
            false,
        ),
    };
    json!({
        "name": name,
        "left": icons::leaf(can_go_left),
        "gauge": gauge,
        "right": icons::bolt(can_go_right),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_dimmed(svg: &Value) -> bool {
        svg.as_str().unwrap().contains(r#"stroke-opacity="0.3""#)
    }

    #[test]
    fn known_profiles_render_their_display_name_and_colored_needle() {
        let f = feedback_for_state(ProfileState::Known(0));
        assert_eq!(f["name"], "Power Saver");
        assert!(f["gauge"].as_str().unwrap().contains("#22c55e"));

        let f = feedback_for_state(ProfileState::Known(1));
        assert_eq!(f["name"], "Balanced");
        assert!(f["gauge"].as_str().unwrap().contains("#eab308"));

        let f = feedback_for_state(ProfileState::Known(2));
        assert_eq!(f["name"], "Performance");
        assert!(f["gauge"].as_str().unwrap().contains("#ef4444"));
    }

    #[test]
    fn side_icons_dim_at_the_clamped_ends() {
        let saver = feedback_for_state(ProfileState::Known(0));
        assert!(is_dimmed(&saver["left"]) && !is_dimmed(&saver["right"]));

        let balanced = feedback_for_state(ProfileState::Known(1));
        assert!(!is_dimmed(&balanced["left"]) && !is_dimmed(&balanced["right"]));

        let performance = feedback_for_state(ProfileState::Known(2));
        assert!(!is_dimmed(&performance["left"]) && is_dimmed(&performance["right"]));
    }

    #[test]
    fn unknown_state_renders_a_needleless_gauge_but_stays_rotatable() {
        let f = feedback_for_state(ProfileState::Unknown);
        assert_eq!(f["name"], "Unknown");
        assert!(f["gauge"].as_str().unwrap().contains(DISABLED_COLOR));
        assert!(!is_dimmed(&f["left"]) && !is_dimmed(&f["right"]));
    }

    #[test]
    fn unavailable_state_renders_a_needleless_gauge_and_dims_both_sides() {
        let f = feedback_for_state(ProfileState::Unavailable);
        assert_eq!(f["name"], "Unavailable");
        assert!(f["gauge"].as_str().unwrap().contains(DISABLED_COLOR));
        assert!(is_dimmed(&f["left"]) && is_dimmed(&f["right"]));
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
