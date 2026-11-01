use serde_json::{Value, json};

use crate::decision::{Profile, ProfileState, Snapshot, can_step};
use crate::icons::{self, Needle};

const DISABLED_COLOR: &str = "#6b7280";

/// Longest application id the status line shows whole; the strip is 200 px.
const MAX_APP_ID_CHARS: usize = 20;

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

/// Cuts `text` to `max` characters, ending in "…" when it was longer.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max - 1).collect();
    short.push('…');
    short
}

/// The line under the gauge: who holds the profile, else why performance is
/// degraded, else nothing. With several holds it names the one that asked
/// for the active profile (the effective one) and counts the rest.
pub fn status_text(snapshot: &Snapshot) -> String {
    if snapshot.state == ProfileState::Unavailable {
        return String::new();
    }
    let holds = &snapshot.conditions.holds;
    if let Some(first) = holds.first() {
        let active = match snapshot.state {
            ProfileState::Known(profile) => Some(profile.dbus_name()),
            _ => None,
        };
        let shown = holds
            .iter()
            .find(|hold| Some(hold.profile.as_str()) == active)
            .unwrap_or(first);
        let holder = match shown.application_id.as_str() {
            "" => "an app".to_string(),
            id => shorten(id, MAX_APP_ID_CHARS),
        };
        return match holds.len() {
            1 => format!("Held by {holder}"),
            n => format!("Held by {holder} +{}", n - 1),
        };
    }
    match snapshot.conditions.degraded.as_str() {
        "" => String::new(),
        "lap-detected" => "Limited: lap detected".to_string(),
        "high-operating-temperature" => "Limited: too hot".to_string(),
        other => format!(
            "Limited: {}",
            shorten(&other.replace('-', " "), MAX_APP_ID_CHARS)
        ),
    }
}

/// Converts a `Snapshot` into the Encoder's `setFeedback` payload: a flat
/// object keyed by each layout item's `key`
/// (see assets/layouts/power-profile.json).
///
/// The side icons hint at what rotating does - the leaf towards Power Saver,
/// the bolt towards Performance - and dim when rotating that way would do
/// nothing (`decision::can_step`). Holds and degradation only add the status
/// line; they never change the name, gauge or hints.
pub fn feedback_for(snapshot: &Snapshot) -> Value {
    let state = snapshot.state;
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
        "status": status_text(snapshot),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{Conditions, Hold};
    use std::collections::BTreeSet;

    const ALL_STATES: [ProfileState; 5] = [
        ProfileState::Known(Profile::PowerSaver),
        ProfileState::Known(Profile::Balanced),
        ProfileState::Known(Profile::Performance),
        ProfileState::Unknown,
        ProfileState::Unavailable,
    ];

    fn hold(application_id: &str, profile: &str) -> Hold {
        Hold {
            application_id: application_id.into(),
            profile: profile.into(),
            reason: "test".into(),
        }
    }

    fn with(state: ProfileState, holds: Vec<Hold>, degraded: &str) -> Snapshot {
        Snapshot {
            state,
            conditions: Conditions {
                holds,
                degraded: degraded.into(),
            },
        }
    }

    /// Every state, plain and with a hold and a degraded reason.
    fn all_snapshots() -> Vec<Snapshot> {
        ALL_STATES
            .into_iter()
            .flat_map(|state| {
                [
                    Snapshot::plain(state),
                    with(state, vec![hold("gamemode", "performance")], "lap-detected"),
                ]
            })
            .collect()
    }

    fn feedback_for_state(state: ProfileState) -> Value {
        feedback_for(&Snapshot::plain(state))
    }

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
    fn a_plain_state_has_an_empty_status_line() {
        for state in ALL_STATES {
            assert_eq!(feedback_for_state(state)["status"], "", "{state:?}");
        }
    }

    #[test]
    fn a_hold_names_its_holder() {
        let balanced = ProfileState::Known(Profile::Balanced);
        let s = with(balanced, vec![hold("gamemode", "balanced")], "");
        assert_eq!(feedback_for(&s)["status"], "Held by gamemode");
        let s = with(balanced, vec![hold("", "balanced")], "");
        assert_eq!(status_text(&s), "Held by an app");
    }

    #[test]
    fn several_holds_name_the_effective_one_and_count_the_rest() {
        let saver = ProfileState::Known(Profile::PowerSaver);
        let holds = vec![
            hold("game", "performance"),
            hold("battery-widget", "power-saver"),
            hold("other", "performance"),
        ];
        assert_eq!(
            status_text(&with(saver, holds.clone(), "")),
            "Held by battery-widget +2"
        );
        // No hold matches the active profile: the first one is named.
        assert_eq!(
            status_text(&with(ProfileState::Unknown, holds, "")),
            "Held by game +2"
        );
    }

    #[test]
    fn a_long_application_id_is_shortened() {
        let s = with(
            ProfileState::Known(Profile::Performance),
            vec![hold("com.feralinteractive.GameMode", "performance")],
            "",
        );
        assert_eq!(status_text(&s), "Held by com.feralinteractiv…");
    }

    #[test]
    fn degradation_shows_only_without_a_hold() {
        let balanced = ProfileState::Known(Profile::Balanced);
        for (reason, text) in [
            ("lap-detected", "Limited: lap detected"),
            ("high-operating-temperature", "Limited: too hot"),
            ("something-new", "Limited: something new"),
        ] {
            assert_eq!(status_text(&with(balanced, vec![], reason)), text);
        }
        let held = with(balanced, vec![hold("game", "balanced")], "lap-detected");
        assert_eq!(status_text(&held), "Held by game");
    }

    #[test]
    fn conditions_never_change_the_name_gauge_or_hints() {
        for state in ALL_STATES {
            let plain = feedback_for_state(state);
            let held = feedback_for(&with(state, vec![hold("g", "performance")], "lap-detected"));
            for key in ["name", "left", "gauge", "right"] {
                assert_eq!(plain[key], held[key], "{key} for {state:?}");
            }
        }
    }

    #[test]
    fn unavailable_shows_no_status_even_with_stale_conditions() {
        let s = with(
            ProfileState::Unavailable,
            vec![hold("g", "balanced")],
            "lap-detected",
        );
        assert_eq!(status_text(&s), "");
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
        for snapshot in all_snapshots() {
            let feedback = feedback_for(&snapshot);
            let feedback_keys: BTreeSet<&str> = feedback
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(feedback_keys, layout_keys, "feedback for {snapshot:?}");
        }
    }

    /// OpenDeck's strip renderer parses each pixmap with
    /// `roxmltree::Document::parse(svg).unwrap()`, so malformed SVG would
    /// panic the host, not the plugin.
    #[test]
    fn every_pixmap_in_every_state_is_well_formed_svg() {
        for snapshot in all_snapshots() {
            let state = &snapshot;
            let feedback = feedback_for(state);
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
