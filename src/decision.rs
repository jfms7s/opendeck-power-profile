//! Pure profile-stepping logic - no D-Bus, no async, fully unit-testable.

/// The three profiles every `PowerProfiles` D-Bus provider (power-profiles-daemon,
/// tuned-ppd) exposes, in low-to-high power order. Hardcoded rather than read
/// from the provider's `Profiles` property, whose order isn't guaranteed to
/// match power level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    PowerSaver,
    Balanced,
    Performance,
}

impl Profile {
    /// Every profile, in dial order (counter-clockwise end first).
    pub const ALL: [Profile; 3] = [Profile::PowerSaver, Profile::Balanced, Profile::Performance];

    /// What pressing the dial resets to.
    pub const RESET: Profile = Profile::Balanced;

    /// The `ActiveProfile` string the D-Bus provider uses for this profile.
    pub fn dbus_name(self) -> &'static str {
        match self {
            Profile::PowerSaver => "power-saver",
            Profile::Balanced => "balanced",
            Profile::Performance => "performance",
        }
    }

    /// The profile a D-Bus `ActiveProfile` string names, if it is one of the three.
    pub fn from_dbus(name: &str) -> Option<Profile> {
        Profile::ALL.into_iter().find(|p| p.dbus_name() == name)
    }

    fn position(self) -> usize {
        Profile::ALL
            .iter()
            .position(|p| *p == self)
            .expect("every Profile is in Profile::ALL")
    }

    /// The neighbouring profile one step in `direction`'s sign, or `None` if
    /// the dial is already clamped at that end (or `direction` is zero).
    pub fn step(self, direction: i16) -> Option<Profile> {
        let target = match direction.signum() {
            -1 => self.position().checked_sub(1)?,
            1 => self.position() + 1,
            _ => return None,
        };
        Profile::ALL.get(target).copied()
    }
}

/// What the plugin currently knows about the active profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileState {
    /// Connected, and the provider reports one of the three profiles.
    Known(Profile),
    /// Connected, but the provider reports a profile name we don't recognise.
    Unknown,
    /// No `PowerProfiles` provider is reachable, so the dial can't do anything.
    Unavailable,
}

/// Classifies a freshly-read `ActiveProfile` string into a `ProfileState`.
pub fn state_for_profile(name: &str) -> ProfileState {
    match Profile::from_dbus(name) {
        Some(profile) => ProfileState::Known(profile),
        None => ProfileState::Unknown,
    }
}

/// A dial gesture, reduced to what it asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// A rotation by `ticks` detents. Only the sign matters: one step per
    /// event, however fast the dial was spun.
    Step(i16),
    /// A press: jump straight to `Profile::RESET`.
    Reset,
}

/// The profile to ask the provider for when `request` arrives in `state`, or
/// `None` when there is nothing to request (clamped at an end, a zero-tick
/// rotation, or no provider to talk to).
///
/// From `Unknown`, any gesture falls back to `Profile::RESET` rather than
/// guessing a position.
pub fn target_for(state: ProfileState, request: Request) -> Option<Profile> {
    match (state, request) {
        (ProfileState::Unavailable, _) => None,
        (_, Request::Reset) => Some(Profile::RESET),
        (ProfileState::Known(profile), Request::Step(ticks)) => profile.step(ticks),
        (ProfileState::Unknown, Request::Step(ticks)) if ticks != 0 => Some(Profile::RESET),
        (ProfileState::Unknown, Request::Step(_)) => None,
    }
}

/// Whether rotating in `direction`'s sign from `state` would request anything -
/// what the touch strip's side hints show.
pub fn can_step(state: ProfileState, direction: i16) -> bool {
    target_for(state, Request::Step(direction)).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use Profile::*;

    #[test]
    fn from_dbus_finds_each_known_profile_and_round_trips() {
        for profile in Profile::ALL {
            assert_eq!(Profile::from_dbus(profile.dbus_name()), Some(profile));
        }
        assert_eq!(Profile::from_dbus("power-saver"), Some(PowerSaver));
        assert_eq!(Profile::from_dbus("balanced"), Some(Balanced));
        assert_eq!(Profile::from_dbus("performance"), Some(Performance));
    }

    #[test]
    fn from_dbus_is_none_for_unrecognized_names() {
        assert_eq!(Profile::from_dbus("turbo"), None);
        assert_eq!(Profile::from_dbus(""), None);
    }

    #[test]
    fn state_for_profile_classifies_known_and_unknown() {
        assert_eq!(state_for_profile("balanced"), ProfileState::Known(Balanced));
        assert_eq!(state_for_profile("eco"), ProfileState::Unknown);
    }

    #[test]
    fn step_moves_one_profile_either_way_from_the_middle() {
        assert_eq!(
            target_for(ProfileState::Known(Balanced), Request::Step(1)),
            Some(Performance)
        );
        assert_eq!(
            target_for(ProfileState::Known(Balanced), Request::Step(-1)),
            Some(PowerSaver)
        );
    }

    #[test]
    fn step_is_clamped_at_both_ends() {
        assert_eq!(
            target_for(ProfileState::Known(Performance), Request::Step(1)),
            None
        );
        assert_eq!(
            target_for(ProfileState::Known(PowerSaver), Request::Step(-1)),
            None
        );
    }

    #[test]
    fn step_ignores_tick_magnitude_beyond_its_sign() {
        // A fast spin (many ticks in one event) still moves only one step.
        assert_eq!(
            target_for(ProfileState::Known(PowerSaver), Request::Step(5)),
            Some(Balanced)
        );
        assert_eq!(
            target_for(ProfileState::Known(Performance), Request::Step(-5)),
            Some(Balanced)
        );
    }

    #[test]
    fn a_zero_tick_rotation_requests_nothing() {
        for state in [ProfileState::Known(Balanced), ProfileState::Unknown] {
            assert_eq!(target_for(state, Request::Step(0)), None);
        }
    }

    #[test]
    fn step_from_unknown_falls_back_to_the_reset_profile() {
        assert_eq!(
            target_for(ProfileState::Unknown, Request::Step(1)),
            Some(Profile::RESET)
        );
        assert_eq!(
            target_for(ProfileState::Unknown, Request::Step(-1)),
            Some(Profile::RESET)
        );
    }

    #[test]
    fn reset_targets_balanced_from_every_connected_state() {
        for state in [
            ProfileState::Known(PowerSaver),
            ProfileState::Known(Balanced),
            ProfileState::Known(Performance),
            ProfileState::Unknown,
        ] {
            assert_eq!(
                target_for(state, Request::Reset),
                Some(Balanced),
                "from {state:?}"
            );
        }
    }

    #[test]
    fn nothing_is_requested_while_unavailable() {
        for request in [Request::Step(1), Request::Step(-1), Request::Reset] {
            assert_eq!(target_for(ProfileState::Unavailable, request), None);
        }
    }

    #[test]
    fn can_step_matches_what_a_rotation_would_request() {
        assert!(!can_step(ProfileState::Known(PowerSaver), -1));
        assert!(can_step(ProfileState::Known(PowerSaver), 1));
        assert!(can_step(ProfileState::Known(Performance), -1));
        assert!(!can_step(ProfileState::Known(Performance), 1));
        assert!(can_step(ProfileState::Unknown, -1) && can_step(ProfileState::Unknown, 1));
        assert!(
            !can_step(ProfileState::Unavailable, -1) && !can_step(ProfileState::Unavailable, 1)
        );
    }
}
