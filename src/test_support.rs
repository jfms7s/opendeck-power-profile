//! Test harness for the D-Bus side: a private `dbus-daemon` per test, a fake
//! `PowerProfiles` provider served on it, and a renderer that records what
//! the touch strip would have shown. Nothing here touches the system bus or
//! the machine's real power profile.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::Value;

use crate::decision::ProfileState;
use crate::hub::{Connector, Renderer};

/// Sends `log` records to stderr, which the test harness captures per test
/// and shows only for failures.
pub fn init_test_logging() {
    struct StderrLog;
    impl log::Log for StderrLog {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.target().starts_with("opendeck_power_profile")
        }
        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                eprintln!("[{}] {}", record.level(), record.args());
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: StderrLog = StderrLog;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
}

/// A throwaway message bus, killed on drop.
pub struct PrivateBus {
    child: Child,
    dir: PathBuf,
    pub address: String,
}

impl PrivateBus {
    pub fn start() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "opendeck-power-profile-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.display()
            ),
        )
        .unwrap();
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("these tests need `dbus-daemon` on PATH (Debian/Ubuntu/Fedora package: dbus-daemon / dbus)");
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            dir,
            address: address.trim().to_string(),
        }
    }

    /// Connects the hub to this bus instead of the system bus.
    pub fn connector(&self) -> Connector {
        let address = self.address.clone();
        Arc::new(move || {
            let address = address.clone();
            Box::pin(async move {
                zbus::connection::Builder::address(address.as_str())?
                    .build()
                    .await
            })
        })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Which D-Bus name a fake provider exports.
#[derive(Debug, Clone, Copy)]
pub enum Flavor {
    /// The current name, exported by power-profiles-daemon >= 0.20 and tuned-ppd.
    UPower,
    /// The legacy name, the only one older power-profiles-daemon exports.
    Hadess,
}

impl Flavor {
    fn name(self) -> &'static str {
        match self {
            Flavor::UPower => "org.freedesktop.UPower.PowerProfiles",
            Flavor::Hadess => "net.hadess.PowerProfiles",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Flavor::UPower => "/org/freedesktop/UPower/PowerProfiles",
            Flavor::Hadess => "/net/hadess/PowerProfiles",
        }
    }
}

#[derive(Default)]
struct FakeState {
    profile: Mutex<String>,
    /// Delay between accepting a Set and announcing it via PropertiesChanged.
    signal_delay: Mutex<Duration>,
    /// Delay before a Set call returns.
    set_delay: Mutex<Duration>,
    fail_sets: AtomicBool,
    conn: OnceLock<Connection>,
    flavor: OnceLock<Flavor>,
}

async fn announce(state: &FakeState, profile: &str) {
    let flavor = *state.flavor.get().unwrap();
    let emitter = SignalEmitter::new(state.conn.get().unwrap(), flavor.path()).unwrap();
    zbus::fdo::Properties::properties_changed(
        &emitter,
        InterfaceName::try_from(flavor.name()).unwrap(),
        HashMap::from([("ActiveProfile", Value::from(profile))]),
        Cow::Borrowed(&[]),
    )
    .await
    .unwrap();
}

async fn handle_set(state: &Arc<FakeState>, value: String) -> zbus::fdo::Result<()> {
    let set_delay = *state.set_delay.lock().unwrap();
    tokio::time::sleep(set_delay).await;
    if state.fail_sets.load(Ordering::SeqCst) {
        return Err(zbus::fdo::Error::AccessDenied(
            "fake provider rejects sets".into(),
        ));
    }
    *state.profile.lock().unwrap() = value.clone();
    let signal_delay = *state.signal_delay.lock().unwrap();
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(signal_delay).await;
        announce(&state, &value).await;
    });
    Ok(())
}

macro_rules! fake_interface {
    ($ty:ident, $iface:tt) => {
        struct $ty(Arc<FakeState>);

        #[zbus::interface(name = $iface)]
        impl $ty {
            #[zbus(property(emits_changed_signal = "false"))]
            fn active_profile(&self) -> String {
                self.0.profile.lock().unwrap().clone()
            }

            #[zbus(property)]
            async fn set_active_profile(&mut self, value: String) -> zbus::fdo::Result<()> {
                handle_set(&self.0, value).await
            }
        }
    };
}

fake_interface!(FakeUPower, "org.freedesktop.UPower.PowerProfiles");
fake_interface!(FakeHadess, "net.hadess.PowerProfiles");

/// A fake power-profiles-daemon / tuned-ppd on a private bus.
pub struct FakeProvider {
    state: Arc<FakeState>,
}

impl FakeProvider {
    pub async fn start(bus: &PrivateBus, flavor: Flavor, profile: &str) -> Self {
        let state = Arc::new(FakeState::default());
        *state.profile.lock().unwrap() = profile.to_string();
        state.flavor.set(flavor).unwrap();
        let builder = zbus::connection::Builder::address(bus.address.as_str()).unwrap();
        let builder = match flavor {
            Flavor::UPower => builder.serve_at(flavor.path(), FakeUPower(state.clone())),
            Flavor::Hadess => builder.serve_at(flavor.path(), FakeHadess(state.clone())),
        }
        .unwrap();
        let conn = builder.name(flavor.name()).unwrap().build().await.unwrap();
        state.conn.set(conn).unwrap();
        Self { state }
    }

    /// Simulates something other than the plugin (a GUI applet, a terminal
    /// `powerprofilesctl`) changing the profile.
    pub async fn change_externally(&self, profile: &str) {
        *self.state.profile.lock().unwrap() = profile.to_string();
        announce(&self.state, profile).await;
    }

    pub fn profile(&self) -> String {
        self.state.profile.lock().unwrap().clone()
    }

    pub fn delay_signals_by(&self, delay: Duration) {
        *self.state.signal_delay.lock().unwrap() = delay;
    }

    pub fn delay_sets_by(&self, delay: Duration) {
        *self.state.set_delay.lock().unwrap() = delay;
    }

    pub fn reject_sets(&self) {
        self.state.fail_sets.store(true, Ordering::SeqCst);
    }

    /// The provider process dying: its bus connection closes, so the bus
    /// drops its name and fails any call still waiting on it.
    pub async fn stop(self) {
        let conn = self.state.conn.get().unwrap().clone();
        conn.close().await.unwrap();
    }
}

/// A renderer that records every state the hub pushes.
pub struct RecordingRenderer(mpsc::UnboundedSender<(Vec<String>, ProfileState)>);

#[async_trait]
impl Renderer for RecordingRenderer {
    async fn render_all(&self, instance_ids: &[String], state: ProfileState) {
        let _ = self.0.send((instance_ids.to_vec(), state));
    }
}

pub struct Rendered(mpsc::UnboundedReceiver<(Vec<String>, ProfileState)>);

pub fn recording_renderer() -> (Arc<RecordingRenderer>, Rendered) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(RecordingRenderer(tx)), Rendered(rx))
}

impl Rendered {
    /// Waits until the hub renders `want`, failing if it doesn't within `within`.
    pub async fn expect(&mut self, want: ProfileState, within: Duration) {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.0.recv()).await {
                Ok(Some((_, state))) if state == want => return,
                Ok(Some((_, state))) => seen.push(state),
                Ok(None) => panic!("renderer dropped while waiting for {want:?}"),
                Err(_) => panic!("{want:?} not rendered within {within:?}; rendered {seen:?}"),
            }
        }
    }

    /// The next render, with the instance ids it went to.
    pub async fn next(&mut self, within: Duration) -> (Vec<String>, ProfileState) {
        tokio::time::timeout(within, self.0.recv())
            .await
            .expect("nothing rendered in time")
            .expect("renderer dropped")
    }

    /// Asserts nothing is rendered for `period`.
    pub async fn expect_quiet(&mut self, period: Duration) {
        if let Ok(Some(render)) = tokio::time::timeout(period, self.0.recv()).await {
            panic!("expected no render, got {render:?}");
        }
    }
}
