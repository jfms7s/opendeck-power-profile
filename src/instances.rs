//! The on-screen instances of the action: the draw side's registry. The hub
//! knows nothing about instances; the renderer draws every snapshot to the
//! ones tracked here.

use std::collections::BTreeSet;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct Instances(Mutex<BTreeSet<String>>);

impl Instances {
    pub fn track(&self, instance_id: &str) {
        self.lock().insert(instance_id.to_string());
    }

    pub fn untrack(&self, instance_id: &str) {
        self.lock().remove(instance_id);
    }

    /// Whether nothing is on screen.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// The tracked instance IDs, sorted.
    pub fn ids(&self) -> Vec<String> {
        self.lock().iter().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        // Never held across an await or a panic point; recover rather than
        // stop drawing if that ever changes.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_and_untracks_ids() {
        let instances = Instances::default();
        instances.track("b");
        instances.track("a");
        instances.track("a");
        assert_eq!(instances.ids(), ["a", "b"]);
        instances.untrack("b");
        instances.untrack("missing");
        assert_eq!(instances.ids(), ["a"]);
        assert!(!instances.is_empty());
        instances.untrack("a");
        assert!(instances.is_empty());
    }
}
