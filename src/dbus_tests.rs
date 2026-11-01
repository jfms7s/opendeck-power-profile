//! End-to-end tests of the hub's watch loop and dial requests against a fake
//! `PowerProfiles` provider on a private bus (see `test_support`). They need
//! `dbus-daemon` installed, nothing else - no system bus, no OpenDeck, and the
//! machine's real power profile is never touched.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::cooling::{CoolingSync, OpenLinkHub};
use crate::decision::{Profile, ProfileState, Request};
use crate::hub::{Connector, HubConfig, ProfileHub, ProfileObserver, RequestError};
use crate::test_support::{
    FakeOpenLinkHub, FakeProvider, PrivateBus, Rendered, recording_renderer,
};

const RETRY: Duration = Duration::from_millis(100);
/// The production value: every test here confirms well within it.
const PENDING_TTL: Duration = Duration::from_secs(3);
/// Generous bound for anything event-driven; far below the old 30 s probe.
const WITHIN: Duration = Duration::from_secs(3);

struct Running {
    hub: ProfileHub,
    rendered: Rendered,
    task: JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn start_hub(bus: &PrivateBus, instances: &[&str]) -> Running {
    start_hub_with(bus.connector(), instances)
}

fn start_hub_with(connector: Connector, instances: &[&str]) -> Running {
    start_hub_observed(connector, instances, Vec::new())
}

fn start_hub_observed(
    connector: Connector,
    instances: &[&str],
    observers: Vec<Arc<dyn ProfileObserver>>,
) -> Running {
    start_hub_configured(
        connector,
        instances,
        observers,
        HubConfig {
            retry_interval: RETRY,
            pending_ttl: PENDING_TTL,
        },
    )
}

fn start_hub_configured(
    connector: Connector,
    instances: &[&str],
    observers: Vec<Arc<dyn ProfileObserver>>,
    config: HubConfig,
) -> Running {
    // Hub warnings show up in a failing test's captured output.
    crate::test_support::init_test_logging();
    let (renderer, rendered) = recording_renderer();
    let hub = ProfileHub::new(renderer, observers, config);
    for id in instances {
        hub.track(id);
    }
    let task = tokio::spawn(hub.clone().run(connector));
    Running {
        hub,
        rendered,
        task,
    }
}

const fn known(profile: Profile) -> ProfileState {
    ProfileState::Known(profile)
}

/// The harness's own bus: an owner-only directory with a random name,
/// removed with the bus.
#[test]
fn the_private_bus_lives_in_an_owner_only_dir_removed_on_drop() {
    use std::os::unix::fs::PermissionsExt;

    let bus = PrivateBus::start();
    let dir = bus.dir().to_path_buf();
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
    assert!(dir.join("bus.conf").is_file());
    drop(bus);
    assert!(!dir.exists(), "{} left behind", dir.display());
}

#[tokio::test]
async fn renders_the_providers_profile_on_connect() {
    let bus = PrivateBus::start();
    let _provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;
}

#[tokio::test]
async fn unrecognised_profile_names_render_as_unknown() {
    let bus = PrivateBus::start();
    let _provider = FakeProvider::start(&bus, "eco").await;
    let mut running = start_hub(&bus, &["dial"]);
    running.rendered.expect(ProfileState::Unknown, WITHIN).await;
}

#[tokio::test]
async fn renders_external_changes() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.change_externally("performance").await;
    running
        .rendered
        .expect_settled(known(Profile::Performance), WITHIN)
        .await;
}

/// A provider that dies after connect must be noticed (the owner-loss
/// path). Before 988121e a 30 s probe read zbus's property cache and never
/// noticed; the uncached proxy itself is guarded by
/// `resyncs_on_name_takeover_without_loss`.
#[tokio::test]
async fn renders_unavailable_when_the_provider_goes_away() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.stop().await;
    running
        .rendered
        .expect(ProfileState::Unavailable, WITHIN)
        .await;
}

/// Another process taking the bus name over (without the name ever being
/// free, and without announcing anything) must be picked up by re-reading
/// the profile. Guards the uncached proxy (988121e): with zbus's property
/// cache the re-read would answer the old provider's value.
#[tokio::test]
async fn resyncs_on_name_takeover_without_loss() {
    let bus = PrivateBus::start();
    let _first = FakeProvider::start(&bus, "power-saver").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::PowerSaver), WITHIN)
        .await;

    let _second = FakeProvider::start(&bus, "performance").await;
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;
}

/// A provider that only invalidates `ActiveProfile` (no value) must make the
/// hub read it.
#[tokio::test]
async fn re_reads_when_the_provider_invalidates_the_profile() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.change_by_invalidation("performance").await;
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;
}

/// An `ActiveProfile` announced as a non-string is treated as an
/// invalidation: the hub reads the real value.
#[tokio::test]
async fn re_reads_when_the_provider_announces_a_non_string_profile() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider
        .change_with_a_malformed_announcement("power-saver")
        .await;
    running
        .rendered
        .expect(known(Profile::PowerSaver), WITHIN)
        .await;
}

/// A provider restart (package update, `systemctl restart`) with a different
/// profile must not leave the dial showing - and stepping from - the old one.
#[tokio::test]
async fn resyncs_when_the_provider_restarts_with_a_different_profile() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "power-saver").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::PowerSaver), WITHIN)
        .await;

    provider.stop().await;
    let restarted = FakeProvider::start(&bus, "performance").await;
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;

    // Clockwise from Performance is clamped: nothing may be requested.
    // Stepping from the stale Power Saver would *lower* it to Balanced.
    running.hub.request(Request::Step(1)).await.unwrap();
    assert_eq!(restarted.profile(), "performance");
}

/// After losing the provider the hub reconnects at once, without waiting a
/// retry interval first, so a restart that is already over (or a provider
/// D-Bus activates again) costs no wait.
#[tokio::test]
async fn reconnects_at_once_after_losing_the_provider() {
    // Long enough that waiting it out is unmistakable.
    const SLOW_RETRY: Duration = Duration::from_secs(2);
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    // Hold the reconnect (the second connect) until the new provider is up,
    // so the test doesn't race the hub's first attempt against its start.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let connects = Arc::new(AtomicUsize::new(0));
    let connector: Connector = {
        let inner = bus.connector();
        let gate = gate.clone();
        let connects = connects.clone();
        Arc::new(move || {
            let inner = inner.clone();
            let gate = gate.clone();
            let attempt = connects.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if attempt == 1 {
                    gate.acquire().await.unwrap().forget();
                }
                inner().await
            })
        })
    };
    let mut running = start_hub_configured(
        connector,
        &["dial"],
        Vec::new(),
        HubConfig {
            retry_interval: SLOW_RETRY,
            pending_ttl: PENDING_TTL,
        },
    );
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.stop().await;
    running
        .rendered
        .expect(ProfileState::Unavailable, WITHIN)
        .await;
    let _restarted = FakeProvider::start(&bus, "performance").await;
    gate.add_permits(1);
    running
        .rendered
        .expect(known(Profile::Performance), SLOW_RETRY / 2)
        .await;
}

/// The provider starting after OpenDeck (or coming back after an outage)
/// must be picked up within one retry interval, not after a 30 s probe: at
/// most one retry sleep plus a connect, so within `RETRY * 2` of it starting.
/// `RETRY * 10` only stops a hung hub from hanging the test.
#[tokio::test]
async fn recovers_within_a_retry_interval_when_the_provider_starts_late() {
    let bus = PrivateBus::start();
    let mut running = start_hub(&bus, &["dial"]);
    // Mid-way between two attempts, after the first one failed.
    tokio::time::sleep(RETRY + RETRY / 2).await;

    let _provider = FakeProvider::start(&bus, "balanced").await;
    let started = tokio::time::Instant::now();
    running
        .rendered
        .expect(known(Profile::Balanced), RETRY * 10)
        .await;
    assert!(
        started.elapsed() <= RETRY * 2,
        "recovered after {:?}",
        started.elapsed()
    );
}

/// Two detents before the provider confirms the first one must still move
/// two profiles, not collapse into one.
#[tokio::test]
async fn quick_consecutive_detents_each_take_a_step() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "power-saver").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::PowerSaver), WITHIN)
        .await;

    provider.delay_signals_by(Duration::from_millis(500));
    running.hub.request(Request::Step(1)).await.unwrap();
    running.hub.request(Request::Step(1)).await.unwrap();
    assert_eq!(provider.profile(), "performance");
    running
        .rendered
        .expect_settled(known(Profile::Performance), WITHIN)
        .await;
}

/// Forward, forward, back - with the first step confirmed before the reversal
/// but the second not yet - must land one below the *last request*
/// (Performance - 1 = Balanced), not one below the confirmed profile.
#[tokio::test]
async fn a_quick_reversal_steps_from_the_last_request() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "power-saver").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::PowerSaver), WITHIN)
        .await;

    provider.delay_signals_by(Duration::from_millis(300));
    let start = tokio::time::Instant::now();
    running.hub.request(Request::Step(1)).await.unwrap();
    tokio::time::sleep_until(start + Duration::from_millis(200)).await;
    running.hub.request(Request::Step(1)).await.unwrap();
    // Balanced (announced at ~300 ms) is confirmed; Performance (~500 ms) is not.
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;
    tokio::time::sleep_until(start + Duration::from_millis(350)).await;
    running.hub.request(Request::Step(-1)).await.unwrap();
    assert_eq!(provider.profile(), "balanced");

    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;
    running
        .rendered
        .expect_quiet(Duration::from_millis(500))
        .await;
}

#[tokio::test]
async fn requests_without_a_provider_are_refused() {
    let bus = PrivateBus::start();
    let running = start_hub(&bus, &["dial"]);
    tokio::time::sleep(RETRY * 2).await;
    for request in [Request::Step(1), Request::Reset] {
        assert!(matches!(
            running.hub.request(request).await,
            Err(RequestError::NotConnected)
        ));
    }
}

/// After a loss the hub must drop the dead client, so a detent is refused
/// (and alerts) rather than quietly doing nothing.
#[tokio::test]
async fn requests_after_losing_the_provider_are_refused() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.stop().await;
    running
        .rendered
        .expect(ProfileState::Unavailable, WITHIN)
        .await;
    for request in [Request::Step(1), Request::Reset] {
        assert!(matches!(
            running.hub.request(request).await,
            Err(RequestError::NotConnected)
        ));
    }
}

#[tokio::test]
async fn a_rejected_set_is_reported_and_changes_nothing() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.reject_sets();
    assert!(matches!(
        running.hub.request(Request::Step(1)).await,
        Err(RequestError::Dbus(_))
    ));
    assert_eq!(provider.profile(), "balanced");
    running
        .rendered
        .expect_quiet(Duration::from_millis(300))
        .await;
}

/// A rejected Set must not stay the stepping base: the next rotation steps
/// from the confirmed profile, not from the profile that was never set.
#[tokio::test]
async fn a_failed_set_clears_the_pending_profile() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.reject_sets();
    assert!(matches!(
        running.hub.request(Request::Step(1)).await,
        Err(RequestError::Dbus(_))
    ));
    provider.accept_sets();
    running.hub.request(Request::Step(-1)).await.unwrap();
    // Stepping down from the failed Performance would land on Balanced.
    assert_eq!(provider.profile(), "power-saver");
}

/// An external change while a request is still unconfirmed replaces the
/// stepping base: the next rotation steps from what the provider now says.
#[tokio::test]
async fn an_external_change_after_a_request_resets_the_base() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "power-saver").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::PowerSaver), WITHIN)
        .await;

    // Keep the request for Balanced unconfirmed while the external change lands.
    provider.delay_signals_by(WITHIN);
    running.hub.request(Request::Step(1)).await.unwrap();
    provider.change_externally("performance").await;
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;

    running.hub.request(Request::Step(-1)).await.unwrap();
    // Stepping down from the stale in-flight Balanced would land on Power Saver.
    assert_eq!(provider.profile(), "balanced");
}

/// A provider without `performance` (power-profiles-daemon on hardware
/// without a platform-profile driver): the dial clamps at Balanced and the
/// bolt hint dims, instead of sending a Set the provider rejects.
#[tokio::test]
async fn a_profile_the_provider_does_not_list_is_never_requested() {
    let bus = PrivateBus::start();
    let provider =
        FakeProvider::start_with_profiles(&bus, "balanced", &["power-saver", "balanced"]).await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect_where("Balanced without Performance", WITHIN, |s| {
            s.state == known(Profile::Balanced) && !s.available.contains(Profile::Performance)
        })
        .await;

    running.hub.request(Request::Step(1)).await.unwrap();
    assert_eq!(provider.profile(), "balanced");
    running.hub.request(Request::Step(-1)).await.unwrap();
    assert_eq!(provider.profile(), "power-saver");
}

/// A `Profiles` list naming none of the three is ignored: all three stay
/// reachable, as before the plugin read the list.
#[tokio::test]
async fn a_profiles_list_without_any_known_profile_offers_all_three() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start_with_profiles(&bus, "balanced", &["eco"]).await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    running.hub.request(Request::Step(1)).await.unwrap();
    assert_eq!(provider.profile(), "performance");
}

#[tokio::test]
async fn pressing_resets_to_balanced() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "performance").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;

    running.hub.request(Request::Reset).await.unwrap();
    assert_eq!(provider.profile(), "balanced");
    running
        .rendered
        .expect_settled(known(Profile::Balanced), WITHIN)
        .await;
}

/// A Set that hangs (the provider stuck in polkit, say) must not hold up
/// noticing that the provider went away.
#[tokio::test]
async fn a_hung_set_does_not_delay_noticing_the_provider_is_gone() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.delay_sets_by(Duration::from_secs(10));
    let hub = running.hub.clone();
    let _hung = tokio::spawn(async move { hub.request(Request::Step(1)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    provider.stop().await;
    running
        .rendered
        .expect(ProfileState::Unavailable, Duration::from_secs(1))
        .await;
}

/// A Set that outlasts the connection's method timeout fails like any other
/// failed Set, so the dial gets its alert and the next detent steps from
/// what the provider confirmed rather than from the abandoned target.
#[tokio::test]
async fn a_set_that_outlasts_the_method_timeout_fails() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub_with(
        bus.connector_with_timeout(Duration::from_millis(300)),
        &["dial"],
    );
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    let stall = Duration::from_secs(2);
    provider.delay_sets_by(stall);
    let stalled_at = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        running.hub.request(Request::Step(1)),
    )
    .await
    .expect("the Set was not bounded by the method timeout");
    assert!(
        matches!(result, Err(RequestError::Dbus(_))),
        "expected a D-Bus error, got {result:?}"
    );
    assert_eq!(provider.profile(), "balanced");

    // The provider still applies the abandoned Set once its stall ends; the
    // watch loop renders that, and the next detent steps from it.
    provider.delay_sets_by(Duration::ZERO);
    tokio::time::sleep_until(stalled_at + stall).await;
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;
    running.hub.request(Request::Step(-1)).await.unwrap();
    assert_eq!(provider.profile(), "balanced");
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;
}

#[tokio::test]
async fn renders_only_to_tracked_instances() {
    let bus = PrivateBus::start();
    let running_hub_instances = ["a", "b"];
    let mut running = start_hub(&bus, &running_hub_instances);
    running.hub.untrack("b");
    let _provider = FakeProvider::start(&bus, "balanced").await;

    let (ids, snapshot) = running.rendered.next(WITHIN).await;
    assert_eq!(snapshot.state, known(Profile::Balanced));
    assert_eq!(ids, vec!["a".to_string()]);
}

#[tokio::test]
async fn an_unchanged_profile_is_not_re_rendered() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.change_externally("balanced").await;
    running
        .rendered
        .expect_quiet(Duration::from_millis(300))
        .await;
}

#[tokio::test]
async fn renders_the_holder_while_a_hold_is_active() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.set_holds(&[("gamemode", "balanced")]).await;
    running
        .rendered
        .expect_status("Held by gamemode", WITHIN)
        .await;
}

#[tokio::test]
async fn renders_a_hold_that_existed_before_connecting() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "performance").await;
    provider.preset_holds(&[("gamemode", "performance")]);
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect_where("held Performance", WITHIN, |s| {
            s.state == known(Profile::Performance)
                && crate::format::status_text(s) == "Held by gamemode"
        })
        .await;
}

#[tokio::test]
async fn clears_the_holder_when_the_hold_is_released() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    provider.preset_holds(&[("gamemode", "balanced")]);
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect_status("Held by gamemode", WITHIN)
        .await;

    provider.set_holds(&[]).await;
    let (_, snapshot) = running.rendered.next(WITHIN).await;
    assert_eq!(snapshot.state, known(Profile::Balanced));
    assert!(snapshot.conditions.holds.is_empty(), "{snapshot:?}");
}

#[tokio::test]
async fn a_step_during_a_hold_still_requests_the_next_profile() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    provider.preset_holds(&[("gamemode", "balanced")]);
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect_status("Held by gamemode", WITHIN)
        .await;

    running.hub.request(Request::Step(1)).await.unwrap();
    assert_eq!(provider.profile(), "performance");
    // The provider releases the hold on a Set; the line clears.
    assert!(provider.holds().is_empty());
    running
        .rendered
        .expect_where("Performance, no hold", WITHIN, |s| {
            s.state == known(Profile::Performance) && s.conditions.holds.is_empty()
        })
        .await;
}

#[tokio::test]
async fn renders_the_degraded_reason_without_a_hold() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "performance").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;

    provider.set_degraded("high-operating-temperature").await;
    running
        .rendered
        .expect_status("Limited: too hot", WITHIN)
        .await;
}

/// A provider without `ActiveProfileHolds`/`PerformanceDegraded` (or one
/// whose reads fail) still shows its profile, with no status line.
#[tokio::test]
async fn failing_condition_reads_never_break_the_profile_display() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "power-saver").await;
    provider.fail_condition_reads();
    provider.preset_holds(&[("gamemode", "power-saver")]);
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect_where("plain Power Saver", WITHIN, |s| {
            s.state == known(Profile::PowerSaver) && s.conditions.holds.is_empty()
        })
        .await;
}

#[tokio::test]
async fn losing_the_provider_clears_its_holds() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, "balanced").await;
    provider.preset_holds(&[("gamemode", "balanced")]);
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect_status("Held by gamemode", WITHIN)
        .await;

    provider.stop().await;
    let (_, snapshot) = running.rendered.next(WITHIN).await;
    assert_eq!(
        snapshot,
        crate::decision::Snapshot::plain(ProfileState::Unavailable)
    );
}

const OPENLINKHUB_FIXTURE: &str = include_str!("../tests/fixtures/openlinkhub-api.json");

fn cooling(server: &FakeOpenLinkHub) -> Vec<Arc<dyn ProfileObserver>> {
    vec![Arc::new(CoolingSync::start(server.client()))]
}

/// The cooling sync writes on a real change, and never for the seed render
/// after connecting or after the provider comes back.
#[tokio::test]
async fn cooling_follows_changes_but_not_connects_or_reconnects() {
    let bus = PrivateBus::start();
    let server = FakeOpenLinkHub::start(OPENLINKHUB_FIXTURE).await;
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub_observed(bus.connector(), &["dial"], cooling(&server));
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.change_externally("performance").await;
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;
    let posts = server.wait_for_posts(1, WITHIN).await;
    assert!(posts[0].contains("\"Performance\""), "{posts:?}");

    // The provider restarts on another profile: Unavailable, then the seed.
    provider.stop().await;
    running
        .rendered
        .expect(ProfileState::Unavailable, WITHIN)
        .await;
    let _provider = FakeProvider::start(&bus, "power-saver").await;
    running
        .rendered
        .expect(known(Profile::PowerSaver), WITHIN)
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.posts().len(), 1, "{:?}", server.posts());
}

/// An OpenLinkHub that accepts but never answers must not slow the dial.
#[tokio::test]
async fn a_hung_openlinkhub_never_delays_rendering() {
    let bus = PrivateBus::start();
    let server = FakeOpenLinkHub::silent().await;
    let observers: Vec<Arc<dyn ProfileObserver>> = vec![Arc::new(CoolingSync::start(
        OpenLinkHub::new(server.addr(), Duration::from_secs(30)),
    ))];
    let provider = FakeProvider::start(&bus, "balanced").await;
    let mut running = start_hub_observed(bus.connector(), &["dial"], observers);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    for (profile, state) in [
        ("performance", known(Profile::Performance)),
        ("power-saver", known(Profile::PowerSaver)),
        ("balanced", known(Profile::Balanced)),
    ] {
        provider.change_externally(profile).await;
        running
            .rendered
            .expect(state, Duration::from_millis(500))
            .await;
    }
    running.hub.request(Request::Step(1)).await.unwrap();
    assert_eq!(provider.profile(), "performance");
}
