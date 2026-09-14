use crate::decision::{ProfileState, RESET_PROFILE, step_target};
use crate::format::feedback_for_state;
use crate::power_profiles::PowerProfilesClient;
use async_trait::async_trait;
use dashmap::DashSet;
use openaction::{Action, Instance, OpenActionResult};
use std::sync::Arc;
use tokio::sync::RwLock;

struct SharedState {
    client: RwLock<Option<PowerProfilesClient>>,
    last_known: RwLock<ProfileState>,
    registry: DashSet<String>,
}

#[derive(Clone)]
pub struct PowerProfileAction {
    shared: Arc<SharedState>,
}

impl PowerProfileAction {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(SharedState {
                client: RwLock::new(None),
                last_known: RwLock::new(ProfileState::Unavailable),
                registry: DashSet::new(),
            }),
        }
    }

    fn track(&self, instance_id: &str) {
        self.shared.registry.insert(instance_id.to_string());
    }

    fn untrack(&self, instance_id: &str) {
        self.shared.registry.remove(instance_id);
    }

    async fn render_cached(&self, instance: &Instance) -> OpenActionResult<()> {
        let state = *self.shared.last_known.read().await;
        instance.set_feedback(&feedback_for_state(state)).await
    }

    /// Sets `last_known` and pushes fresh feedback to every tracked
    /// instance - the single path the background watch/reconnect task
    /// (Task 6) uses, so a self- or externally-triggered profile change can
    /// never render differently or race.
    pub async fn set_state_and_render_all(&self, state: ProfileState) {
        *self.shared.last_known.write().await = state;
        let feedback = feedback_for_state(state);

        // Collect ids into a Vec first, releasing the DashSet shard lock
        // before awaiting `get_instance`/`set_feedback` per entry - holding
        // an iterator guard across an await point would keep that shard
        // locked for the whole loop.
        let instance_ids: Vec<String> = self
            .shared
            .registry
            .iter()
            .map(|e| e.key().clone())
            .collect();

        for instance_id in instance_ids {
            let Some(instance) = openaction::get_instance(instance_id).await else {
                continue; // instance disappeared between the snapshot and now
            };
            if let Err(e) = instance.set_feedback(&feedback).await {
                log::warn!("render failed: {e}");
            }
        }
    }

    pub async fn set_client(&self, client: Option<PowerProfilesClient>) {
        *self.shared.client.write().await = client;
    }

    /// Shared by `dial_rotate` and `dial_up`: reads the current client and
    /// last-known state, computes the target profile via `target_fn`, and
    /// requests it. Never renders itself - the watch stream in `main.rs`
    /// (Task 6) does that once the change is confirmed over D-Bus.
    async fn apply(
        &self,
        instance: &Instance,
        target_fn: impl FnOnce(ProfileState) -> &'static str,
    ) -> OpenActionResult<()> {
        let client_guard = self.shared.client.read().await;
        let Some(client) = client_guard.as_ref() else {
            return instance.show_alert().await;
        };
        let state = *self.shared.last_known.read().await;
        let target = target_fn(state);
        if let Err(e) = client.set_active_profile(target).await {
            log::warn!("set_active_profile({target}) failed: {e}");
            return instance.show_alert().await;
        }
        Ok(())
    }
}

#[async_trait]
impl Action for PowerProfileAction {
    const UUID: &'static str = "com.jfms7s.powerprofile.dial";
    type Settings = ();

    async fn will_appear(&self, instance: &Instance, _settings: &()) -> OpenActionResult<()> {
        self.track(&instance.instance_id);
        self.render_cached(instance).await
    }

    async fn will_disappear(&self, instance: &Instance, _settings: &()) -> OpenActionResult<()> {
        self.untrack(&instance.instance_id);
        Ok(())
    }

    async fn dial_rotate(
        &self,
        instance: &Instance,
        _settings: &(),
        ticks: i16,
        _pressed: bool,
    ) -> OpenActionResult<()> {
        self.apply(instance, |state| step_target(state, ticks))
            .await
    }

    async fn dial_up(&self, instance: &Instance, _settings: &()) -> OpenActionResult<()> {
        self.apply(instance, |_state| RESET_PROFILE).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_uuid_matches_the_shipped_manifest() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../assets/manifest.json")).unwrap();
        let manifest_uuid = manifest["Actions"][0]["UUID"].as_str().unwrap();
        assert_eq!(manifest_uuid, <PowerProfileAction as Action>::UUID);
    }

    #[test]
    fn track_then_untrack_round_trips_through_the_registry() {
        let action = PowerProfileAction::new();
        action.track("ctx1");
        assert!(action.shared.registry.contains("ctx1"));

        action.untrack("ctx1");
        assert!(!action.shared.registry.contains("ctx1"));
    }

    #[test]
    fn tracking_the_same_instance_twice_is_idempotent() {
        let action = PowerProfileAction::new();
        action.track("ctx1");
        action.track("ctx1");
        assert_eq!(action.shared.registry.len(), 1);
    }
}
