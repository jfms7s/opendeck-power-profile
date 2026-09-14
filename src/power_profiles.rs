//! Thin async wrapper around power-profiles-daemon's D-Bus interface. No
//! pure logic here (see `decision.rs`) - this module only talks to the
//! system bus.

use futures_util::{Stream, StreamExt};
use thiserror::Error;
use zbus::Connection;
use zbus::proxy;

#[derive(Debug, Error)]
pub enum PowerProfilesError {
    #[error("D-Bus error: {0}")]
    Dbus(#[from] zbus::Error),
}

#[proxy(
    interface = "net.hadess.PowerProfiles",
    default_service = "net.hadess.PowerProfiles",
    default_path = "/net/hadess/PowerProfiles"
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn set_active_profile(&self, value: &str) -> zbus::Result<()>;
}

pub struct PowerProfilesClient {
    proxy: PowerProfilesProxy<'static>,
}

impl PowerProfilesClient {
    /// Connects to the system bus and builds the proxy. Fails if the bus is
    /// unreachable or power-profiles-daemon isn't running/exporting the
    /// interface.
    pub async fn connect() -> Result<Self, PowerProfilesError> {
        let connection = Connection::system().await?;
        let proxy = PowerProfilesProxy::new(&connection).await?;
        Ok(Self { proxy })
    }

    pub async fn active_profile(&self) -> Result<String, PowerProfilesError> {
        Ok(self.proxy.active_profile().await?)
    }

    /// Same D-Bus call `powerprofilesctl set <name>` makes - no elevated
    /// privileges needed on a normal desktop session.
    pub async fn set_active_profile(&self, name: &str) -> Result<(), PowerProfilesError> {
        Ok(self.proxy.set_active_profile(name).await?)
    }

    /// Yields the new profile name every time `ActiveProfile` changes,
    /// whether this plugin caused it or something external did (GUI
    /// applet, another key, a terminal `powerprofilesctl`).
    pub async fn watch_active_profile(&self) -> impl Stream<Item = String> + 'static {
        self.proxy
            .receive_active_profile_changed()
            .await
            .filter_map(|changed| async move { changed.get().await.ok() })
    }
}
