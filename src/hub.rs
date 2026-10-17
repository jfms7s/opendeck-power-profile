//! The plugin's state hub: owns the D-Bus client, what the plugin knows about
//! the active profile, the set of on-screen instances, and the watch loop
//! that keeps them in sync. `action.rs` only translates OpenDeck events into
//! calls on this.
//!
//! The watch loop is the only thing that renders a profile change, whether
//! the change came from this plugin or from outside, so the two can never
//! disagree. Dial requests never render; they only ask the provider.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use thiserror::Error;
use zbus::Connection;

use crate::decision::{Profile, ProfileState, Request, state_for_profile, target_for};
use crate::power_profiles::{PowerProfilesClient, PowerProfilesError, ProfileUpdate};

/// Pushes a state to the touch strips of on-screen instances. The production
/// renderer talks to OpenDeck; tests record what would have been shown.
#[async_trait]
pub trait Renderer: Send + Sync + 'static {
    async fn render_all(&self, instance_ids: &[String], state: ProfileState);
}

/// Opens a fresh bus connection for each (re)connect attempt.
pub type Connector = Arc<dyn Fn() -> BoxFuture<'static, zbus::Result<Connection>> + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct HubConfig {
    /// How long to wait before reconnecting after a failed connect or a lost provider.
    pub retry_interval: Duration,
    /// How long an unconfirmed request keeps counting as the base the next
    /// rotation steps from. A provider that accepts a Set but never announces
    /// it would otherwise leave the dial stepping from a profile that isn't
    /// active. Keep it well above the provider's confirmation round trip.
    pub pending_ttl: Duration,
}

#[derive(Debug, Error)]
pub enum RequestError {
    #[error("no power profiles provider is connected")]
    NotConnected,
    #[error(transparent)]
    Dbus(#[from] PowerProfilesError),
}

/// What the dial shows, plus the profiles it asked for that the provider has
/// not confirmed yet. Pure bookkeeping: every method takes the time it acts
/// at, so the rules are unit-tested without a bus or a clock.
struct View {
    /// The last state the provider confirmed - what is rendered.
    confirmed: ProfileState,
    /// Requested but unconfirmed profiles, oldest first, each with when it
    /// was requested. Rotations step from the newest one still younger than
    /// the TTL, so detents inside one D-Bus round trip each move a profile,
    /// and a reversal steps from the last request rather than from whatever
    /// the provider has confirmed so far. A confirmation of an in-flight
    /// profile drops it and everything requested before it; any other
    /// confirmation (an external change, Unknown, Unavailable) drops them
    /// all; a failed request drops only itself.
    in_flight: VecDeque<(Profile, Instant)>,
    ttl: Duration,
}

impl View {
    fn new(ttl: Duration) -> Self {
        Self {
            confirmed: ProfileState::Unavailable,
            in_flight: VecDeque::new(),
            ttl,
        }
    }

    /// The state the next rotation steps from: the newest request younger
    /// than the TTL, else the confirmed state.
    fn base(&self, now: Instant) -> ProfileState {
        self.in_flight
            .back()
            .filter(|(_, at)| now.saturating_duration_since(*at) < self.ttl)
            .map_or(self.confirmed, |(profile, _)| ProfileState::Known(*profile))
    }

    /// Records that `target` was just requested, forgetting requests past
    /// the TTL so a provider that never confirms can't grow the list.
    fn requested(&mut self, target: Profile, now: Instant) {
        self.in_flight
            .retain(|(_, at)| now.saturating_duration_since(*at) < self.ttl);
        self.in_flight.push_back((target, now));
    }

    /// Forgets the newest request if it is `target`, whose Set failed.
    /// Requests never overlap (openaction runs handlers one at a time), so
    /// the failing one is the newest unless a confirmation already dropped it.
    fn failed(&mut self, target: Profile) {
        if self.in_flight.back().is_some_and(|(p, _)| *p == target) {
            self.in_flight.pop_back();
        }
    }

    /// Records a state the provider confirmed; returns whether it differs
    /// from what was confirmed before (whether to render).
    fn confirm(&mut self, state: ProfileState) -> bool {
        let acknowledged = match state {
            ProfileState::Known(profile) => self.in_flight.iter().position(|(p, _)| *p == profile),
            ProfileState::Unknown | ProfileState::Unavailable => None,
        };
        match acknowledged {
            Some(index) => drop(self.in_flight.drain(..=index)),
            None => self.in_flight.clear(),
        }
        let changed = self.confirmed != state;
        self.confirmed = state;
        changed
    }
}

struct Inner {
    renderer: Arc<dyn Renderer>,
    config: HubConfig,
    // Plain mutexes: none of them is ever held across an `.await`.
    client: Mutex<Option<PowerProfilesClient>>,
    view: Mutex<View>,
    registry: Mutex<HashSet<String>>,
}

#[derive(Clone)]
pub struct ProfileHub {
    inner: Arc<Inner>,
}

/// Why one connected session of the watch loop ended.
enum SessionEnd {
    /// No provider could be reached at all.
    NoProvider(PowerProfilesError),
    /// A provider was connected, then lost.
    Lost(String),
}

impl fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionEnd::NoProvider(e) => write!(f, "no power profiles provider reachable: {e}"),
            SessionEnd::Lost(reason) => write!(f, "power profiles provider lost: {reason}"),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing panics while holding these locks, but if something ever did,
    // the data is still consistent enough to keep the dial working.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl ProfileHub {
    pub fn new(renderer: Arc<dyn Renderer>, config: HubConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                renderer,
                config,
                client: Mutex::new(None),
                view: Mutex::new(View::new(config.pending_ttl)),
                registry: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// Starts tracking an on-screen instance; returns the state to draw it with.
    pub fn track(&self, instance_id: &str) -> ProfileState {
        lock(&self.inner.registry).insert(instance_id.to_string());
        lock(&self.inner.view).confirmed
    }

    pub fn untrack(&self, instance_id: &str) {
        lock(&self.inner.registry).remove(instance_id);
    }

    /// Records a state the provider confirmed and renders it to every tracked
    /// instance - unless it is what they already show.
    async fn confirm(&self, state: ProfileState) {
        if !lock(&self.inner.view).confirm(state) {
            return;
        }
        let instance_ids: Vec<String> = lock(&self.inner.registry).iter().cloned().collect();
        self.inner.renderer.render_all(&instance_ids, state).await;
    }

    fn set_client(&self, client: Option<PowerProfilesClient>) {
        *lock(&self.inner.client) = client;
    }

    /// Asks the provider for the profile a dial gesture means. Never renders:
    /// the watch loop does that once the provider confirms the change.
    pub async fn request(&self, request: Request) -> Result<(), RequestError> {
        // Clone the client out so no lock is held across the D-Bus call.
        let client = lock(&self.inner.client)
            .clone()
            .ok_or(RequestError::NotConnected)?;
        let target = {
            let mut view = lock(&self.inner.view);
            let now = Instant::now();
            let Some(target) = target_for(view.base(now), request) else {
                return Ok(()); // clamped, or a zero-tick rotation
            };
            view.requested(target, now);
            target
        };
        if let Err(e) = client.set_active_profile(target).await {
            lock(&self.inner.view).failed(target);
            return Err(e.into());
        }
        Ok(())
    }

    /// Runs forever: connects to the provider, renders its profile, follows
    /// its changes, and on losing it renders "Unavailable" and reconnects
    /// every `retry_interval`. `connector` opens the bus connection (the
    /// system bus in production).
    pub async fn run(self, connector: Connector) {
        let mut was_unreachable = false;
        loop {
            match self.session(&connector).await {
                SessionEnd::NoProvider(e) if was_unreachable => log::debug!("{e}"),
                end @ SessionEnd::NoProvider(_) => {
                    log::warn!(
                        "{end}; retrying every {:?}",
                        self.inner.config.retry_interval
                    );
                    was_unreachable = true;
                }
                end @ SessionEnd::Lost(_) => {
                    log::warn!("{end}; reconnecting");
                    was_unreachable = false;
                }
            }
            self.set_client(None);
            self.confirm(ProfileState::Unavailable).await;
            tokio::time::sleep(self.inner.config.retry_interval).await;
        }
    }

    /// One connect-and-follow cycle; returns when the provider is unreachable or lost.
    async fn session(&self, connector: &Connector) -> SessionEnd {
        let connected = async {
            let connection = connector().await?;
            PowerProfilesClient::connect(&connection).await
        };
        let client = match connected.await {
            Ok(client) => client,
            Err(e) => return SessionEnd::NoProvider(e),
        };
        log::info!("connected to {}", client.bus_name());
        match self.follow(&client).await {
            Ok(reason) => SessionEnd::Lost(reason.to_string()),
            Err(e) => SessionEnd::Lost(e.to_string()),
        }
    }

    /// Seeds the display from `client` and follows it until it goes away.
    async fn follow(
        &self,
        client: &PowerProfilesClient,
    ) -> Result<&'static str, PowerProfilesError> {
        // Subscribe before the seed read, so a change between the two is not missed.
        let mut owner_changes = Box::pin(client.owner_changes().await?);
        let mut profile_changes = Box::pin(client.profile_changes().await?);
        let seed = client.active_profile().await?;
        self.confirm(state_for_profile(&seed)).await;
        self.set_client(Some(client.clone()));

        loop {
            tokio::select! {
                owner = owner_changes.next() => match owner {
                    // Another process took the name over: whatever we knew is stale.
                    Some(true) => {
                        let name = client.active_profile().await?;
                        self.confirm(state_for_profile(&name)).await;
                    }
                    Some(false) => return Ok("its bus name lost its owner"),
                    None => return Ok("the name-owner stream ended"),
                },
                update = profile_changes.next() => match update {
                    Some(ProfileUpdate::Changed(name)) => {
                        self.confirm(state_for_profile(&name)).await;
                    }
                    Some(ProfileUpdate::Invalidated) => {
                        let name = client.active_profile().await?;
                        self.confirm(state_for_profile(&name)).await;
                    }
                    None => return Ok("the PropertiesChanged stream ended"),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Profile::*;

    const TTL: Duration = Duration::from_secs(3);

    fn known_view(profile: Profile) -> View {
        let mut view = View::new(TTL);
        view.confirm(ProfileState::Known(profile));
        view
    }

    /// Steps the way `ProfileHub::request` does, returning the target.
    fn step(view: &mut View, ticks: i16, now: Instant) -> Profile {
        let target = target_for(view.base(now), Request::Step(ticks)).unwrap();
        view.requested(target, now);
        target
    }

    #[test]
    fn a_reversal_after_a_partial_confirmation_steps_from_the_last_request() {
        let now = Instant::now();
        let mut view = known_view(PowerSaver);
        assert_eq!(step(&mut view, 1, now), Balanced);
        assert_eq!(step(&mut view, 1, now), Performance);
        view.confirm(ProfileState::Known(Balanced));
        assert_eq!(view.base(now), ProfileState::Known(Performance));
        assert_eq!(step(&mut view, -1, now), Balanced);
    }

    #[test]
    fn in_order_confirmations_empty_the_list() {
        let now = Instant::now();
        let mut view = known_view(PowerSaver);
        step(&mut view, 1, now);
        step(&mut view, 1, now);
        step(&mut view, -1, now);
        for profile in [Balanced, Performance, Balanced] {
            view.confirm(ProfileState::Known(profile));
        }
        assert!(view.in_flight.is_empty());
        assert_eq!(view.base(now), ProfileState::Known(Balanced));
    }

    #[test]
    fn a_confirmation_not_in_the_list_clears_it() {
        for external in [
            ProfileState::Known(PowerSaver),
            ProfileState::Unknown,
            ProfileState::Unavailable,
        ] {
            let now = Instant::now();
            let mut view = known_view(Balanced);
            step(&mut view, 1, now);
            view.confirm(external);
            assert!(view.in_flight.is_empty(), "{external:?}");
            assert_eq!(view.base(now), external);
        }
    }

    #[test]
    fn a_failed_newest_request_pops_only_itself() {
        let now = Instant::now();
        let mut view = known_view(PowerSaver);
        step(&mut view, 1, now);
        let newest = step(&mut view, 1, now);
        view.failed(newest);
        assert_eq!(view.in_flight.len(), 1);
        assert_eq!(view.base(now), ProfileState::Known(Balanced));
        // A target that is not the newest (already confirmed away) is left alone.
        view.failed(Performance);
        assert_eq!(view.in_flight.len(), 1);
    }

    #[test]
    fn a_request_older_than_the_ttl_is_not_the_base() {
        let then = Instant::now();
        let mut view = known_view(PowerSaver);
        step(&mut view, 1, then);
        assert_eq!(
            view.base(then + TTL - Duration::from_millis(1)),
            ProfileState::Known(Balanced)
        );
        assert_eq!(view.base(then + TTL), ProfileState::Known(PowerSaver));
        // The next request steps from the confirmed state and drops the stale one.
        assert_eq!(step(&mut view, 1, then + TTL), Balanced);
        assert_eq!(view.in_flight.len(), 1);
    }

    #[test]
    fn confirm_reports_whether_the_state_changed() {
        let mut view = known_view(Balanced);
        assert!(!view.confirm(ProfileState::Known(Balanced)));
        assert!(view.confirm(ProfileState::Known(Performance)));
    }
}
