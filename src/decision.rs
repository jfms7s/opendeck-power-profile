//! Pure profile-stepping logic - no D-Bus, no async, fully unit-testable.

pub const PROFILE_ORDER: [&str; 3] = ["power-saver", "balanced", "performance"];
pub const RESET_PROFILE: &str = "balanced";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileState {
    Known(usize),
    Unknown,
    Unavailable,
}

/// Index of `name` within `PROFILE_ORDER`, or `None` if it isn't one of the
/// three profiles power-profiles-daemon actually exposes.
pub fn profile_index(name: &str) -> Option<usize> {
    PROFILE_ORDER.iter().position(|p| *p == name)
}

/// Classifies a freshly-read `ActiveProfile` string into a `ProfileState`.
pub fn state_for_profile(name: &str) -> ProfileState {
    match profile_index(name) {
        Some(idx) => ProfileState::Known(idx),
        None => ProfileState::Unknown,
    }
}

/// The profile to request after a dial rotation of `ticks` (sign only
/// matters - one step per event, clamped at both ends). From `Unknown` or
/// `Unavailable`, falls back to `RESET_PROFILE` rather than guessing a
/// position.
pub fn step_target(state: ProfileState, ticks: i16) -> &'static str {
    match state {
        ProfileState::Known(idx) => {
            let delta = ticks.signum() as isize;
            let last = PROFILE_ORDER.len() as isize - 1;
            let new_idx = (idx as isize + delta).clamp(0, last) as usize;
            PROFILE_ORDER[new_idx]
        }
        ProfileState::Unknown | ProfileState::Unavailable => RESET_PROFILE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_index_finds_each_known_profile() {
        assert_eq!(profile_index("power-saver"), Some(0));
        assert_eq!(profile_index("balanced"), Some(1));
        assert_eq!(profile_index("performance"), Some(2));
    }

    #[test]
    fn profile_index_is_none_for_unrecognized_names() {
        assert_eq!(profile_index("turbo"), None);
        assert_eq!(profile_index(""), None);
    }

    #[test]
    fn state_for_profile_classifies_known_and_unknown() {
        assert_eq!(state_for_profile("balanced"), ProfileState::Known(1));
        assert_eq!(state_for_profile("eco"), ProfileState::Unknown);
    }

    #[test]
    fn step_target_moves_up_one_from_the_middle() {
        assert_eq!(step_target(ProfileState::Known(1), 1), "performance");
    }

    #[test]
    fn step_target_moves_down_one_from_the_middle() {
        assert_eq!(step_target(ProfileState::Known(1), -1), "power-saver");
    }

    #[test]
    fn step_target_clamps_at_the_top() {
        assert_eq!(step_target(ProfileState::Known(2), 1), "performance");
    }

    #[test]
    fn step_target_clamps_at_the_bottom() {
        assert_eq!(step_target(ProfileState::Known(0), -1), "power-saver");
    }

    #[test]
    fn step_target_ignores_tick_magnitude_beyond_its_sign() {
        // A fast spin (many ticks in one event) still moves only one step.
        assert_eq!(step_target(ProfileState::Known(0), 5), "balanced");
        assert_eq!(step_target(ProfileState::Known(2), -5), "balanced");
    }

    #[test]
    fn step_target_from_unknown_falls_back_to_reset_profile() {
        assert_eq!(step_target(ProfileState::Unknown, 1), RESET_PROFILE);
        assert_eq!(step_target(ProfileState::Unknown, -1), RESET_PROFILE);
    }

    #[test]
    fn step_target_from_unavailable_falls_back_to_reset_profile() {
        assert_eq!(step_target(ProfileState::Unavailable, 1), RESET_PROFILE);
    }
}
