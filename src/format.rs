use serde_json::{Value, json};

use crate::decision::{Profile, ProfileState, can_step};
use crate::icons::{self, Needle};

const DISABLED_COLOR: &str = "#6b7280";

/// How one profile looks on the touch strip.
struct ProfileStyle {
    name: &'static str,
    color: &'static str,
    /// Needle angle in degrees from straight up (negative = left).
    needle_angle: f64,
}

/// The single presentation table for the profiles: name, the low-to-high
/// green/yellow/red colour convention, and where the gauge needle points.
fn style(profile: Profile) -> ProfileStyle {
    match profile {
        Profile::PowerSaver => ProfileStyle {
            name: "Power Saver",
            color: "#22c55e",
            needle_angle: -60.0,
        },
        Profile::Balanced => ProfileStyle {
            name: "Balanced",
            color: "#eab308",
            needle_angle: 0.0,
        },
        Profile::Performance => ProfileStyle {
            name: "Performance",
            color: "#ef4444",
            needle_angle: 60.0,
        },
    }
}

/// Converts a `ProfileState` into the Encoder's `setFeedback` payload: a flat
/// object keyed by each layout item's `key`
/// (see assets/layouts/power-profile.json).
///
/// The side icons hint at what rotating does - the leaf towards Power Saver,
/// the bolt towards Performance - and dim when rotating that way would do
/// nothing (`decision::can_step`).
pub fn feedback_for_state(state: ProfileState) -> Value {
    let (name, gauge) = match state {
        ProfileState::Known(profile) => {
            let style = style(profile);
            let needle = Needle {
                angle_degrees: style.needle_angle,
                color: style.color,
            };
            (style.name, icons::gauge(Some(needle), icons::WHITE))
        }
        ProfileState::Unknown => ("Unknown", icons::gauge(None, DISABLED_COLOR)),
        ProfileState::Unavailable => ("Unavailable", icons::gauge(None, DISABLED_COLOR)),
    };
    json!({
        "name": name,
        "left": icons::leaf(can_step(state, -1)),
        "gauge": gauge,
        "right": icons::bolt(can_step(state, 1)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const ALL_STATES: [ProfileState; 5] = [
        ProfileState::Known(Profile::PowerSaver),
        ProfileState::Known(Profile::Balanced),
        ProfileState::Known(Profile::Performance),
        ProfileState::Unknown,
        ProfileState::Unavailable,
    ];

    fn is_dimmed(svg: &Value) -> bool {
        let dimmed = format!(r#"stroke-opacity="{}""#, icons::DIMMED_OPACITY);
        svg.as_str().unwrap().contains(&dimmed)
    }

    #[test]
    fn known_profiles_render_their_display_name_and_colored_needle() {
        for (profile, name, color) in [
            (Profile::PowerSaver, "Power Saver", "#22c55e"),
            (Profile::Balanced, "Balanced", "#eab308"),
            (Profile::Performance, "Performance", "#ef4444"),
        ] {
            let f = feedback_for_state(ProfileState::Known(profile));
            assert_eq!(f["name"], name);
            assert!(f["gauge"].as_str().unwrap().contains(color), "{profile:?}");
        }
    }

    #[test]
    fn side_icons_dim_at_the_clamped_ends() {
        let saver = feedback_for_state(ProfileState::Known(Profile::PowerSaver));
        assert!(is_dimmed(&saver["left"]) && !is_dimmed(&saver["right"]));

        let balanced = feedback_for_state(ProfileState::Known(Profile::Balanced));
        assert!(!is_dimmed(&balanced["left"]) && !is_dimmed(&balanced["right"]));

        let performance = feedback_for_state(ProfileState::Known(Profile::Performance));
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
    fn feedback_keys_match_the_shipped_layout_in_every_state() {
        let layout: Value =
            serde_json::from_str(include_str!("../assets/layouts/power-profile.json")).unwrap();
        let layout_keys: BTreeSet<&str> = layout["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["key"].as_str().unwrap())
            .collect();
        for state in ALL_STATES {
            let feedback = feedback_for_state(state);
            let feedback_keys: BTreeSet<&str> = feedback
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(feedback_keys, layout_keys, "feedback for {state:?}");
        }
    }

    /// OpenDeck's strip renderer parses each pixmap with
    /// `roxmltree::Document::parse(svg).unwrap()`, so malformed SVG would
    /// panic the host, not the plugin.
    #[test]
    fn every_pixmap_in_every_state_is_well_formed_svg() {
        for state in ALL_STATES {
            let feedback = feedback_for_state(state);
            for key in ["left", "gauge", "right"] {
                let svg = feedback[key].as_str().unwrap();
                let doc = roxmltree::Document::parse(svg)
                    .unwrap_or_else(|e| panic!("{key} for {state:?} is not XML: {e}\n{svg}"));
                let root = doc.root_element();
                assert_eq!(root.tag_name().name(), "svg", "{key} for {state:?}");
                assert!(root.attribute("viewBox").is_some(), "{key} for {state:?}");
            }
        }
    }
}
