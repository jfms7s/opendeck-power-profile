//! Test harness for the D-Bus side: a private `dbus-daemon` per test, a fake
//! `PowerProfiles` provider served on it, and a renderer that records what
//! the touch strip would have shown. Nothing here touches the system bus or
//! the machine's real power profile.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{OpenOptions, Permissions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::fdo::RequestNameFlags;
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;
use zbus::{OwnedVariant, Variant};

use crate::decision::{ProfileState, Snapshot};
use crate::hub::{Connector, Renderer};
use crate::instances::Instances;

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
    /// Kills `child` and removes `dir` if this process dies without dropping
    /// the bus.
    watchdog: Child,
    /// Removed on drop, after the daemon is gone.
    dir: TempDir,
    pub address: String,
}

impl PrivateBus {
    pub fn start() -> Self {
        // A fresh, owner-only directory with a random name: nothing another
        // user created beforehand can stand in for it.
        // (tempfile leaves the mode to the umask unless told.)
        let dir = tempfile::Builder::new()
            .prefix("opendeck-power-profile-test-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let config = dir.path().join("bus.conf");
        // `create_new`: an existing file or symlink here is an error, never
        // followed.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&config)
            .unwrap();
        write!(
            file,
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
            dir.path().display()
        )
        .unwrap();
        drop(file);
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("these tests need `dbus-daemon` on PATH (Debian/Ubuntu/Fedora package: dbus-daemon / dbus)");
        let watchdog = spawn_watchdog(child.id(), dir.path());
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            watchdog,
            dir,
            address: address.trim().to_string(),
        }
    }

    /// The directory the bus lives in (its config and socket).
    pub fn dir(&self) -> &Path {
        self.dir.path()
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
                configure(zbus::connection::Builder::address(address.as_str()))
                    .build()
                    .await
            })
        })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        // The watchdog goes first, so it can never fire at a reused pid.
        let _ = self.watchdog.kill();
        let _ = self.watchdog.wait();
        // Then the daemon; the `TempDir` field is removed after this.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a `sh` that kills `pid` and removes `dir` once its stdin reaches
/// EOF. Only this process holds the write end (std's pipes are
/// close-on-exec), so EOF means this process died: a killed test run then
/// leaves neither a daemon nor its directory behind.
/// (`PR_SET_PDEATHSIG` on the daemon would not do: on Fedora, exec'ing
/// `dbus-daemon` is an SELinux domain transition, which clears it.)
fn spawn_watchdog(pid: u32, dir: &Path) -> Child {
    Command::new("sh")
        .args([
            "-c",
            r#"read _; kill -9 "$1" 2>/dev/null; rm -rf -- "$2""#,
            "private-bus-watchdog",
        ])
        .arg(pid.to_string())
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("these tests need `sh` on PATH")
}

/// The bus name and object path a fake provider exports, as power-profiles-daemon
/// >= 0.20 and tuned-ppd do.
const PROVIDER_NAME: &str = "org.freedesktop.UPower.PowerProfiles";
const PROVIDER_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";

/// A fake hold: `(ApplicationId, Profile)`.
pub type FakeHold = (String, String);

#[derive(Default)]
struct FakeState {
    profile: Mutex<String>,
    /// The `Profile` names `Profiles` lists.
    profiles: Mutex<Vec<String>>,
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
}

fn holds_value(holds: &[FakeHold]) -> Vec<HashMap<String, OwnedVariant>> {
    holds
        .iter()
        .map(|(application_id, profile)| {
            HashMap::from([
                (
                    "ApplicationId".to_string(),
                    OwnedVariant::from(zbus::Str::from(application_id.clone())),
                ),
                (
                    "Profile".to_string(),
                    OwnedVariant::from(zbus::Str::from(profile.clone())),
                ),
                (
                    "Reason".to_string(),
                    OwnedVariant::from(zbus::Str::from("test")),
                ),
            ])
        })
        .collect()
}

async fn emit_changed(state: &FakeState, changed: HashMap<&str, Variant<'_>>) {
    emit_properties_changed(state, changed, &[]).await;
}

async fn emit_properties_changed(
    state: &FakeState,
    changed: HashMap<&str, Variant<'_>>,
    invalidated: &[&str],
) {
    let emitter = SignalEmitter::new(state.conn.get().unwrap(), PROVIDER_PATH).unwrap();
    zbus::fdo::Properties::properties_changed(
        &emitter,
        InterfaceName::try_from(PROVIDER_NAME).unwrap(),
        changed,
        Cow::Borrowed(invalidated),
    )
    .await
    .unwrap();
}

async fn announce(state: &FakeState, profile: &str) {
    emit_changed(
        state,
        HashMap::from([("ActiveProfile", Variant::from(profile))]),
    )
    .await;
}

async fn announce_holds(state: &FakeState) {
    let holds = holds_value(&state.holds.lock().unwrap());
    emit_changed(
        state,
        HashMap::from([("ActiveProfileHolds", Variant::from(holds))]),
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

struct FakePowerProfiles(Arc<FakeState>);

#[zbus::interface(name = "org.freedesktop.UPower.PowerProfiles")]
impl FakePowerProfiles {
    #[zbus(property(emits_changed_signal = "false"))]
    fn active_profile(&self) -> String {
        self.0.profile.lock().unwrap().clone()
    }

    #[zbus(property)]
    async fn set_active_profile(&mut self, value: String) -> zbus::fdo::Result<()> {
        handle_set(&self.0, value).await
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn active_profile_holds(&self) -> zbus::fdo::Result<Vec<HashMap<String, OwnedVariant>>> {
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

    #[zbus(property(emits_changed_signal = "false"))]
    fn profiles(&self) -> Vec<HashMap<String, OwnedVariant>> {
        self.0
            .profiles
            .lock()
            .unwrap()
            .iter()
            .map(|name| {
                HashMap::from([
                    (
                        "Profile".to_string(),
                        OwnedVariant::from(zbus::Str::from(name.clone())),
                    ),
                    (
                        "Driver".to_string(),
                        OwnedVariant::from(zbus::Str::from("fake")),
                    ),
                ])
            })
            .collect()
    }
}

/// A fake power-profiles-daemon / tuned-ppd on a private bus.
pub struct FakeProvider {
    state: Arc<FakeState>,
}

impl FakeProvider {
    /// A provider offering all three profiles, currently on `profile`.
    pub async fn start(bus: &PrivateBus, profile: &str) -> Self {
        Self::start_with_profiles(bus, profile, &["power-saver", "balanced", "performance"]).await
    }

    /// A provider whose `Profiles` lists only `profiles`.
    pub async fn start_with_profiles(bus: &PrivateBus, profile: &str, profiles: &[&str]) -> Self {
        let state = Arc::new(FakeState::default());
        *state.profile.lock().unwrap() = profile.to_string();
        *state.profiles.lock().unwrap() = profiles.iter().map(|p| p.to_string()).collect();
        let conn = zbus::connection::Builder::address(bus.address.as_str())
            .serve_at(PROVIDER_PATH, FakePowerProfiles(state.clone()))
            .build()
            .await
            .unwrap();
        // Replaceable, and replacing: a second fake started while this one
        // still runs takes the name over, as a restarted daemon can.
        conn.request_name_with_flags(
            PROVIDER_NAME,
            RequestNameFlags::ALLOW_REPLACEMENT | RequestNameFlags::REPLACE_EXISTING,
        )
        .await
        .unwrap();
        state.conn.set(conn).unwrap();
        Self { state }
    }

    /// Simulates something other than the plugin (a GUI applet, a terminal
    /// `powerprofilesctl`) changing the profile.
    pub async fn change_externally(&self, profile: &str) {
        *self.state.profile.lock().unwrap() = profile.to_string();
        announce(&self.state, profile).await;
    }

    /// Changes the profile but only says `ActiveProfile` was invalidated,
    /// without sending the new value, so a client has to read it.
    pub async fn change_by_invalidation(&self, profile: &str) {
        *self.state.profile.lock().unwrap() = profile.to_string();
        emit_properties_changed(&self.state, HashMap::new(), &["ActiveProfile"]).await;
    }

    /// Changes the profile but announces `ActiveProfile` as a non-string, so
    /// a client has to read the real value.
    pub async fn change_with_a_malformed_announcement(&self, profile: &str) {
        *self.state.profile.lock().unwrap() = profile.to_string();
        emit_changed(
            &self.state,
            HashMap::from([("ActiveProfile", Variant::from(3u32))]),
        )
        .await;
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
            HashMap::from([("PerformanceDegraded", Variant::from(reason))]),
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

    /// Undoes `reject_sets`.
    pub fn accept_sets(&self) {
        self.state.fail_sets.store(false, Ordering::SeqCst);
    }

    /// The provider process dying: its bus connection closes, so the bus
    /// drops its name and fails any call still waiting on it.
    pub async fn stop(self) {
        let conn = self.state.conn.get().unwrap().clone();
        conn.close().await.unwrap();
    }
}

/// A renderer that records every snapshot the hub pushes.
/// Records every render with the instance IDs tracked at that moment.
pub struct RecordingRenderer {
    instances: Arc<Instances>,
    tx: mpsc::UnboundedSender<(Vec<String>, Snapshot)>,
}

#[async_trait]
impl Renderer for RecordingRenderer {
    async fn render(&self, snapshot: &Snapshot) {
        let _ = self.tx.send((self.instances.ids(), snapshot.clone()));
    }
}

/// Records like `RecordingRenderer`, but panics instead, once, the first
/// time it is asked to render `panic_on`: a stand-in for a bug that kills
/// the watch loop.
pub struct PanicOnceRenderer {
    recording: RecordingRenderer,
    panic_on: ProfileState,
    panicked: AtomicBool,
}

#[async_trait]
impl Renderer for PanicOnceRenderer {
    async fn render(&self, snapshot: &Snapshot) {
        if snapshot.state == self.panic_on && !self.panicked.swap(true, Ordering::SeqCst) {
            panic!("test renderer: deliberate panic on {:?}", self.panic_on);
        }
        self.recording.render(snapshot).await;
    }
}

pub fn panic_once_renderer(
    instances: Arc<Instances>,
    panic_on: ProfileState,
) -> (Arc<PanicOnceRenderer>, Rendered) {
    let (tx, rx) = mpsc::unbounded_channel();
    let renderer = PanicOnceRenderer {
        recording: RecordingRenderer { instances, tx },
        panic_on,
        panicked: AtomicBool::new(false),
    };
    (Arc::new(renderer), Rendered(rx))
}

pub struct Rendered(mpsc::UnboundedReceiver<(Vec<String>, Snapshot)>);

pub fn recording_renderer(instances: Arc<Instances>) -> (Arc<RecordingRenderer>, Rendered) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(RecordingRenderer { instances, tx }), Rendered(rx))
}

/// How long `Rendered::expect_settled` watches for a stray render.
const SETTLE: Duration = Duration::from_millis(200);

impl Rendered {
    /// Waits until the hub renders `want`, failing if it doesn't within `within`.
    pub async fn expect(&mut self, want: ProfileState, within: Duration) {
        self.expect_where(&format!("{want:?}"), within, |s| s.state == want)
            .await;
    }

    /// Waits until the hub renders `want`, and nothing else is rendered
    /// after it for `SETTLE` (no spurious revert or flicker).
    pub async fn expect_settled(&mut self, want: ProfileState, within: Duration) {
        self.expect(want, within).await;
        self.expect_quiet(SETTLE).await;
    }

    /// Waits until the hub renders a snapshot whose status line is `want`.
    pub async fn expect_status(&mut self, want: &str, within: Duration) {
        self.expect_where(&format!("status {want:?}"), within, |s| {
            crate::format::status_text(s, None) == want
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
