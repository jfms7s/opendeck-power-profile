//! Inline SVG line icons for the touch strip, in the style of Elgato's own
//! dial layouts: white 2px round-cap strokes on a 24x24 grid. OpenDeck's
//! strip renderer (resvg) accepts an SVG string directly as a pixmap value.

pub(crate) const WHITE: &str = "#ffffff";

/// Stroke opacity for a side icon whose direction is blocked (the dial is
/// already clamped at that end).
pub(crate) const DIMMED_OPACITY: f64 = 0.3;

/// The gauge needle pivots on this point and is this long.
const NEEDLE_HUB: (f64, f64) = (12.0, 16.0);
const NEEDLE_LENGTH: f64 = 7.0;

/// A gauge needle: its angle in degrees from straight up (negative = left),
/// and its colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Needle {
    pub angle_degrees: f64,
    pub color: &'static str,
}

fn svg(opacity: f64, body: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="{WHITE}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" stroke-opacity="{opacity}">{body}</svg>"#
    )
}

fn opacity_for(enabled: bool) -> f64 {
    if enabled { 1.0 } else { DIMMED_OPACITY }
}

/// Leaf - the "rotate left" hint towards Power Saver.
pub fn leaf(enabled: bool) -> String {
    svg(
        opacity_for(enabled),
        r#"<path d="M5 19C5 10 10 5 20 4C20 14 15 19 5 19Z"/><path d="M5 19L13 11"/>"#,
    )
}

/// Lightning bolt - the "rotate right" hint towards Performance.
pub fn bolt(enabled: bool) -> String {
    svg(
        opacity_for(enabled),
        r#"<path d="M13 2L4 14H11L10 22L20 10H13Z"/>"#,
    )
}

/// Speedometer arc in `dial_color`, with `needle` drawn over it. `None`
/// draws an empty dial (no needle) for unknown/unavailable.
pub fn gauge(needle: Option<Needle>, dial_color: &str) -> String {
    let dial = format!(r#"<path d="M2.6 19.42A10 10 0 1 1 21.4 19.42" stroke="{dial_color}"/>"#);
    let needle = match needle {
        Some(Needle {
            angle_degrees,
            color,
        }) => {
            let (hub_x, hub_y) = NEEDLE_HUB;
            let angle = angle_degrees.to_radians();
            let x = hub_x + NEEDLE_LENGTH * angle.sin();
            let y = hub_y - NEEDLE_LENGTH * angle.cos();
            format!(
                r#"<path d="M{hub_x} {hub_y}L{x:.2} {y:.2}" stroke="{color}"/><circle cx="{hub_x}" cy="{hub_y}" r="1.5" fill="{color}" stroke="{color}"/>"#
            )
        }
        None => String::new(),
    };
    svg(1.0, &format!("{dial}{needle}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_icons_dim_only_when_disabled() {
        let dimmed = format!(r#"stroke-opacity="{DIMMED_OPACITY}""#);
        assert!(leaf(true).contains(r#"stroke-opacity="1""#));
        assert!(leaf(false).contains(&dimmed));
        assert!(bolt(true).contains(r#"stroke-opacity="1""#));
        assert!(bolt(false).contains(&dimmed));
    }

    #[test]
    fn gauge_draws_a_needle_at_the_given_angle_and_color() {
        let right = gauge(
            Some(Needle {
                angle_degrees: 60.0,
                color: "#ef4444",
            }),
            WHITE,
        );
        assert!(right.contains("M12 16L18.06 12.50"), "{right}");
        assert!(right.contains(r##"stroke="#ef4444""##));

        let up = gauge(
            Some(Needle {
                angle_degrees: 0.0,
                color: "#eab308",
            }),
            WHITE,
        );
        assert!(up.contains("M12 16L12.00 9.00"), "{up}");

        let left = gauge(
            Some(Needle {
                angle_degrees: -60.0,
                color: "#22c55e",
            }),
            WHITE,
        );
        assert!(left.contains("M12 16L5.94 12.50"), "{left}");
    }

    #[test]
    fn gauge_without_a_needle_draws_only_the_dial() {
        let empty = gauge(None, "#6b7280");
        assert!(!empty.contains("circle"));
        assert!(empty.contains(r##"stroke="#6b7280""##));
    }
}
