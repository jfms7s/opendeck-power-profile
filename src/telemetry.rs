//! CPU temperature for the touch strip's status line: finds the CPU sensor
//! under sysfs `hwmon`, reads and formats it, and polls it - but only while
//! an instance is on screen. Display only: nothing here touches the profile,
//! and a sensor that is missing or unreadable just leaves the line empty.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;

use crate::instances::Instances;

/// Where the kernel lists its hardware monitors.
pub const HWMON_ROOT: &str = "/sys/class/hwmon";

/// How often the temperature is read while an instance is on screen. The
/// same 5 seconds as the reconnect interval: often enough to follow a load
/// change, rare enough to cost nothing.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The CPU temperature drivers, each with the labels of the reading that
/// stands for the whole package, best first. `hwmonN` numbers are not stable
/// across boots, so a sensor is found by its driver name.
const CPU_SENSORS: [(&str, &[&str]); 3] = [
    ("k10temp", &["Tctl", "Tdie"]),
    ("zenpower", &["Tdie", "Tctl"]),
    ("coretemp", &["Package id 0"]),
];

/// The CPU temperature under `root` (normally [`HWMON_ROOT`]) as the strip
/// shows it, e.g. `"42 °C"`; `None` when there is no known CPU sensor or it
/// can't be read. Blocking file reads: call it off the async runtime.
pub fn read_cpu_temperature(root: &Path) -> Option<String> {
    let input = find_cpu_input(root)?;
    format_millidegrees(&fs::read_to_string(input).ok()?)
}

/// The `temp*_input` file of the first known CPU sensor under `root`: the
/// one whose `temp*_label` is the driver's preferred label, else `temp1_input`.
fn find_cpu_input(root: &Path) -> Option<PathBuf> {
    let mut devices: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    devices.sort();
    for (driver, labels) in CPU_SENSORS {
        for device in &devices {
            let name = fs::read_to_string(device.join("name")).unwrap_or_default();
            if name.trim() != driver {
                continue;
            }
            if let Some(input) = labelled_input(device, labels) {
                return Some(input);
            }
            let first = device.join("temp1_input");
            if first.is_file() {
                return Some(first);
            }
        }
    }
    None
}

/// The `tempN_input` whose `tempN_label` is the first of `labels` present.
fn labelled_input(device: &Path, labels: &[&str]) -> Option<PathBuf> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(device).ok()?.flatten() {
        let file_name = entry.file_name();
        let Some(channel) = file_name
            .to_str()
            .and_then(|name| name.strip_suffix("_label"))
            .filter(|channel| channel.starts_with("temp"))
        else {
            continue;
        };
        let Ok(label) = fs::read_to_string(entry.path()) else {
            continue;
        };
        found.push((
            label.trim().to_string(),
            device.join(format!("{channel}_input")),
        ));
    }
    labels.iter().find_map(|wanted| {
        found
            .iter()
            .find(|(label, _)| label == wanted)
            .map(|(_, input)| input.clone())
    })
}

/// A sysfs temperature (millidegrees Celsius, as text) rounded to whole
/// degrees: `"42125\n"` -> `"42 °C"`. `None` for anything else.
pub fn format_millidegrees(text: &str) -> Option<String> {
    let millidegrees: i64 = text.trim().parse().ok()?;
    // `as i64` also turns a rounded -0.0 into 0.
    let degrees = (millidegrees as f64 / 1000.0).round() as i64;
    Some(format!("{degrees} °C"))
}

/// The latest reading, shared by the poller (writes) and the renderer
/// (reads). Logs once when the sensor goes missing and once when it is back,
/// never on every poll.
#[derive(Debug, Default)]
pub struct Telemetry(Mutex<Latest>);

#[derive(Debug, Default)]
struct Latest {
    reading: Option<String>,
    missing_logged: bool,
}

impl Telemetry {
    /// What the status line shows when nothing else claims it.
    pub fn latest(&self) -> Option<String> {
        self.lock().reading.clone()
    }

    /// Records a reading; returns whether it differs from the last one
    /// (whether to redraw).
    fn record(&self, reading: Option<String>) -> bool {
        let mut latest = self.lock();
        match (&reading, latest.missing_logged) {
            (None, false) => {
                log::info!("no readable CPU temperature sensor; the status line stays empty");
                latest.missing_logged = true;
            }
            (Some(_), true) => {
                log::info!("CPU temperature sensor readable again");
                latest.missing_logged = false;
            }
            _ => {}
        }
        let changed = latest.reading != reading;
        latest.reading = reading;
        changed
    }

    /// Forgets the reading without logging, so an instance that appears
    /// after a pause never shows a stale temperature.
    fn forget(&self) {
        self.lock().reading = None;
    }

    fn lock(&self) -> MutexGuard<'_, Latest> {
        // Never held across an await; recover rather than stop the line.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Redraws the status line after the reading changed.
#[async_trait]
pub trait TelemetrySink: Send + Sync + 'static {
    async fn telemetry_changed(&self);
}

/// Reads the sensor (`read` runs on the blocking pool) every `interval`
/// while `instances` has anything on screen, and tells `sink` when the
/// reading changes. Runs forever; spawn it.
pub async fn poll<R>(
    instances: Arc<Instances>,
    telemetry: Arc<Telemetry>,
    sink: Arc<dyn TelemetrySink>,
    read: R,
    interval: Duration,
) where
    R: Fn() -> Option<String> + Clone + Send + 'static,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if tick(&instances, &telemetry, read.clone()).await {
            sink.telemetry_changed().await;
        }
    }
}

/// One poll: no read at all while nothing is on screen. Returns whether the
/// reading changed.
async fn tick<R>(instances: &Instances, telemetry: &Telemetry, read: R) -> bool
where
    R: FnOnce() -> Option<String> + Send + 'static,
{
    if instances.is_empty() {
        telemetry.forget();
        return false;
    }
    // A panicking read counts as no reading; it never takes the poller down.
    let reading = tokio::task::spawn_blocking(read).await.unwrap_or(None);
    telemetry.record(reading)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A fake `/sys/class/hwmon`: one directory per device, each file given
    /// as (name, contents).
    fn hwmon(devices: &[(&str, &[(&str, &str)])]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (device, files) in devices {
            let dir = root.path().join(device);
            fs::create_dir(&dir).unwrap();
            for (name, contents) in *files {
                fs::write(dir.join(name), contents).unwrap();
            }
        }
        root
    }

    #[test]
    fn k10temp_reads_its_tctl() {
        let root = hwmon(&[
            ("hwmon0", &[("name", "nvme\n"), ("temp1_input", "35850\n")]),
            (
                "hwmon1",
                &[
                    ("name", "k10temp\n"),
                    ("temp1_input", "42125\n"),
                    ("temp1_label", "Tctl\n"),
                    ("temp3_input", "39000\n"),
                    ("temp3_label", "Tccd1\n"),
                ],
            ),
        ]);
        assert_eq!(read_cpu_temperature(root.path()).as_deref(), Some("42 °C"));
    }

    #[test]
    fn the_preferred_label_wins_over_temp1() {
        let root = hwmon(&[(
            "hwmon3",
            &[
                ("name", "coretemp\n"),
                ("temp1_input", "30000\n"),
                ("temp1_label", "Core 0\n"),
                ("temp2_input", "55500\n"),
                ("temp2_label", "Package id 0\n"),
            ],
        )]);
        assert_eq!(read_cpu_temperature(root.path()).as_deref(), Some("56 °C"));
    }

    #[test]
    fn without_labels_temp1_is_read() {
        let root = hwmon(&[("hwmon1", &[("name", "k10temp\n"), ("temp1_input", "61000")])]);
        assert_eq!(read_cpu_temperature(root.path()).as_deref(), Some("61 °C"));
    }

    #[test]
    fn no_cpu_sensor_reads_nothing() {
        let root = hwmon(&[
            ("hwmon0", &[("name", "nvme\n"), ("temp1_input", "35850\n")]),
            (
                "hwmon1",
                &[("name", "amdgpu\n"), ("temp1_input", "50000\n")],
            ),
        ]);
        assert_eq!(read_cpu_temperature(root.path()), None);
        assert_eq!(read_cpu_temperature(&root.path().join("missing")), None);
    }

    #[test]
    fn an_unreadable_reading_is_none_not_a_panic() {
        let garbage = hwmon(&[("hwmon1", &[("name", "k10temp\n"), ("temp1_input", "n/a\n")])]);
        assert_eq!(read_cpu_temperature(garbage.path()), None);
        // The label points at an input that isn't there.
        let dangling = hwmon(&[(
            "hwmon1",
            &[("name", "k10temp\n"), ("temp1_label", "Tctl\n")],
        )]);
        assert_eq!(read_cpu_temperature(dangling.path()), None);
    }

    #[test]
    fn millidegrees_round_to_whole_degrees() {
        for (text, shown) in [
            ("42125\n", Some("42 °C")),
            ("42500", Some("43 °C")),
            ("0", Some("0 °C")),
            ("-5400", Some("-5 °C")),
            ("-400", Some("0 °C")),
            ("", None),
            ("hot", None),
        ] {
            assert_eq!(format_millidegrees(text).as_deref(), shown, "{text:?}");
        }
    }

    #[test]
    fn record_reports_changes_only() {
        let telemetry = Telemetry::default();
        assert!(telemetry.record(Some("42 °C".into())));
        assert!(!telemetry.record(Some("42 °C".into())));
        assert!(telemetry.record(Some("43 °C".into())));
        assert!(telemetry.record(None));
        assert!(!telemetry.record(None));
        assert_eq!(telemetry.latest(), None);
    }

    fn counting_read(
        reads: &Arc<AtomicUsize>,
        value: &'static str,
    ) -> impl FnOnce() -> Option<String> + Send + 'static {
        let reads = reads.clone();
        move || {
            reads.fetch_add(1, Ordering::SeqCst);
            Some(value.to_string())
        }
    }

    #[tokio::test]
    async fn nothing_is_read_while_no_instance_is_on_screen() {
        let instances = Instances::default();
        let telemetry = Telemetry::default();
        let reads = Arc::new(AtomicUsize::new(0));

        assert!(!tick(&instances, &telemetry, counting_read(&reads, "42 °C")).await);
        assert_eq!(reads.load(Ordering::SeqCst), 0);

        instances.track("dial");
        assert!(tick(&instances, &telemetry, counting_read(&reads, "42 °C")).await);
        assert!(!tick(&instances, &telemetry, counting_read(&reads, "42 °C")).await);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(telemetry.latest().as_deref(), Some("42 °C"));

        // The last instance goes: no more reads, and the reading is dropped
        // so the next instance never starts with a stale one.
        instances.untrack("dial");
        assert!(!tick(&instances, &telemetry, counting_read(&reads, "50 °C")).await);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(telemetry.latest(), None);
    }

    #[tokio::test]
    async fn a_panicking_read_counts_as_no_reading() {
        let instances = Instances::default();
        instances.track("dial");
        let telemetry = Telemetry::default();
        telemetry.record(Some("42 °C".into()));
        assert!(tick(&instances, &telemetry, || panic!("sensor driver bug")).await);
        assert_eq!(telemetry.latest(), None);
    }
}
