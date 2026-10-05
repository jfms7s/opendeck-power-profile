use crate::decision::{ProfileState, Request};
use crate::format::feedback_for_state;
use crate::hub::{ProfileHub, Renderer};
use async_trait::async_trait;
use openaction::{Action, Instance, OpenActionResult};
use serde::{Deserialize, Serialize};

/// The dial has nothing to configure per instance. Still a struct rather
/// than `()`: OpenDeck sends `"settings": {}` with every event, and a map
/// can't deserialize into `()` (openaction would log an ERROR each time).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PowerProfileSettings {}

/// Renders hub state changes onto OpenDeck instances' touch strips.
pub struct OpenDeckRenderer;

#[async_trait]
impl Renderer for OpenDeckRenderer {
    async fn render_all(&self, instance_ids: &[String], state: ProfileState) {
        let feedback = feedback_for_state(state);
        for instance_id in instance_ids {
            let Some(instance) = openaction::get_instance(instance_id.clone()).await else {
                continue; // disappeared since the hub snapshotted its registry
            };
            if let Err(e) = instance.set_feedback(&feedback).await {
                log::warn!("render to {instance_id} failed: {e}");
            }
        }
    }
}

/// The "Power Profile" dial: translates OpenDeck events into hub calls.
pub struct PowerProfileAction {
    hub: ProfileHub,
}

impl PowerProfileAction {
    pub fn new(hub: ProfileHub) -> Self {
        Self { hub }
    }

    async fn request(&self, instance: &Instance, request: Request) -> OpenActionResult<()> {
        if let Err(e) = self.hub.request(request).await {
            log::warn!("{request:?} failed: {e}");
            return instance.show_alert().await;
        }
        Ok(())
    }
}

#[async_trait]
impl Action for PowerProfileAction {
    const UUID: &'static str = "com.jfms7s.powerprofile.dial";
    type Settings = PowerProfileSettings;

    async fn will_appear(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let state = self.hub.track(&instance.instance_id);
        instance.set_feedback(&feedback_for_state(state)).await
    }

    async fn will_disappear(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        self.hub.untrack(&instance.instance_id);
        Ok(())
    }

    async fn dial_rotate(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
        ticks: i16,
        _pressed: bool,
    ) -> OpenActionResult<()> {
        self.request(instance, Request::Step(ticks)).await
    }

    async fn dial_up(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        self.request(instance, Request::Reset).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> serde_json::Value {
        serde_json::from_str(include_str!("../assets/manifest.json")).unwrap()
    }

    #[test]
    fn action_uuid_matches_the_shipped_manifest() {
        let manifest_uuid = manifest()["Actions"][0]["UUID"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(manifest_uuid, <PowerProfileAction as Action>::UUID);
    }

    #[test]
    fn layout_id_belongs_to_this_plugin() {
        let layout: serde_json::Value =
            serde_json::from_str(include_str!("../assets/layouts/power-profile.json")).unwrap();
        let (plugin_uuid, _action) = <PowerProfileAction as Action>::UUID
            .rsplit_once('.')
            .unwrap();
        let layout_id = layout["id"].as_str().unwrap();
        assert!(
            layout_id.starts_with(&format!("{plugin_uuid}.")),
            "{layout_id}"
        );
    }

    /// build.mjs checks this too, but only when packaging; this fails CI first.
    #[test]
    fn crate_version_matches_the_shipped_manifest() {
        assert_eq!(manifest()["Version"], env!("CARGO_PKG_VERSION"));
    }

    /// OpenDeck sends `"settings": {}` with every event; openaction logs an
    /// ERROR for every event whose settings fail to deserialize.
    #[test]
    fn settings_accept_what_opendeck_sends() {
        for sent in [serde_json::json!({}), serde_json::json!({"unexpected": 1})] {
            serde_json::from_value::<<PowerProfileAction as Action>::Settings>(sent.clone())
                .unwrap_or_else(|e| panic!("settings {sent} rejected: {e}"));
        }
    }
}
