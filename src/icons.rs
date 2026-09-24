//! Inline SVG line icons for the touch strip, in the style of Elgato's own
//! dial layouts: white 2px round-cap strokes on a 24x24 grid. OpenDeck's
//! strip renderer (resvg) accepts an SVG string directly as a pixmap value.

const WHITE: &str = "#ffffff";

/// Stroke opacity for a side icon whose direction is blocked (the dial is
/// already clamped at that end).
const DIMMED_OPACITY: f64 = 0.3;

/// Needle tip for each profile index: pointing up-left, straight up and
/// up-right from the gauge hub at (12, 16), 7 units long.
const NEEDLE_TIPS: [(f64, f64); 3] = [(5.94, 12.5), (12.0, 9.0), (18.06, 12.5)];

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

/// Speedometer whose needle points at the active profile, drawn in
/// `color`. `None` draws an empty dial (no needle) for unknown/unavailable.
pub fn gauge(needle: Option<(usize, &str)>, dial_color: &str) -> String {
    let dial = format!(r#"<path d="M2.6 19.42A10 10 0 1 1 21.4 19.42" stroke="{dial_color}"/>"#);
    let needle = match needle {
        Some((idx, color)) => {
            let (x, y) = NEEDLE_TIPS[idx];
            format!(
                r#"<path d="M12 16L{x} {y}" stroke="{color}"/><circle cx="12" cy="16" r="1.5" fill="{color}" stroke="{color}"/>"#
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
        assert!(leaf(true).contains(r#"stroke-opacity="1""#));
        assert!(leaf(false).contains(r#"stroke-opacity="0.3""#));
        assert!(bolt(true).contains(r#"stroke-opacity="1""#));
        assert!(bolt(false).contains(r#"stroke-opacity="0.3""#));
    }

    #[test]
    fn gauge_draws_a_needle_in_the_given_color_only_when_known() {
        let g = gauge(Some((2, "#ef4444")), WHITE);
        assert!(g.contains("M12 16L18.06 12.5"));
        assert!(g.contains(r##"stroke="#ef4444""##));

        let empty = gauge(None, "#6b7280");
        assert!(!empty.contains("circle"));
        assert!(empty.contains(r##"stroke="#6b7280""##));
    }
}
