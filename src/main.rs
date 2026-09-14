mod action;
mod decision;
mod format;
mod power_profiles;

use action::PowerProfileAction;
use decision::{ProfileState, state_for_profile};
use futures_util::StreamExt;
use openaction::{OpenActionResult, register_action, run};
use power_profiles::PowerProfilesClient;
use std::time::Duration;

#[tokio::main]
async fn main() -> OpenActionResult<()> {
    simplelog::SimpleLogger::init(log::LevelFilter::Info, simplelog::Config::default())
        .expect("logger init");

    let action = PowerProfileAction::new();
    let watcher = action.clone();
    tokio::spawn(async move { watch_and_reconnect(watcher).await });

    register_action(action).await;
    run(std::env::args().collect()).await
}

/// Runs forever: connects to power-profiles-daemon, seeds the initial
/// profile, then holds its D-Bus change stream open and renders every
/// change to every tracked instance. On any failure (connect, or the
/// stream ending because the bus connection dropped), marks state
/// "Unavailable" and retries after a short delay - the daemon may start
/// after OpenDeck does, or the bus connection may recover.
async fn watch_and_reconnect(action: PowerProfileAction) {
    loop {
        match PowerProfilesClient::connect().await {
            Ok(client) => {
                match client.active_profile().await {
                    Ok(name) => {
                        action
                            .set_state_and_render_all(state_for_profile(&name))
                            .await;
                    }
                    Err(e) => {
                        log::warn!("initial ActiveProfile read failed: {e}");
                        action
                            .set_state_and_render_all(ProfileState::Unavailable)
                            .await;
                    }
                }

                // watch_active_profile()'s stream is built from a `filter_map` over an
                // async closure, which is not `Unpin` - `StreamExt::next()` requires
                // `Self: Unpin`, so it must be pinned first. `Pin<Box<S>>` is always
                // `Unpin` regardless of `S`, which is exactly what makes this work.
                let mut stream = Box::pin(client.watch_active_profile().await);
                action.set_client(Some(client)).await;

                while let Some(name) = stream.next().await {
                    action
                        .set_state_and_render_all(state_for_profile(&name))
                        .await;
                }

                log::warn!("power-profiles-daemon watch stream ended; reconnecting");
                action.set_client(None).await;
                action
                    .set_state_and_render_all(ProfileState::Unavailable)
                    .await;
            }
            Err(e) => {
                log::warn!("power-profiles-daemon connect failed: {e}");
                action
                    .set_state_and_render_all(ProfileState::Unavailable)
                    .await;
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
