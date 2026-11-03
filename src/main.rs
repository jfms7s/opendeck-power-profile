mod action;
mod cooling;
#[cfg(test)]
mod dbus_tests;
mod decision;
mod format;
mod hub;
mod icons;
mod instances;
mod power_profiles;
mod telemetry;
#[cfg(test)]
mod test_support;

use std::sync::Arc;
use std::time::Duration;

use action::{OpenDeckRenderer, PowerProfileAction};
use cooling::{CoolingSync, OPENLINKHUB_ADDR, OpenLinkHub, REQUEST_TIMEOUT};
use hub::{Connector, HubConfig, ProfileHub};
use instances::Instances;
use openaction::{OpenActionResult, register_action, run};
use telemetry::{HWMON_ROOT, POLL_INTERVAL, Telemetry};

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
    simplelog::CombinedLogger::init(
        log_filters()
            .into_iter()
            .map(|(level, config)| -> Box<dyn simplelog::SharedLogger> {
                simplelog::SimpleLogger::new(level, config)
            })
            .collect(),
    )
    .expect("logger init");

    // Switches OpenLinkHub's fan channels along with the profile, when it runs.
    let cooling = CoolingSync::start(OpenLinkHub::new(OPENLINKHUB_ADDR, REQUEST_TIMEOUT));
    let instances = Arc::new(Instances::default());
    let telemetry = Arc::new(Telemetry::default());
    let renderer = Arc::new(OpenDeckRenderer::new(instances.clone(), telemetry.clone()));
    // The CPU temperature on the status line, read only while an instance
    // is on screen. Its own task: a sensor problem never reaches the profile.
    tokio::spawn(telemetry::poll(
        instances.clone(),
        telemetry.clone(),
        renderer.clone(),
        || telemetry::read_cpu_temperature(std::path::Path::new(HWMON_ROOT)),
        POLL_INTERVAL,
    ));
    let hub = ProfileHub::new(
        renderer,
        vec![Arc::new(cooling)],
        HubConfig {
            retry_interval: RETRY_INTERVAL,
            pending_ttl: PENDING_TTL,
        },
    );
    let connector: Connector = Arc::new(|| {
        Box::pin(async {
            zbus::connection::Builder::system()
                .method_timeout(METHOD_TIMEOUT)
                .build()
                .await
        })
    });
    tokio::spawn(hub.clone().run_supervised(connector));

    register_action(PowerProfileAction::new(hub, instances, telemetry)).await;
    run(std::env::args().collect()).await
}

/// The plugin log, as (level, filter) pairs for one logger each: the
/// plugin's own records (and everything else) at Info, but zbus only at
/// Warn - its connection handshake logs at Info, and a reconnect attempt
/// every few seconds while the provider is away would fill the log with it.
/// zbus warnings still get through (they reach `log` via `tracing`).
fn log_filters() -> [(log::LevelFilter, simplelog::Config); 2] {
    let everything_but_zbus = simplelog::ConfigBuilder::new()
        .add_filter_ignore_str("zbus")
        .add_filter_ignore_str("tracing::span")
        .build();
    let zbus = simplelog::ConfigBuilder::new()
        .add_filter_allow_str("zbus")
        .build();
    [
        (log::LevelFilter::Info, everything_but_zbus),
        (log::LevelFilter::Warn, zbus),
    ]
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use log::{Level, Log, Record};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_log_keeps_zbus_warnings_and_our_info_but_not_zbus_info() {
        let buf = SharedBuf::default();
        let logger = simplelog::CombinedLogger::new(
            log_filters()
                .into_iter()
                .map(|(level, config)| -> Box<dyn simplelog::SharedLogger> {
                    simplelog::WriteLogger::new(level, config, buf.clone())
                })
                .collect(),
        );
        let log = |level, target, message| {
            logger.log(
                &Record::builder()
                    .level(level)
                    .target(target)
                    .args(format_args!("{message}"))
                    .build(),
            );
        };
        log(
            Level::Info,
            "zbus::connection::handshake::common",
            "zbus handshake chatter",
        );
        log(Level::Info, "tracing::span", "span chatter");
        log(Level::Warn, "zbus::connection", "zbus warning");
        log(Level::Info, "opendeck_power_profile::hub", "our info");
        let written = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(!written.contains("zbus handshake chatter"), "{written}");
        assert!(!written.contains("span chatter"), "{written}");
        assert_eq!(written.matches("zbus warning").count(), 1, "{written}");
        assert_eq!(written.matches("our info").count(), 1, "{written}");
    }
}
