mod action;
#[cfg(test)]
mod dbus_tests;
mod decision;
mod format;
mod hub;
mod icons;
mod power_profiles;
#[cfg(test)]
mod test_support;

use std::sync::Arc;
use std::time::Duration;

use action::{OpenDeckRenderer, PowerProfileAction};
use hub::{Connector, HubConfig, ProfileHub};
use openaction::{OpenActionResult, register_action, run};

/// How long to wait before reconnecting after the provider is lost or a
/// connect attempt fails.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// How long an unconfirmed dial request stays the base the next rotation
/// steps from. Well above tuned-ppd's confirmation round trip (one signal),
/// short enough that a provider that never announces a Set doesn't leave the
/// dial stepping from a profile that isn't active for long.
const PENDING_TTL: Duration = Duration::from_secs(3);

/// Bounds every D-Bus method call, so a provider that is alive but stuck
/// (in polkit, say) cannot hold a dial event - and every event queued behind
/// it - forever. Long enough for D-Bus to activate a stopped provider on
/// connect, which goes through the same connection.
const METHOD_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main(flavor = "current_thread")]
async fn main() -> OpenActionResult<()> {
    simplelog::SimpleLogger::init(log::LevelFilter::Info, simplelog::Config::default())
        .expect("logger init");

    let hub = ProfileHub::new(
        Arc::new(OpenDeckRenderer),
        HubConfig {
            retry_interval: RETRY_INTERVAL,
            pending_ttl: PENDING_TTL,
        },
    );
    let connector: Connector = Arc::new(|| {
        Box::pin(async {
            zbus::connection::Builder::system()?
                .method_timeout(METHOD_TIMEOUT)
                .build()
                .await
        })
    });
    tokio::spawn(supervise(hub.clone(), connector));

    register_action(PowerProfileAction::new(hub)).await;
    run(std::env::args().collect()).await
}

/// Keeps the watch loop alive: `ProfileHub::run` never returns, so the only
/// way it ends is a panic - log it and start a fresh one rather than leave
/// the dial frozen on its last state.
async fn supervise(hub: ProfileHub, connector: Connector) {
    loop {
        let watcher = tokio::spawn(hub.clone().run(connector.clone()));
        if let Err(e) = watcher.await {
            log::error!("power profile watcher stopped ({e}); restarting it");
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}
