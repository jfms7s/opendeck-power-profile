//! Thin async adapter over the `PowerProfiles` D-Bus interface on the system
//! bus, as exported by power-profiles-daemon and by tuned-ppd. No pure logic
//! here (see `decision.rs`) - this module only talks to the bus.
//!
//! Every read is a real D-Bus round trip: the proxy is built without zbus's
//! property cache, which is filled once and then only updated by
//! `PropertiesChanged` - it is never cleared when the provider dies, so a
//! cached read would keep answering for a provider that is gone.

use futures_util::{Stream, StreamExt};
use thiserror::Error;
use zbus::proxy::CacheProperties;
use zbus::{Connection, proxy};

use crate::decision::Profile;

#[derive(Debug, Error)]
pub enum PowerProfilesError {
    #[error("D-Bus error: {0}")]
    Dbus(#[from] zbus::Error),
    #[error("D-Bus error: {0}")]
    Fdo(#[from] zbus::fdo::Error),
}

/// One D-Bus name a provider may export the interface under.
#[derive(Debug)]
struct BusName {
    name: &'static str,
    path: &'static str,
    interface: &'static str,
}

/// Tried in order. power-profiles-daemon >= 0.20 and tuned-ppd export both;
/// older power-profiles-daemon only has the legacy `net.hadess` name.
const BUS_NAMES: [BusName; 2] = [
    BusName {
        name: "org.freedesktop.UPower.PowerProfiles",
        path: "/org/freedesktop/UPower/PowerProfiles",
        interface: "org.freedesktop.UPower.PowerProfiles",
    },
    BusName {
        name: "net.hadess.PowerProfiles",
        path: "/net/hadess/PowerProfiles",
        interface: "net.hadess.PowerProfiles",
    },
];

const ACTIVE_PROFILE: &str = "ActiveProfile";

// The interface name, service and path here are only defaults: `connect`
// overrides all three per `BUS_NAMES` entry.
#[proxy(
    interface = "org.freedesktop.UPower.PowerProfiles",
    default_service = "org.freedesktop.UPower.PowerProfiles",
    default_path = "/org/freedesktop/UPower/PowerProfiles"
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn set_active_profile(&self, value: &str) -> zbus::Result<()>;
}

/// What a `PropertiesChanged` signal said about `ActiveProfile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileUpdate {
    /// The provider announced the new value.
    Changed(String),
    /// The provider said the value changed without sending it; read it.
    Invalidated,
}

#[derive(Clone)]
pub struct PowerProfilesClient {
    proxy: PowerProfilesProxy<'static>,
    bus_name: &'static BusName,
}

impl PowerProfilesClient {
    /// Finds a provider on `connection`, preferring the current bus name over
    /// the legacy one. Reads `ActiveProfile` from each candidate, so this
    /// fails when no provider is running (and none can be bus-activated),
    /// not only when the bus itself is unreachable.
    pub async fn connect(connection: &Connection) -> Result<Self, PowerProfilesError> {
        let mut last_error = None;
        for bus_name in &BUS_NAMES {
            let proxy = PowerProfilesProxy::builder(connection)
                .destination(bus_name.name)?
                .path(bus_name.path)?
                .interface(bus_name.interface)?
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            match proxy.active_profile().await {
                Ok(_) => return Ok(Self { proxy, bus_name }),
                Err(e) => {
                    log::debug!("no power profiles provider at {}: {e}", bus_name.name);
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.expect("BUS_NAMES is not empty").into())
    }

    /// The bus name this client talks to.
    pub fn bus_name(&self) -> &'static str {
        self.bus_name.name
    }

    /// Reads `ActiveProfile` from the provider (never from a cache).
    pub async fn active_profile(&self) -> Result<String, PowerProfilesError> {
        Ok(self.proxy.active_profile().await?)
    }

    /// Same D-Bus call `powerprofilesctl set <name>` makes - no elevated
    /// privileges needed on a normal desktop session.
    pub async fn set_active_profile(&self, profile: Profile) -> Result<(), PowerProfilesError> {
        Ok(self.proxy.set_active_profile(profile.dbus_name()).await?)
    }

    /// Yields whenever the bus name changes hands: `false` when the provider
    /// goes away (process exit, crash, `systemctl stop`), `true` when a new
    /// process takes the name over.
    pub async fn owner_changes(
        &self,
    ) -> Result<impl Stream<Item = bool> + 'static, PowerProfilesError> {
        Ok(self
            .proxy
            .inner()
            .receive_owner_changed()
            .await?
            .map(|owner| owner.is_some()))
    }

    /// Yields every `ActiveProfile` change the provider announces, whether
    /// this plugin caused it or something external did (GUI applet, another
    /// key, a terminal `powerprofilesctl`).
    pub async fn profile_changes(
        &self,
    ) -> Result<impl Stream<Item = ProfileUpdate> + 'static, PowerProfilesError> {
        let properties = zbus::fdo::PropertiesProxy::builder(self.proxy.inner().connection())
            .destination(self.bus_name.name)?
            .path(self.bus_name.path)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let interface = self.bus_name.interface;
        let signals = properties.receive_properties_changed().await?;
        Ok(signals.filter_map(move |signal| async move {
            let args = match signal.args() {
                Ok(args) => args,
                Err(e) => {
                    log::warn!("unreadable PropertiesChanged signal from {interface}: {e}");
                    return None;
                }
            };
            if args.interface_name().as_str() != interface {
                return None;
            }
            if let Some(value) = args.changed_properties().get(ACTIVE_PROFILE) {
                return match <&str>::try_from(value) {
                    Ok(name) => Some(ProfileUpdate::Changed(name.to_string())),
                    Err(e) => {
                        log::warn!("{interface}.{ACTIVE_PROFILE} changed to a non-string: {e}");
                        Some(ProfileUpdate::Invalidated)
                    }
                };
            }
            args.invalidated_properties()
                .contains(&ACTIVE_PROFILE)
                .then_some(ProfileUpdate::Invalidated)
        }))
    }
}
