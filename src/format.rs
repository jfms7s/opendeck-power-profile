use serde_json::{Value, json};

use crate::decision::{Profile, ProfileState, Snapshot, can_step};
use crate::icons::{self, Glyph, Needle};

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
/// degraded, else the CPU temperature (`telemetry`), else nothing. With
/// several holds it names the one that asked for the active profile (the
/// effective one) and counts the rest. Empty while the provider is
/// unavailable: a dimmed dial shows nothing live.
pub fn status_text(snapshot: &Snapshot, telemetry: Option<&str>) -> String {
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
        "" => telemetry.unwrap_or_default().to_string(),
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
/// nothing (`decision::can_step`), including towards a profile the provider
/// doesn't offer. Holds, degradation and the temperature only fill the
/// status line; they never change the name, gauge or hints.
pub fn feedback_for(snapshot: &Snapshot, telemetry: Option<&str>) -> Value {
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
        "left": icons::leaf(can_step(state, -1, snapshot.available)),
        "gauge": gauge,
        "right": icons::bolt(can_step(state, 1, snapshot.available)),
        "status": status_text(snapshot, telemetry),
    })
}

/// The `setFeedback` payload for a new temperature reading: the status line
/// alone (`setFeedback` merges keys, so the rest of the strip stays).
pub fn status_feedback(snapshot: &Snapshot, telemetry: Option<&str>) -> Value {
    json!({ "status": status_text(snapshot, telemetry) })
}

/// A key's title: the profile's name, "Unknown" or "Unavailable".
pub fn title_for(snapshot: &Snapshot) -> &'static str {
    match snapshot.state {
        ProfileState::Known(profile) => style(profile).name,
        ProfileState::Unknown => "Unknown",
        ProfileState::Unavailable => "Unavailable",
    }
}

/// A key's image as a `data:image/svg+xml;base64,` URI: the profile's glyph
/// (leaf, gauge, bolt) in its colour; an empty grey gauge when the profile
/// is unknown or unavailable.
pub fn tile_image_for(snapshot: &Snapshot) -> String {
    let svg = match snapshot.state {
        ProfileState::Known(profile) => {
            let style = style(profile);
            let glyph = match profile {
                Profile::PowerSaver => Glyph::Leaf,
                Profile::Balanced => Glyph::Gauge(Some(Needle {
                    angle_degrees: style.needle_angle,
                    color: style.color,
                })),
                Profile::Performance => Glyph::Bolt,
            };
            icons::key_tile(glyph, style.color)
        }
        ProfileState::Unknown | ProfileState::Unavailable => {
            icons::key_tile(Glyph::Gauge(None), DISABLED_COLOR)
        }
    };
    format!("data:image/svg+xml;base64,{}", base64(svg.as_bytes()))
}

/// Standard base64 with padding (RFC 4648), enough for a data URI.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{Available, Conditions, Hold};
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
            available: Available::ALL,
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
        feedback_for(&Snapshot::plain(state), None)
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
    fn the_bolt_dims_when_the_provider_offers_no_performance() {
        let snapshot = Snapshot {
            available: Available::from_names(["power-saver", "balanced"]).unwrap(),
            ..Snapshot::plain(ProfileState::Known(Profile::Balanced))
        };
        let f = feedback_for(&snapshot, None);
        assert!(!is_dimmed(&f["left"]) && is_dimmed(&f["right"]));
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
        assert_eq!(feedback_for(&s, None)["status"], "Held by gamemode");
        let s = with(balanced, vec![hold("", "balanced")], "");
        assert_eq!(status_text(&s, None), "Held by an app");
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
            status_text(&with(saver, holds.clone(), ""), None),
            "Held by battery-widget +2"
        );
        // No hold matches the active profile: the first one is named.
        assert_eq!(
            status_text(&with(ProfileState::Unknown, holds, ""), None),
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
        assert_eq!(status_text(&s, None), "Held by com.feralinteractiv…");
    }

    #[test]
    fn degradation_shows_only_without_a_hold() {
        let balanced = ProfileState::Known(Profile::Balanced);
        for (reason, text) in [
            ("lap-detected", "Limited: lap detected"),
            ("high-operating-temperature", "Limited: too hot"),
            ("something-new", "Limited: something new"),
        ] {
            assert_eq!(status_text(&with(balanced, vec![], reason), None), text);
        }
        let held = with(balanced, vec![hold("game", "balanced")], "lap-detected");
        assert_eq!(status_text(&held, None), "Held by game");
    }

    #[test]
    fn conditions_never_change_the_name_gauge_or_hints() {
        for state in ALL_STATES {
            let plain = feedback_for_state(state);
            let held = feedback_for(
                &with(state, vec![hold("g", "performance")], "lap-detected"),
                Some("42 °C"),
            );
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
        assert_eq!(status_text(&s, None), "");
    }

    #[test]
    fn the_temperature_shows_only_when_nothing_else_claims_the_line() {
        let balanced = ProfileState::Known(Profile::Balanced);
        let temp = Some("42 °C");
        for state in [balanced, ProfileState::Unknown] {
            assert_eq!(status_text(&Snapshot::plain(state), temp), "42 °C");
        }
        let held = with(balanced, vec![hold("game", "balanced")], "");
        assert_eq!(status_text(&held, temp), "Held by game");
        let limited = with(balanced, vec![], "lap-detected");
        assert_eq!(status_text(&limited, temp), "Limited: lap detected");
        let unavailable = Snapshot::plain(ProfileState::Unavailable);
        assert_eq!(status_text(&unavailable, temp), "");
    }

    /// A temperature tick sends the status line alone, the same text a full
    /// render would put there.
    #[test]
    fn a_status_update_carries_only_the_status_line() {
        for snapshot in all_snapshots() {
            for telemetry in [None, Some("42 °C")] {
                let update = status_feedback(&snapshot, telemetry);
                let keys: Vec<&String> = update.as_object().unwrap().keys().collect();
                assert_eq!(keys, ["status"], "{snapshot:?}");
                assert_eq!(
                    update["status"],
                    feedback_for(&snapshot, telemetry)["status"],
                    "{snapshot:?} {telemetry:?}"
                );
            }
        }
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
        for (snapshot, telemetry) in all_snapshots()
            .into_iter()
            .flat_map(|snapshot| [(snapshot.clone(), None), (snapshot, Some("42 °C"))])
        {
            let feedback = feedback_for(&snapshot, telemetry);
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
            let feedback = feedback_for(state, Some("42 °C"));
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

    /// Decodes standard padded base64; the test's own check on `base64`.
    fn decode_base64(text: &str) -> Vec<u8> {
        const ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut bits = 0u32;
        let mut count = 0;
        let mut out = Vec::new();
        for c in text.chars().filter(|c| *c != '=') {
            bits = bits << 6 | ALPHABET.find(c).expect("base64 character") as u32;
            count += 6;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
            }
        }
        out
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
            assert_eq!(decode_base64(encoded), plain.as_bytes(), "{encoded:?}");
        }
    }

    #[test]
    fn every_key_tile_is_a_base64_svg_in_the_profile_colour() {
        for snapshot in all_snapshots() {
            let uri = tile_image_for(&snapshot);
            let encoded = uri
                .strip_prefix("data:image/svg+xml;base64,")
                .unwrap_or_else(|| panic!("not an SVG data URI: {uri}"));
            let svg = String::from_utf8(decode_base64(encoded)).unwrap();
            let doc = roxmltree::Document::parse(&svg)
                .unwrap_or_else(|e| panic!("tile for {snapshot:?} is not XML: {e}\n{svg}"));
            assert_eq!(doc.root_element().tag_name().name(), "svg");
            let color = match snapshot.state {
                ProfileState::Known(profile) => style(profile).color,
                _ => DISABLED_COLOR,
            };
            assert!(svg.contains(&format!(r#"stroke="{color}""#)), "{svg}");
        }
    }

    #[test]
    fn key_titles_name_the_state() {
        let titles: Vec<&str> = ALL_STATES
            .iter()
            .map(|state| title_for(&Snapshot::plain(*state)))
            .collect();
        assert_eq!(
            titles,
            [
                "Power Saver",
                "Balanced",
                "Performance",
                "Unknown",
                "Unavailable"
            ]
        );
    }
}
