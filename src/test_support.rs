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
use zbus::zvariant::{OwnedValue, Value};

use crate::decision::{ProfileState, Snapshot};
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
        self.connector_built_with(|builder| builder)
    }

    /// Like [`Self::connector`], but every method call on the connection
    /// fails once it has waited `timeout`, as production's does.
    pub fn connector_with_timeout(&self, timeout: Duration) -> Connector {
        self.connector_built_with(move |builder| builder.method_timeout(timeout))
    }

    fn connector_built_with(
        &self,
        configure: impl Fn(zbus::connection::Builder<'static>) -> zbus::connection::Builder<'static>
        + Send
        + Sync
        + 'static,
    ) -> Connector {
        let address = self.address.clone();
        let configure = Arc::new(configure);
        Arc::new(move || {
            let address = address.clone();
            let configure = configure.clone();
            Box::pin(async move {
                configure(zbus::connection::Builder::address(address.as_str())?)
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

/// A fake hold: `(ApplicationId, Profile)`.
pub type FakeHold = (String, String);

#[derive(Default)]
struct FakeState {
    profile: Mutex<String>,
    holds: Mutex<Vec<FakeHold>>,
    degraded: Mutex<String>,
    /// Make reads of `ActiveProfileHolds` and `PerformanceDegraded` fail, as
    /// on a provider that doesn't implement them.
    fail_condition_reads: AtomicBool,
    /// Delay between accepting a Set and announcing it via PropertiesChanged.
    signal_delay: Mutex<Duration>,
    /// Delay before a Set call returns.
    set_delay: Mutex<Duration>,
    fail_sets: AtomicBool,
    conn: OnceLock<Connection>,
    flavor: OnceLock<Flavor>,
}

fn holds_value(holds: &[FakeHold]) -> Vec<HashMap<String, OwnedValue>> {
    holds
        .iter()
        .map(|(application_id, profile)| {
            HashMap::from([
                (
                    "ApplicationId".to_string(),
                    OwnedValue::from(zbus::zvariant::Str::from(application_id.clone())),
                ),
                (
                    "Profile".to_string(),
                    OwnedValue::from(zbus::zvariant::Str::from(profile.clone())),
                ),
                (
                    "Reason".to_string(),
                    OwnedValue::from(zbus::zvariant::Str::from("test")),
                ),
            ])
        })
        .collect()
}

async fn emit_changed(state: &FakeState, changed: HashMap<&str, Value<'_>>) {
    let flavor = *state.flavor.get().unwrap();
    let emitter = SignalEmitter::new(state.conn.get().unwrap(), flavor.path()).unwrap();
    zbus::fdo::Properties::properties_changed(
        &emitter,
        InterfaceName::try_from(flavor.name()).unwrap(),
        changed,
        Cow::Borrowed(&[]),
    )
    .await
    .unwrap();
}

async fn announce(state: &FakeState, profile: &str) {
    emit_changed(
        state,
        HashMap::from([("ActiveProfile", Value::from(profile))]),
    )
    .await;
}

async fn announce_holds(state: &FakeState) {
    let holds = holds_value(&state.holds.lock().unwrap());
    emit_changed(
        state,
        HashMap::from([("ActiveProfileHolds", Value::from(holds))]),
    )
    .await;
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
    // Like tuned-ppd: setting the profile releases every hold.
    let released = !std::mem::take(&mut *state.holds.lock().unwrap()).is_empty();
    let signal_delay = *state.signal_delay.lock().unwrap();
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(signal_delay).await;
        announce(&state, &value).await;
        if released {
            announce_holds(&state).await;
        }
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

            #[zbus(property(emits_changed_signal = "false"))]
            fn active_profile_holds(&self) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
                if self.0.fail_condition_reads.load(Ordering::SeqCst) {
                    return Err(zbus::fdo::Error::UnknownProperty(
                        "ActiveProfileHolds".into(),
                    ));
                }
                Ok(holds_value(&self.0.holds.lock().unwrap()))
            }

            #[zbus(property(emits_changed_signal = "false"))]
            fn performance_degraded(&self) -> zbus::fdo::Result<String> {
                if self.0.fail_condition_reads.load(Ordering::SeqCst) {
                    return Err(zbus::fdo::Error::UnknownProperty(
                        "PerformanceDegraded".into(),
                    ));
                }
                Ok(self.0.degraded.lock().unwrap().clone())
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

    /// Simulates applications taking or releasing holds (`HoldProfile` /
    /// `ReleaseProfile`): replaces the holds and announces them.
    pub async fn set_holds(&self, holds: &[(&str, &str)]) {
        *self.state.holds.lock().unwrap() = holds
            .iter()
            .map(|(app, profile)| (app.to_string(), profile.to_string()))
            .collect();
        announce_holds(&self.state).await;
    }

    /// Sets `PerformanceDegraded` and announces it.
    pub async fn set_degraded(&self, reason: &str) {
        *self.state.degraded.lock().unwrap() = reason.to_string();
        emit_changed(
            &self.state,
            HashMap::from([("PerformanceDegraded", Value::from(reason))]),
        )
        .await;
    }

    /// Sets holds without announcing them, as if they existed before the
    /// plugin connected.
    pub fn preset_holds(&self, holds: &[(&str, &str)]) {
        *self.state.holds.lock().unwrap() = holds
            .iter()
            .map(|(app, profile)| (app.to_string(), profile.to_string()))
            .collect();
    }

    /// Makes the holds and degraded reads fail, like a provider without them.
    pub fn fail_condition_reads(&self) {
        self.state
            .fail_condition_reads
            .store(true, Ordering::SeqCst);
    }

    pub fn holds(&self) -> Vec<FakeHold> {
        self.state.holds.lock().unwrap().clone()
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

/// A renderer that records every snapshot the hub pushes.
pub struct RecordingRenderer(mpsc::UnboundedSender<(Vec<String>, Snapshot)>);

#[async_trait]
impl Renderer for RecordingRenderer {
    async fn render_all(&self, instance_ids: &[String], snapshot: &Snapshot) {
        let _ = self.0.send((instance_ids.to_vec(), snapshot.clone()));
    }
}

pub struct Rendered(mpsc::UnboundedReceiver<(Vec<String>, Snapshot)>);

pub fn recording_renderer() -> (Arc<RecordingRenderer>, Rendered) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(RecordingRenderer(tx)), Rendered(rx))
}

impl Rendered {
    /// Waits until the hub renders `want`, failing if it doesn't within `within`.
    pub async fn expect(&mut self, want: ProfileState, within: Duration) {
        self.expect_where(&format!("{want:?}"), within, |s| s.state == want)
            .await;
    }

    /// Waits until the hub renders a snapshot whose status line is `want`.
    pub async fn expect_status(&mut self, want: &str, within: Duration) {
        self.expect_where(&format!("status {want:?}"), within, |s| {
            crate::format::status_text(s) == want
        })
        .await;
    }

    /// Waits until the hub renders a snapshot matching `matches`.
    pub async fn expect_where(
        &mut self,
        what: &str,
        within: Duration,
        matches: impl Fn(&Snapshot) -> bool,
    ) {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.0.recv()).await {
                Ok(Some((_, snapshot))) if matches(&snapshot) => return,
                Ok(Some((_, snapshot))) => seen.push(snapshot),
                Ok(None) => panic!("renderer dropped while waiting for {what}"),
                Err(_) => panic!("{what} not rendered within {within:?}; rendered {seen:?}"),
            }
        }
    }

    /// The next render, with the instance ids it went to.
    pub async fn next(&mut self, within: Duration) -> (Vec<String>, Snapshot) {
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

/// One request the fake OpenLinkHub received.
#[derive(Debug, Clone)]
struct HttpRequest {
    method: String,
    path: String,
    body: String,
}

#[derive(Default)]
struct FakeHubState {
    requests: Mutex<Vec<HttpRequest>>,
    refuse_writes: AtomicBool,
}

/// A local stand-in for OpenLinkHub's HTTP API on a random loopback port:
/// answers `GET /api/` with a fixture and `POST /api/speed` with success
/// (or a refusal), recording every request. Aborted on drop.
pub struct FakeOpenLinkHub {
    addr: String,
    state: Arc<FakeHubState>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeOpenLinkHub {
    pub async fn start(api_response: &'static str) -> Self {
        Self::serve(Some(api_response)).await
    }

    /// Accepts connections and never answers.
    pub async fn silent() -> Self {
        Self::serve(None).await
    }

    async fn serve(api_response: Option<&'static str>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = Arc::new(FakeHubState::default());
        let served = state.clone();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                match api_response {
                    None => held.push(stream),
                    Some(api) => {
                        tokio::spawn(answer(stream, api, served.clone()));
                    }
                }
            }
        });
        Self { addr, state, task }
    }

    pub fn addr(&self) -> String {
        self.addr.clone()
    }

    /// A client for this server with a short timeout.
    pub fn client(&self) -> crate::cooling::OpenLinkHub {
        crate::cooling::OpenLinkHub::new(self.addr(), Duration::from_secs(1))
    }

    pub fn refuse_writes(&self) {
        self.state.refuse_writes.store(true, Ordering::SeqCst);
    }

    /// How many requests arrived, of any kind.
    pub fn requests(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }

    /// The bodies of every `POST /api/speed` so far.
    pub fn posts(&self) -> Vec<String> {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/api/speed")
            .map(|r| r.body.clone())
            .collect()
    }

    /// Waits until at least `count` writes arrived, then a little longer to
    /// catch extra ones, and returns them all.
    pub async fn wait_for_posts(&self, count: usize, within: Duration) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + within;
        while self.posts().len() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected {count} POST /api/speed within {within:?}, got {:?}",
                self.posts()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.posts()
    }
}

impl Drop for FakeOpenLinkHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn answer(mut stream: tokio::net::TcpStream, api: &'static str, state: Arc<FakeHubState>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let body_start = loop {
        let n = stream.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..body_start]).to_string();
    let length: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0);
    while raw.len() < body_start + length {
        let n = stream.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let mut request_line = head.lines().next().unwrap_or("").split_whitespace();
    let request = HttpRequest {
        method: request_line.next().unwrap_or("").to_string(),
        path: request_line.next().unwrap_or("").to_string(),
        body: String::from_utf8_lossy(&raw[body_start..]).to_string(),
    };
    let reply = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/") => api.to_string(),
        ("POST", "/api/speed") if state.refuse_writes.load(Ordering::SeqCst) => {
            r#"{"code":200,"status":0,"message":"Unable to apply speed profile"}"#.to_string()
        }
        ("POST", "/api/speed") => {
            r#"{"code":200,"status":1,"message":"Speed profile successfully applied"}"#.to_string()
        }
        _ => r#"{"code":404,"status":0}"#.to_string(),
    };
    state.requests.lock().unwrap().push(request);
    let response = format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{reply}");
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}
