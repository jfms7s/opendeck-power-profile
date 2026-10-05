//! The plugin's state hub: owns the D-Bus client, what the plugin knows about
//! the active profile, the set of on-screen instances, and the watch loop
//! that keeps them in sync. `action.rs` only translates OpenDeck events into
//! calls on this.
//!
//! The watch loop is the only thing that renders a profile change, whether
//! the change came from this plugin or from outside, so the two can never
//! disagree. Dial requests never render; they only ask the provider.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

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
}

#[derive(Debug, Error)]
pub enum RequestError {
    #[error("no power profiles provider is connected")]
    NotConnected,
    #[error(transparent)]
    Dbus(#[from] PowerProfilesError),
}

/// What the dial shows, plus the profile it last asked for.
struct View {
    /// The last state the provider confirmed - what is rendered.
    confirmed: ProfileState,
    /// A profile requested but not yet confirmed. Rotations step from this,
    /// so two detents inside one D-Bus round trip move two profiles rather
    /// than both stepping from the same confirmed one. Cleared by every
    /// confirmation and by a failed request.
    pending: Option<Profile>,
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
                view: Mutex::new(View {
                    confirmed: ProfileState::Unavailable,
                    pending: None,
                }),
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
        {
            let mut view = lock(&self.inner.view);
            view.pending = None;
            if view.confirmed == state {
                return;
            }
            view.confirmed = state;
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
            let base = view.pending.map_or(view.confirmed, ProfileState::Known);
            let Some(target) = target_for(base, request) else {
                return Ok(()); // clamped, or a zero-tick rotation
            };
            view.pending = Some(target);
            target
        };
        if let Err(e) = client.set_active_profile(target).await {
            let mut view = lock(&self.inner.view);
            if view.pending == Some(target) {
                view.pending = None;
            }
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
