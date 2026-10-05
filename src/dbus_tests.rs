//! End-to-end tests of the hub's watch loop and dial requests against a fake
//! `PowerProfiles` provider on a private bus (see `test_support`). They need
//! `dbus-daemon` installed, nothing else - no system bus, no OpenDeck, and the
//! machine's real power profile is never touched.

use std::time::Duration;

use tokio::task::JoinHandle;

use crate::decision::{Profile, ProfileState, Request};
use crate::hub::{HubConfig, ProfileHub, RequestError};
use crate::test_support::{FakeProvider, Flavor, PrivateBus, Rendered, recording_renderer};

const RETRY: Duration = Duration::from_millis(100);
/// Generous bound for anything event-driven; far below the old 30 s probe.
const WITHIN: Duration = Duration::from_secs(3);
const FLAVORS: [Flavor; 2] = [Flavor::Hadess, Flavor::UPower];

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
    // Hub warnings show up in a failing test's captured output.
    crate::test_support::init_test_logging();
    let (renderer, rendered) = recording_renderer();
    let hub = ProfileHub::new(
        renderer,
        HubConfig {
            retry_interval: RETRY,
        },
    );
    for id in instances {
        hub.track(id);
    }
    let task = tokio::spawn(hub.clone().run(bus.connector()));
    Running {
        hub,
        rendered,
        task,
    }
}

const fn known(profile: Profile) -> ProfileState {
    ProfileState::Known(profile)
}

#[tokio::test]
async fn renders_the_providers_profile_on_connect() {
    for flavor in FLAVORS {
        let bus = PrivateBus::start();
        let _provider = FakeProvider::start(&bus, flavor, "balanced").await;
        let mut running = start_hub(&bus, &["dial"]);
        running
            .rendered
            .expect(known(Profile::Balanced), WITHIN)
            .await;
    }
}

#[tokio::test]
async fn unrecognised_profile_names_render_as_unknown() {
    let bus = PrivateBus::start();
    let _provider = FakeProvider::start(&bus, Flavor::UPower, "eco").await;
    let mut running = start_hub(&bus, &["dial"]);
    running.rendered.expect(ProfileState::Unknown, WITHIN).await;
}

#[tokio::test]
async fn renders_external_changes() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, Flavor::UPower, "balanced").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;

    provider.change_externally("performance").await;
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;
}

/// Regression test for 988121e: the 30 s "liveness probe" read zbus's
/// property cache, so a provider that died after connect was never noticed.
#[tokio::test]
async fn renders_unavailable_when_the_provider_goes_away() {
    for flavor in FLAVORS {
        let bus = PrivateBus::start();
        let provider = FakeProvider::start(&bus, flavor, "balanced").await;
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
}

/// A provider restart (package update, `systemctl restart`) with a different
/// profile must not leave the dial showing - and stepping from - the old one.
#[tokio::test]
async fn resyncs_when_the_provider_restarts_with_a_different_profile() {
    for flavor in FLAVORS {
        let bus = PrivateBus::start();
        let provider = FakeProvider::start(&bus, flavor, "power-saver").await;
        let mut running = start_hub(&bus, &["dial"]);
        running
            .rendered
            .expect(known(Profile::PowerSaver), WITHIN)
            .await;

        provider.stop().await;
        let restarted = FakeProvider::start(&bus, flavor, "performance").await;
        running
            .rendered
            .expect(known(Profile::Performance), WITHIN)
            .await;

        // Clockwise from Performance is clamped: nothing may be requested.
        // Stepping from the stale Power Saver would *lower* it to Balanced.
        running.hub.request(Request::Step(1)).await.unwrap();
        assert_eq!(restarted.profile(), "performance", "{flavor:?}");
    }
}

/// The provider starting after OpenDeck (or coming back after an outage)
/// must be picked up within one retry interval, not after a 30 s probe.
#[tokio::test]
async fn recovers_within_a_retry_interval_when_the_provider_starts_late() {
    for flavor in FLAVORS {
        let bus = PrivateBus::start();
        let mut running = start_hub(&bus, &["dial"]);
        tokio::time::sleep(RETRY * 3).await;

        let _provider = FakeProvider::start(&bus, flavor, "balanced").await;
        running
            .rendered
            .expect(known(Profile::Balanced), RETRY * 10)
            .await;
    }
}

/// Two detents before the provider confirms the first one must still move
/// two profiles, not collapse into one.
#[tokio::test]
async fn quick_consecutive_detents_each_take_a_step() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, Flavor::UPower, "power-saver").await;
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
        .expect(known(Profile::Performance), WITHIN)
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

#[tokio::test]
async fn a_rejected_set_is_reported_and_changes_nothing() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, Flavor::UPower, "balanced").await;
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

#[tokio::test]
async fn pressing_resets_to_balanced() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, Flavor::UPower, "performance").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;

    running.hub.request(Request::Reset).await.unwrap();
    assert_eq!(provider.profile(), "balanced");
    running
        .rendered
        .expect(known(Profile::Balanced), WITHIN)
        .await;
}

/// A Set that hangs (the provider stuck in polkit, say) must not hold up
/// noticing that the provider went away.
#[tokio::test]
async fn a_hung_set_does_not_delay_noticing_the_provider_is_gone() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, Flavor::UPower, "balanced").await;
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

#[tokio::test]
async fn prefers_the_upower_name_when_both_are_exported() {
    let bus = PrivateBus::start();
    let _legacy = FakeProvider::start(&bus, Flavor::Hadess, "power-saver").await;
    let _current = FakeProvider::start(&bus, Flavor::UPower, "performance").await;
    let mut running = start_hub(&bus, &["dial"]);
    running
        .rendered
        .expect(known(Profile::Performance), WITHIN)
        .await;
}

#[tokio::test]
async fn renders_only_to_tracked_instances() {
    let bus = PrivateBus::start();
    let running_hub_instances = ["a", "b"];
    let mut running = start_hub(&bus, &running_hub_instances);
    running.hub.untrack("b");
    let _provider = FakeProvider::start(&bus, Flavor::UPower, "balanced").await;

    let (ids, state) = running.rendered.next(WITHIN).await;
    assert_eq!(state, known(Profile::Balanced));
    assert_eq!(ids, vec!["a".to_string()]);
}

#[tokio::test]
async fn an_unchanged_profile_is_not_re_rendered() {
    let bus = PrivateBus::start();
    let provider = FakeProvider::start(&bus, Flavor::UPower, "balanced").await;
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
