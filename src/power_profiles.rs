//! Thin async adapter over the `PowerProfiles` D-Bus interface on the system
//! bus, as exported by power-profiles-daemon and by tuned-ppd. No pure logic
//! here (see `decision.rs`) - this module only talks to the bus.
//!
//! Every read is a real D-Bus round trip: the proxy is built without zbus's
//! property cache, which is filled once and then only updated by
//! `PropertiesChanged` - it is never cleared when the provider dies, so a
//! cached read would keep answering for a provider that is gone.

use std::collections::HashMap;

use futures_util::{Stream, StreamExt, stream};
use thiserror::Error;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, proxy};

use crate::decision::{Conditions, Hold, Profile};

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
const ACTIVE_PROFILE_HOLDS: &str = "ActiveProfileHolds";
const PERFORMANCE_DEGRADED: &str = "PerformanceDegraded";

/// One `ActiveProfileHolds` entry as it comes off the bus (`a{sv}`).
type RawHold = HashMap<String, OwnedValue>;

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

    #[zbus(property)]
    fn active_profile_holds(&self) -> zbus::Result<Vec<RawHold>>;

    #[zbus(property)]
    fn performance_degraded(&self) -> zbus::Result<String>;
}

/// What a `PropertiesChanged` signal said. One signal can carry several
/// properties, so it can yield several of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileUpdate {
    /// The provider announced the new `ActiveProfile`.
    Changed(String),
    /// The provider said `ActiveProfile` changed without sending it; read it.
    Invalidated,
    /// The provider announced the new `ActiveProfileHolds`.
    Holds(Vec<Hold>),
    /// The provider announced the new `PerformanceDegraded`.
    Degraded(String),
    /// Holds or degradation changed but the new value wasn't usable (not
    /// sent, or the wrong type); read both again.
    ConditionsInvalidated,
}

/// Reads a string field of a hold, empty when missing or not a string.
fn string_field(raw: &RawHold, key: &str) -> String {
    raw.get(key)
        .and_then(|value| <&str>::try_from(&**value).ok())
        .unwrap_or_default()
        .to_string()
}

/// Turns the provider's hold maps into `Hold`s, tolerating missing keys.
fn holds_from(raw: &[RawHold]) -> Vec<Hold> {
    raw.iter()
        .map(|hold| Hold {
            application_id: string_field(hold, "ApplicationId"),
            profile: string_field(hold, "Profile"),
            reason: string_field(hold, "Reason"),
        })
        .collect()
}

/// Decodes an `ActiveProfileHolds` value from a signal, if it is an `aa{sv}`.
fn holds_from_value(value: &Value<'_>) -> Option<Vec<Hold>> {
    let owned = value.try_to_owned().ok()?;
    let raw = Vec::<RawHold>::try_from(owned).ok()?;
    Some(holds_from(&raw))
}

/// Everything one `PropertiesChanged` signal for `interface` says.
fn updates_from(
    changed: &HashMap<&str, Value<'_>>,
    invalidated: &[&str],
    interface: &str,
) -> Vec<ProfileUpdate> {
    let mut updates = Vec::new();
    if let Some(value) = changed.get(ACTIVE_PROFILE) {
        updates.push(match <&str>::try_from(value) {
            Ok(name) => ProfileUpdate::Changed(name.to_string()),
            Err(e) => {
                log::warn!("{interface}.{ACTIVE_PROFILE} changed to a non-string: {e}");
                ProfileUpdate::Invalidated
            }
        });
    } else if invalidated.contains(&ACTIVE_PROFILE) {
        updates.push(ProfileUpdate::Invalidated);
    }
    let mut reread_conditions = false;
    match changed.get(ACTIVE_PROFILE_HOLDS) {
        Some(value) => match holds_from_value(value) {
            Some(holds) => updates.push(ProfileUpdate::Holds(holds)),
            None => {
                log::debug!("{interface}.{ACTIVE_PROFILE_HOLDS} changed to an unexpected type");
                reread_conditions = true;
            }
        },
        None => reread_conditions |= invalidated.contains(&ACTIVE_PROFILE_HOLDS),
    }
    match changed.get(PERFORMANCE_DEGRADED) {
        Some(value) => match <&str>::try_from(value) {
            Ok(reason) => updates.push(ProfileUpdate::Degraded(reason.to_string())),
            Err(e) => {
                log::debug!("{interface}.{PERFORMANCE_DEGRADED} changed to a non-string: {e}");
                reread_conditions = true;
            }
        },
        None => reread_conditions |= invalidated.contains(&PERFORMANCE_DEGRADED),
    }
    if reread_conditions {
        updates.push(ProfileUpdate::ConditionsInvalidated);
    }
    updates
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

    /// Reads `ActiveProfileHolds` and `PerformanceDegraded`. Display only, so
    /// never an error: a provider without them (or a failed read) counts as
    /// no holds and not degraded.
    pub async fn conditions(&self) -> Conditions {
        let holds = match self.proxy.active_profile_holds().await {
            Ok(raw) => holds_from(&raw),
            Err(e) => {
                log::debug!("reading {ACTIVE_PROFILE_HOLDS} failed: {e}");
                Vec::new()
            }
        };
        let degraded = self.proxy.performance_degraded().await.unwrap_or_else(|e| {
            log::debug!("reading {PERFORMANCE_DEGRADED} failed: {e}");
            String::new()
        });
        Conditions { holds, degraded }
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

    /// Yields every change the provider announces to `ActiveProfile`,
    /// `ActiveProfileHolds` or `PerformanceDegraded`, whether this plugin
    /// caused it or something external did (GUI applet, another key, a
    /// terminal `powerprofilesctl`, an app taking a hold).
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
        Ok(signals.flat_map(move |signal| {
            let updates = match signal.args() {
                Ok(args) if args.interface_name().as_str() == interface => updates_from(
                    args.changed_properties(),
                    args.invalidated_properties(),
                    interface,
                ),
                Ok(_) => Vec::new(),
                Err(e) => {
                    log::warn!("unreadable PropertiesChanged signal from {interface}: {e}");
                    Vec::new()
                }
            };
            stream::iter(updates)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(fields: &[(&str, Value<'static>)]) -> RawHold {
        fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.try_to_owned().unwrap()))
            .collect()
    }

    #[test]
    fn holds_are_parsed_leniently() {
        let holds = holds_from(&[
            raw(&[
                (
                    "ApplicationId",
                    Value::from("com.feralinteractive.GameMode"),
                ),
                ("Profile", Value::from("performance")),
                ("Reason", Value::from("game")),
                ("Extra", Value::from(7u32)),
            ]),
            raw(&[
                ("Profile", Value::from("power-saver")),
                ("ApplicationId", Value::from(1u32)),
            ]),
            raw(&[]),
        ]);
        assert_eq!(
            holds,
            vec![
                Hold {
                    application_id: "com.feralinteractive.GameMode".into(),
                    profile: "performance".into(),
                    reason: "game".into(),
                },
                Hold {
                    profile: "power-saver".into(),
                    ..Hold::default()
                },
                Hold::default(),
            ]
        );
    }

    #[test]
    fn one_signal_can_carry_every_property() {
        let holds = Value::from(vec![HashMap::from([(
            "ApplicationId".to_string(),
            Value::from("app"),
        )])]);
        let changed = HashMap::from([
            (ACTIVE_PROFILE, Value::from("balanced")),
            (ACTIVE_PROFILE_HOLDS, holds),
            (PERFORMANCE_DEGRADED, Value::from("lap-detected")),
        ]);
        assert_eq!(
            updates_from(&changed, &[], "i"),
            vec![
                ProfileUpdate::Changed("balanced".into()),
                ProfileUpdate::Holds(vec![Hold {
                    application_id: "app".into(),
                    ..Hold::default()
                }]),
                ProfileUpdate::Degraded("lap-detected".into()),
            ]
        );
    }

    #[test]
    fn unusable_or_invalidated_conditions_ask_for_one_re_read() {
        let changed = HashMap::from([
            (ACTIVE_PROFILE_HOLDS, Value::from("not an array")),
            (PERFORMANCE_DEGRADED, Value::from(3u32)),
        ]);
        assert_eq!(
            updates_from(&changed, &[], "i"),
            vec![ProfileUpdate::ConditionsInvalidated]
        );
        assert_eq!(
            updates_from(
                &HashMap::new(),
                &[ACTIVE_PROFILE_HOLDS, ACTIVE_PROFILE],
                "i"
            ),
            vec![
                ProfileUpdate::Invalidated,
                ProfileUpdate::ConditionsInvalidated
            ]
        );
        assert!(updates_from(&HashMap::new(), &["Other"], "i").is_empty());
    }
}
