use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::decision::{Available, Conditions, ProfileState, Request, Snapshot, is_long_press};
use crate::format::{feedback_for, status_feedback, tile_image_for, title_for};
use crate::hub::{ProfileHub, Renderer, RequestError};
use crate::instances::Instances;
use crate::telemetry::{Telemetry, TelemetrySink};
use async_trait::async_trait;
use openaction::{Action, Instance, OpenActionResult};
use serde::{Deserialize, Serialize};

/// The dial has nothing to configure per instance. Still a struct rather
/// than `()`: OpenDeck sends `"settings": {}` with every event, and a map
/// can't deserialize into `()` (openaction would log an ERROR each time).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PowerProfileSettings {}

fn is_key(instance: &Instance) -> bool {
    instance.controller == "Keypad"
}

/// Draws `snapshot` on one instance. The only place a whole instance is
/// drawn: the renderer calls it for every change, `will_appear` for a new
/// instance. A dial gets the touch-strip layout (with the CPU temperature,
/// `telemetry`, on its status line); a key gets the profile's glyph as its
/// image and the profile's name as its title.
async fn draw(
    instance: &Instance,
    snapshot: &Snapshot,
    telemetry: Option<&str>,
) -> OpenActionResult<()> {
    if is_key(instance) {
        instance
            .set_image(Some(tile_image_for(snapshot)), None)
            .await?;
        instance.set_title(Some(title_for(snapshot)), None).await
    } else {
        instance
            .set_feedback(&feedback_for(snapshot, telemetry))
            .await
    }
}

/// Renders hub state changes, and new temperature readings, onto every
/// tracked OpenDeck instance.
pub struct OpenDeckRenderer {
    instances: Arc<Instances>,
    telemetry: Arc<Telemetry>,
    /// The snapshot last rendered. Held across the draws so a temperature
    /// update and a state render never interleave: an update drawn between
    /// a render's reads and its writes could put the temperature over a
    /// "Held by" line that just arrived.
    shown: tokio::sync::Mutex<Snapshot>,
}

impl OpenDeckRenderer {
    pub fn new(instances: Arc<Instances>, telemetry: Arc<Telemetry>) -> Self {
        Self {
            instances,
            telemetry,
            // The hub's own starting view; it renders every change from it.
            shown: tokio::sync::Mutex::new(Snapshot {
                state: ProfileState::Unavailable,
                conditions: Conditions::default(),
                available: Available::ALL,
            }),
        }
    }

    /// The tracked instances that still exist.
    async fn on_screen(&self) -> Vec<Arc<Instance>> {
        let mut found = Vec::new();
        for instance_id in self.instances.ids() {
            // None: disappeared since the IDs were read.
            if let Some(instance) = openaction::get_instance(instance_id).await {
                found.push(instance);
            }
        }
        found
    }
}

#[async_trait]
impl Renderer for OpenDeckRenderer {
    async fn render(&self, snapshot: &Snapshot) {
        let mut shown = self.shown.lock().await;
        *shown = snapshot.clone();
        let telemetry = self.telemetry.latest();
        for instance in self.on_screen().await {
            if let Err(e) = draw(&instance, snapshot, telemetry.as_deref()).await {
                log::warn!("render to {} failed: {e}", instance.instance_id);
            }
        }
    }
}

#[async_trait]
impl TelemetrySink for OpenDeckRenderer {
    /// Redraws the dials' status line only; keys show no temperature.
    async fn telemetry_changed(&self) {
        let shown = self.shown.lock().await;
        let update = status_feedback(&shown, self.telemetry.latest().as_deref());
        for instance in self.on_screen().await {
            if is_key(&instance) {
                continue;
            }
            if let Err(e) = instance.set_feedback(&update).await {
                log::warn!("status update to {} failed: {e}", instance.instance_id);
            }
        }
    }
}

/// The "Power Profile" action, on a dial or a key: translates OpenDeck
/// events into hub calls.
pub struct PowerProfileAction {
    hub: ProfileHub,
    instances: Arc<Instances>,
    telemetry: Arc<Telemetry>,
    /// When each key instance went down, to tell a press from a long press
    /// on release.
    key_downs: Mutex<HashMap<String, Instant>>,
}

impl PowerProfileAction {
    pub fn new(hub: ProfileHub, instances: Arc<Instances>, telemetry: Arc<Telemetry>) -> Self {
        Self {
            hub,
            instances,
            telemetry,
            key_downs: Mutex::new(HashMap::new()),
        }
    }

    fn key_downs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.key_downs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn request(&self, instance: &Instance, request: Request) -> OpenActionResult<()> {
        if let Err(e) = self.hub.request(request).await {
            // A dimmed dial is refused on every detent; the alert is the
            // feedback, so don't also fill the log with it.
            match e {
                RequestError::NotConnected => log::debug!("{request:?} refused: {e}"),
                RequestError::Dbus(_) => log::warn!("{request:?} failed: {e}"),
            }
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
        // Track first: a change from here on is rendered to it as well.
        self.instances.track(&instance.instance_id);
        let telemetry = self.telemetry.latest();
        draw(instance, &self.hub.snapshot(), telemetry.as_deref()).await
    }

    async fn will_disappear(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        self.instances.untrack(&instance.instance_id);
        self.key_downs().remove(&instance.instance_id);
        Ok(())
    }

    async fn key_down(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        self.key_downs()
            .insert(instance.instance_id.clone(), Instant::now());
        Ok(())
    }

    /// A press cycles to the next profile; a long press resets to Balanced.
    /// A release without a recorded key-down counts as a press.
    async fn key_up(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let pressed_at = self.key_downs().remove(&instance.instance_id);
        let request = match pressed_at {
            Some(at) if is_long_press(at.elapsed()) => Request::Reset,
            _ => Request::Cycle,
        };
        self.request(instance, request).await
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

    #[test]
    fn the_action_is_offered_on_dials_and_keys() {
        assert_eq!(
            manifest()["Actions"][0]["Controllers"],
            serde_json::json!(["Encoder", "Keypad"])
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
