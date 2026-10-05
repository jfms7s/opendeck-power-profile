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

#[tokio::main(flavor = "current_thread")]
async fn main() -> OpenActionResult<()> {
    simplelog::SimpleLogger::init(log::LevelFilter::Info, simplelog::Config::default())
        .expect("logger init");

    let hub = ProfileHub::new(
        Arc::new(OpenDeckRenderer),
        HubConfig {
            retry_interval: RETRY_INTERVAL,
        },
    );
    let connector: Connector = Arc::new(|| Box::pin(zbus::Connection::system()));
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
