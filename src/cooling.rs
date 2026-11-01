//! Cooling follows the profile: after the provider confirms a change from one
//! known profile to another, switch OpenLinkHub's fan channels to the
//! matching speed profile. Best effort and never in the dial's way: the hub
//! only hands the target to a worker task, which talks to OpenLinkHub on
//! loopback with a short timeout and logs what happened.
//!
//! The pure part (`fan_profile_for`, `sync_target`, `channels_to_write`) is
//! unit-tested; the I/O part is a minimal HTTP/1.0 client, tested against a
//! local fake server.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::decision::{Profile, ProfileState, Snapshot};
use crate::hub::ProfileObserver;

/// Where OpenLinkHub serves its API (its default; it only listens on loopback).
pub const OPENLINKHUB_ADDR: &str = "127.0.0.1:27003";

/// Bounds each request (connect, write and read together).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// The most of a response body this reads; `GET /api/` is about 25 KiB.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// The OpenLinkHub speed profile each power profile maps to.
pub fn fan_profile_for(profile: Profile) -> &'static str {
    match profile {
        Profile::PowerSaver => "Quiet",
        Profile::Balanced => "Normal",
        Profile::Performance => "Performance",
    }
}

/// Whether a channel's current speed profile is one this sync manages. A
/// channel on any other (custom) curve is left alone.
fn is_managed(fan_profile: &str) -> bool {
    Profile::ALL
        .into_iter()
        .any(|p| fan_profile_for(p) == fan_profile)
}

/// The profile to sync the fans to on a change from `previous` to `current`:
/// only a change between two different known profiles. Never the first known
/// state after "Unavailable" (startup, reconnect) and never `Unknown`.
pub fn sync_target(previous: ProfileState, current: ProfileState) -> Option<Profile> {
    match (previous, current) {
        (ProfileState::Known(a), ProfileState::Known(b)) if a != b => Some(b),
        _ => None,
    }
}

/// One fan channel to switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    /// The device's key in `GET /api/`'s `device` map (its serial), which
    /// `POST /api/speed` takes as `deviceId`.
    pub device_id: String,
    pub channel_id: i64,
    pub name: String,
}

/// The channels of a `GET /api/` response that should switch to
/// `fan_profile`: ones with a speed whose current profile is managed and not
/// already `fan_profile`. Anything missing or oddly typed is skipped.
pub fn channels_to_write(api: &Value, fan_profile: &str) -> Vec<Channel> {
    let Some(devices) = api.get("device").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut channels = Vec::new();
    for (device_id, device) in devices {
        let Some(device_channels) = device
            .pointer("/GetDevice/devices")
            .and_then(Value::as_object)
        else {
            continue;
        };
        for channel in device_channels.values() {
            let has_speed = channel.get("HasSpeed").and_then(Value::as_bool) == Some(true);
            let is_probe = channel.get("IsTemperatureProbe").and_then(Value::as_bool) == Some(true);
            let current = channel.get("profile").and_then(Value::as_str).unwrap_or("");
            let Some(channel_id) = channel.get("channelId").and_then(Value::as_i64) else {
                continue;
            };
            if has_speed && !is_probe && is_managed(current) && current != fan_profile {
                channels.push(Channel {
                    device_id: device_id.clone(),
                    channel_id,
                    name: channel
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
    }
    channels.sort_by(|a, b| (&a.device_id, a.channel_id).cmp(&(&b.device_id, b.channel_id)));
    channels
}

#[derive(Debug, Error)]
pub enum CoolingError {
    #[error("OpenLinkHub is not reachable at {addr}: {source}")]
    Unreachable {
        addr: String,
        source: std::io::Error,
    },
    #[error("OpenLinkHub did not answer within {0:?}")]
    Timeout(Duration),
    #[error("OpenLinkHub sent an unexpected response: {0}")]
    BadResponse(String),
    #[error("OpenLinkHub refused to set {channel}: {message}")]
    Refused { channel: String, message: String },
}

impl CoolingError {
    /// Groups errors for "log once per kind".
    fn kind(&self) -> &'static str {
        match self {
            CoolingError::Unreachable { .. } => "unreachable",
            CoolingError::Timeout(_) => "timeout",
            CoolingError::BadResponse(_) => "bad response",
            CoolingError::Refused { .. } => "refused",
        }
    }
}

/// A minimal OpenLinkHub API client: one HTTP/1.0 request per connection,
/// so the server ends the body by closing (no chunked decoding needed).
#[derive(Debug, Clone)]
pub struct OpenLinkHub {
    addr: String,
    timeout: Duration,
}

impl OpenLinkHub {
    pub fn new(addr: impl Into<String>, timeout: Duration) -> Self {
        Self {
            addr: addr.into(),
            timeout,
        }
    }

    async fn request(&self, method: &str, path: &str, body: &str) -> Result<Value, CoolingError> {
        tokio::time::timeout(self.timeout, self.exchange(method, path, body))
            .await
            .map_err(|_| CoolingError::Timeout(self.timeout))?
    }

    async fn exchange(&self, method: &str, path: &str, body: &str) -> Result<Value, CoolingError> {
        let unreachable = |source| CoolingError::Unreachable {
            addr: self.addr.clone(),
            source,
        };
        let mut stream = TcpStream::connect(&self.addr).await.map_err(unreachable)?;
        let request = format!(
            "{method} {path} HTTP/1.0\r\nHost: {}\r\nAccept: application/json\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(unreachable)?;
        let mut response = Vec::new();
        (&mut stream)
            .take(MAX_RESPONSE_BYTES)
            .read_to_end(&mut response)
            .await
            .map_err(unreachable)?;
        parse_response(&response)
    }

    /// `GET /api/`: every device with its channels and their speed profiles.
    pub async fn devices(&self) -> Result<Value, CoolingError> {
        self.request("GET", "/api/", "").await
    }

    /// `POST /api/speed`: switch one channel's speed profile.
    pub async fn set_speed(
        &self,
        channel: &Channel,
        fan_profile: &str,
    ) -> Result<(), CoolingError> {
        let body = json!({
            "deviceId": channel.device_id,
            "channelId": channel.channel_id,
            "profile": fan_profile,
        })
        .to_string();
        let response = self.request("POST", "/api/speed", &body).await?;
        if response.get("status").and_then(Value::as_i64) == Some(1) {
            return Ok(());
        }
        Err(CoolingError::Refused {
            channel: format!("{}/{}", channel.device_id, channel.channel_id),
            message: response
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message")
                .to_string(),
        })
    }
}

/// Splits an HTTP/1.x response and parses its body as JSON; anything but a
/// 200 is an error.
fn parse_response(raw: &[u8]) -> Result<Value, CoolingError> {
    let bad = |why: String| CoolingError::BadResponse(why);
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("no end of headers".into()))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status_line = head.lines().next().unwrap_or("");
    let status = status_line.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        return Err(bad(format!("HTTP status {status_line:?}")));
    }
    if head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:"))
    {
        return Err(bad("a transfer-encoded body".into()));
    }
    serde_json::from_slice(&raw[split + 4..]).map_err(|e| bad(format!("body is not JSON: {e}")))
}

/// Remembers the last failure kind, so a repeating one logs at debug.
#[derive(Default)]
struct FailureLog(Mutex<Option<&'static str>>);

impl FailureLog {
    fn failed(&self, error: &CoolingError) {
        let kind = error.kind();
        let mut last = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if *last == Some(kind) {
            log::debug!("cooling sync: {error}");
        } else {
            log::warn!("cooling sync: {error}");
            *last = Some(kind);
        }
    }

    fn succeeded(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Switches the managed channels to `profile`'s fan profile.
async fn sync_once(client: &OpenLinkHub, profile: Profile, failures: &FailureLog) {
    let fan_profile = fan_profile_for(profile);
    let api = match client.devices().await {
        Ok(api) => api,
        Err(e) => return failures.failed(&e),
    };
    let channels = channels_to_write(&api, fan_profile);
    if channels.is_empty() {
        log::debug!("cooling sync: no channel to switch to {fan_profile}");
    }
    for channel in &channels {
        match client.set_speed(channel, fan_profile).await {
            Ok(()) => {
                log::info!(
                    "cooling sync: {} channel {} ({}) -> {fan_profile}",
                    channel.device_id,
                    channel.channel_id,
                    channel.name
                );
                failures.succeeded();
            }
            Err(e) => failures.failed(&e),
        }
    }
    if channels.is_empty() {
        failures.succeeded();
    }
}

/// The hub observer: hands each sync target to one worker task, so the hub
/// never waits on HTTP, writes never overlap, and a burst of changes
/// collapses to the latest profile.
pub struct CoolingSync {
    targets: watch::Sender<Option<Profile>>,
}

impl CoolingSync {
    /// Starts the worker on the current tokio runtime.
    pub fn start(client: OpenLinkHub) -> Self {
        let (targets, mut receiver) = watch::channel(None);
        tokio::spawn(async move {
            let failures = FailureLog::default();
            while receiver.changed().await.is_ok() {
                let target = *receiver.borrow_and_update();
                if let Some(profile) = target {
                    sync_once(&client, profile, &failures).await;
                }
            }
        });
        Self { targets }
    }
}

impl ProfileObserver for CoolingSync {
    fn changed(&self, previous: &Snapshot, current: &Snapshot) {
        if let Some(profile) = sync_target(previous.state, current.state) {
            self.targets.send_replace(Some(profile));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeOpenLinkHub;
    use Profile::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/openlinkhub-api.json");

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).unwrap()
    }

    fn fan_1() -> Channel {
        Channel {
            device_id: "SERIAL1".into(),
            channel_id: 0,
            name: "Fan 1".into(),
        }
    }

    #[test]
    fn every_profile_maps_to_a_distinct_fan_profile() {
        assert_eq!(fan_profile_for(PowerSaver), "Quiet");
        assert_eq!(fan_profile_for(Balanced), "Normal");
        assert_eq!(fan_profile_for(Performance), "Performance");
    }

    #[test]
    fn only_a_change_between_two_known_profiles_syncs() {
        let k = ProfileState::Known;
        assert_eq!(sync_target(k(Balanced), k(Performance)), Some(Performance));
        assert_eq!(sync_target(k(Performance), k(PowerSaver)), Some(PowerSaver));
        assert_eq!(sync_target(k(Balanced), k(Balanced)), None);
        for (previous, current) in [
            (ProfileState::Unavailable, k(Balanced)),
            (ProfileState::Unknown, k(Balanced)),
            (k(Balanced), ProfileState::Unknown),
            (k(Balanced), ProfileState::Unavailable),
        ] {
            assert_eq!(
                sync_target(previous, current),
                None,
                "{previous:?} -> {current:?}"
            );
        }
    }

    #[test]
    fn the_recorded_hub_selects_its_fan_for_another_profile() {
        assert_eq!(channels_to_write(&fixture(), "Performance"), vec![fan_1()]);
        assert_eq!(channels_to_write(&fixture(), "Normal"), vec![fan_1()]);
        // Already on Quiet: nothing to write.
        assert!(channels_to_write(&fixture(), "Quiet").is_empty());
    }

    #[test]
    fn custom_curves_probes_and_speedless_channels_are_left_alone() {
        let api = json!({"device": {"D": {"GetDevice": {"devices": {
            "0": {"channelId": 0, "profile": "My curve", "HasSpeed": true},
            "1": {"channelId": 1, "profile": "Quiet", "HasSpeed": true, "IsTemperatureProbe": true},
            "2": {"channelId": 2, "profile": "Quiet", "HasSpeed": false},
            "3": {"channelId": 3, "profile": "Quiet"},
            "4": {"profile": "Quiet", "HasSpeed": true},
            "5": {"channelId": 5, "profile": "Normal", "HasSpeed": true, "name": "Pump"}
        }}}, "cluster": {"GetDevice": null}}});
        assert_eq!(
            channels_to_write(&api, "Performance"),
            vec![Channel {
                device_id: "D".into(),
                channel_id: 5,
                name: "Pump".into()
            }]
        );
    }

    #[test]
    fn a_response_without_matching_profile_names_selects_nothing() {
        let mut api = fixture();
        api["device"]["SERIAL1"]["GetDevice"]["devices"]["0"]["profile"] = json!("Silent");
        assert!(channels_to_write(&api, "Performance").is_empty());
        assert!(channels_to_write(&json!({"status": 0}), "Performance").is_empty());
        assert!(channels_to_write(&json!([]), "Performance").is_empty());
    }

    #[test]
    fn responses_are_parsed_and_checked() {
        let ok = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":1}";
        assert_eq!(parse_response(ok).unwrap(), json!({"status": 1}));
        for bad in [
            &b"HTTP/1.0 404 Not Found\r\n\r\n{}"[..],
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
            b"HTTP/1.0 200 OK\r\n\r\n<html>",
            b"HTTP/1.0 200 OK",
        ] {
            assert!(
                matches!(parse_response(bad), Err(CoolingError::BadResponse(_))),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[tokio::test]
    async fn a_change_posts_the_mapped_profile_to_each_selected_channel() {
        let server = FakeOpenLinkHub::start(FIXTURE).await;
        let sync = CoolingSync::start(server.client());
        let k = |p| Snapshot::plain(ProfileState::Known(p));
        sync.changed(&k(Balanced), &k(Performance));

        let posts = server.wait_for_posts(1, Duration::from_secs(3)).await;
        assert_eq!(posts.len(), 1);
        let body: Value = serde_json::from_str(&posts[0]).unwrap();
        assert_eq!(
            body,
            json!({"deviceId": "SERIAL1", "channelId": 0, "profile": "Performance"})
        );
    }

    #[tokio::test]
    async fn startup_reconnect_and_unknown_never_post() {
        let server = FakeOpenLinkHub::start(FIXTURE).await;
        let sync = CoolingSync::start(server.client());
        let plain = Snapshot::plain;
        sync.changed(
            &plain(ProfileState::Unavailable),
            &plain(ProfileState::Known(Performance)),
        );
        sync.changed(
            &plain(ProfileState::Known(Performance)),
            &plain(ProfileState::Unknown),
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(server.requests(), 0);
    }

    #[tokio::test]
    async fn a_silent_server_times_out() {
        let server = FakeOpenLinkHub::silent().await;
        let client = OpenLinkHub::new(server.addr(), Duration::from_millis(200));
        let started = tokio::time::Instant::now();
        assert!(matches!(
            client.devices().await,
            Err(CoolingError::Timeout(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_refused_write_is_an_error() {
        let server = FakeOpenLinkHub::start(FIXTURE).await;
        server.refuse_writes();
        let result = server.client().set_speed(&fan_1(), "Performance").await;
        assert!(
            matches!(result, Err(CoolingError::Refused { .. })),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn nothing_listening_is_unreachable() {
        let addr = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().to_string()
        };
        let client = OpenLinkHub::new(addr, Duration::from_secs(1));
        assert!(matches!(
            client.devices().await,
            Err(CoolingError::Unreachable { .. })
        ));
    }
}
