use crate::metrics::{self, Metric};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_AGE: f64 = 600.0;
pub const COUNTERS: [(&str, &str); 4] = [
    (
        "ecc_correctable_total",
        "/LifeTime/CorrectableECCErrorCount",
    ),
    (
        "ecc_uncorrectable_total",
        "/LifeTime/UncorrectableECCErrorCount",
    ),
    (
        "rows_correctable_total",
        "/Oem/Nvidia/RowRemapping/CorrectableRowRemappingCount",
    ),
    (
        "rows_uncorrectable_total",
        "/Oem/Nvidia/RowRemapping/UncorrectableRowRemappingCount",
    ),
];
const NAMES: [[&str; 5]; 4] = [
    [
        "gpu_memory_state_ecc_correctable_total",
        "gpu_memory_state_ecc_correctable_event_timestamp_seconds",
        "gpu_memory_state_ecc_correctable_event_delta",
        "gpu_memory_state_ecc_correctable_discontinuity_timestamp_seconds",
        "redfish_gpu_ecc_correctable_errors",
    ],
    [
        "gpu_memory_state_ecc_uncorrectable_total",
        "gpu_memory_state_ecc_uncorrectable_event_timestamp_seconds",
        "gpu_memory_state_ecc_uncorrectable_event_delta",
        "gpu_memory_state_ecc_uncorrectable_discontinuity_timestamp_seconds",
        "redfish_gpu_ecc_uncorrectable_errors",
    ],
    [
        "gpu_memory_state_rows_correctable_total",
        "gpu_memory_state_rows_correctable_event_timestamp_seconds",
        "gpu_memory_state_rows_correctable_event_delta",
        "gpu_memory_state_rows_correctable_discontinuity_timestamp_seconds",
        "redfish_gpu_row_remap_correctable",
    ],
    [
        "gpu_memory_state_rows_uncorrectable_total",
        "gpu_memory_state_rows_uncorrectable_event_timestamp_seconds",
        "gpu_memory_state_rows_uncorrectable_event_delta",
        "gpu_memory_state_rows_uncorrectable_discontinuity_timestamp_seconds",
        "redfish_gpu_row_remap_uncorrectable",
    ],
];

#[derive(Debug, Clone, Copy)]
pub struct Measurement {
    pub value: f64,
    pub source_at: f64,
}

#[derive(Debug, Default)]
pub struct Gpu {
    pub pending: Option<bool>,
    pub failed: Option<bool>,
    pub identity: Option<String>,
    pub marker: Option<String>,
    pub counters: [Option<Measurement>; 4],
}

#[derive(Debug, Default)]
pub struct Round {
    pub gpus: BTreeMap<String, Gpu>,
    pub inventory: Vec<Metric>,
    pub complete: bool,
}

#[derive(Debug)]
pub struct Snapshot {
    pub observed_at: f64,
    pub state: Vec<Metric>,
    pub inventory: Vec<Metric>,
}

impl Snapshot {
    pub fn fresh(&self, now: f64) -> bool {
        (0.0..MAX_AGE).contains(&(now - self.observed_at))
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    gpus: BTreeMap<String, GpuState>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GpuState {
    identity: String,
    marker: Option<String>,
    counters: BTreeMap<String, Counter>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Counter {
    last: f64,
    peak: f64,
    low_samples: u32,
    buffer: f64,
    event_at: f64,
    event_delta: f64,
    ambiguous_at: f64,
    seen: f64,
    // Python v1 state has only local observation time. Conservatively use
    // that time as the report floor on import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_at: Option<f64>,
}

impl Counter {
    fn new(m: Measurement, now: f64) -> Self {
        Self {
            last: m.value,
            peak: m.value,
            low_samples: 0,
            buffer: 0.0,
            event_at: 0.0,
            event_delta: 0.0,
            ambiguous_at: 0.0,
            seen: now,
            source_at: Some(m.source_at),
        }
    }

    fn observe(&mut self, m: Measurement, now: f64, marker_changed: bool) -> bool {
        if now <= self.seen || m.source_at <= self.source_at.unwrap_or(self.seen) {
            return false;
        }
        let mut delta = 0.0;
        if m.value < self.peak {
            if self.low_samples == 0 || now - self.seen > MAX_AGE || m.value < self.last {
                self.low_samples = 1;
                self.buffer = 0.0;
                self.ambiguous_at = now;
            } else {
                self.low_samples = self.low_samples.saturating_add(1);
                self.buffer += (m.value - self.last).max(0.0);
                if self.low_samples >= 3 {
                    delta = self.buffer;
                    self.buffer = 0.0;
                }
            }
        } else {
            delta = (m.value - self.peak).max(0.0);
            if self.low_samples != 0 {
                self.ambiguous_at = now;
            }
            self.low_samples = 0;
            self.buffer = 0.0;
            self.peak = m.value;
        }
        if delta > 0.0 {
            self.event_at = now;
            self.event_delta = delta;
        }
        if marker_changed {
            self.ambiguous_at = now;
        }
        self.last = m.value;
        self.seen = now;
        self.source_at = Some(m.source_at);
        true
    }
}

pub type Floors = BTreeMap<String, (String, [Option<f64>; 4])>;

/// Call under a single process-wide lock and expose events only after save.
pub struct Tracker {
    path: PathBuf,
    state: State,
}

impl Tracker {
    pub fn load(path: PathBuf) -> Result<Self> {
        if !path.is_absolute() || path.file_name().is_none() {
            bail!("GPU memory state requires an absolute file path");
        }
        if !path.parent().is_some_and(Path::is_dir) {
            bail!("GPU memory state directory must already exist");
        }
        let state = match fs::symlink_metadata(&path) {
            Ok(md) => {
                if !md.file_type().is_file() || md.len() > 16 * 1024 * 1024 {
                    bail!("GPU memory state must be a regular file at most 16 MiB");
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if md.permissions().mode() & 0o077 != 0 {
                        bail!("GPU memory state must not be readable by group or others");
                    }
                }
                let state: State =
                    serde_json::from_slice(&fs::read(&path).context("read GPU memory baseline")?)
                        .map_err(|_| anyhow::anyhow!("invalid GPU memory baseline"))?;
                if state.version != 1
                    || state.gpus.len() > 4096
                    || state.gpus.values().any(|g| {
                        g.identity.is_empty()
                            || g.counters
                                .keys()
                                .any(|k| !COUNTERS.iter().any(|(name, _)| k == name))
                            || g.counters.values().any(|c| {
                                c.last > c.peak
                                    || [
                                        c.last,
                                        c.peak,
                                        c.buffer,
                                        c.event_at,
                                        c.event_delta,
                                        c.ambiguous_at,
                                        c.seen,
                                        c.source_at.unwrap_or(0.0),
                                    ]
                                    .iter()
                                    .any(|v| !v.is_finite() || *v < 0.0)
                            })
                    })
                {
                    bail!("invalid GPU memory baseline version or counters");
                }
                state
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State {
                version: 1,
                gpus: BTreeMap::new(),
            },
            Err(e) => return Err(e).context("inspect GPU memory baseline"),
        };
        let tmp = temp_path(&path)?;
        drop(create_private(&tmp).context("GPU memory state directory is not writable")?);
        fs::remove_file(tmp).context("remove GPU memory state write probe")?;
        Ok(Self { path, state })
    }

    pub fn floors(&self, address: &str) -> Floors {
        let prefix = format!("{address}/");
        self.state
            .gpus
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .map(|(key, g)| {
                (
                    key[prefix.len()..].to_owned(),
                    (
                        g.identity.clone(),
                        std::array::from_fn(|i| {
                            g.counters
                                .get(COUNTERS[i].0)
                                .map(|s| s.source_at.unwrap_or(s.seen))
                        }),
                    ),
                )
            })
            .collect()
    }

    pub fn apply(
        &mut self,
        host: &str,
        address: &str,
        mut round: Round,
        now: f64,
    ) -> Result<Snapshot> {
        let mut candidate = self.state.clone();
        let mut state = Vec::new();
        let label = || vec![("host", host.to_owned()), ("instance", address.to_owned())];
        let mut complete = round.complete && round.gpus.len() == 8;
        let mut changed = false;
        let mut count = 0;
        for (id, gpu) in round.gpus {
            let labels = || {
                let mut l = label();
                l.push(("gpu", id.clone()));
                l
            };
            let mut known = false;
            for (metric, value) in [
                ("gpu_memory_state_pending", gpu.pending),
                ("gpu_memory_state_failed", gpu.failed),
            ] {
                if let Some(value) = value {
                    metrics::add(&mut state, metric, labels(), f64::from(value));
                    known = true;
                } else {
                    complete = false;
                }
            }
            if let Some(identity) = gpu.identity {
                // Preserve the exact Python address/GPU key and UUID|serial identity.
                let key = format!("{address}/{id}");
                let entry = candidate.gpus.entry(key).or_insert_with(|| {
                    changed = true;
                    GpuState {
                        identity: identity.clone(),
                        marker: None,
                        counters: BTreeMap::new(),
                    }
                });
                if entry.identity != identity {
                    *entry = GpuState {
                        identity,
                        marker: None,
                        counters: BTreeMap::new(),
                    };
                    changed = true;
                }
                let marker_changed = gpu.marker.as_ref().is_some_and(|marker| {
                    entry.marker.as_ref().is_some_and(|previous| {
                        serde_json::from_str::<serde_json::Value>(previous).ok()
                            != serde_json::from_str::<serde_json::Value>(marker).ok()
                    })
                });
                let mut accepted_all = true;
                for (i, m) in gpu.counters.into_iter().enumerate() {
                    let Some(m) = m else {
                        complete = false;
                        accepted_all = false;
                        continue;
                    };
                    use std::collections::btree_map::Entry;
                    let c = match entry.counters.entry(COUNTERS[i].0.into()) {
                        Entry::Vacant(v) => v.insert(Counter::new(m, now)),
                        Entry::Occupied(v) => {
                            let c = v.into_mut();
                            if !c.observe(m, now, marker_changed) {
                                complete = false;
                                accepted_all = false;
                                continue;
                            }
                            c
                        }
                    };
                    changed = true;
                    known = true;
                    let [total, event_at, delta, discontinuity, raw] = NAMES[i];
                    metrics::add(&mut state, total, labels(), m.value);
                    metrics::add(&mut state, event_at, labels(), c.event_at);
                    metrics::add(&mut state, delta, labels(), c.event_delta);
                    metrics::add(&mut state, discontinuity, labels(), c.ambiguous_at);
                    metrics::add(
                        &mut round.inventory,
                        raw,
                        vec![("gpu", id.clone())],
                        m.value,
                    );
                }
                // Do not forget a reset on a round with missing counters.
                if accepted_all && gpu.marker.is_some() && entry.marker != gpu.marker {
                    entry.marker = gpu.marker;
                    changed = true;
                }
            } else {
                complete = false;
            }
            if known {
                count += 1;
                metrics::add(
                    &mut state,
                    "gpu_memory_state_observed_timestamp_seconds",
                    labels(),
                    now,
                );
            }
        }
        if changed {
            save(&self.path, &candidate)?;
            self.state = candidate;
        }
        metrics::add(
            &mut state,
            "gpu_memory_state_gpu_count",
            label(),
            f64::from(count),
        );
        metrics::add(
            &mut state,
            "gpu_memory_state_scrape_success",
            label(),
            f64::from(complete),
        );
        if complete {
            metrics::add(
                &mut state,
                "gpu_memory_state_last_success_timestamp_seconds",
                label(),
                now,
            );
        }
        Ok(Snapshot {
            observed_at: now,
            state,
            inventory: round.inventory,
        })
    }
}

fn temp_path(path: &Path) -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    Ok(path
        .parent()
        .context("GPU memory baseline directory")?
        .join(format!(
            ".gpu-memory-state-{}-{}-{}.tmp",
            std::process::id(),
            (metrics::epoch() * 1_000_000.0) as u64,
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
}

fn create_private(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn save(path: &Path, state: &State) -> Result<()> {
    let tmp = temp_path(path)?;
    let mut f = create_private(&tmp)?;
    let result = (|| -> Result<()> {
        serde_json::to_writer(&mut f, state)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        fs::File::open(path.parent().context("baseline directory")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn measure(value: f64, at: f64) -> Measurement {
        Measurement {
            value,
            source_at: at,
        }
    }

    #[test]
    fn history_rebound_and_confirmed_growth() {
        let mut c = Counter::new(measure(9.0, 100.0), 100.0);
        assert!(c.observe(measure(0.0, 400.0), 400.0, false));
        assert!(c.observe(measure(9.0, 700.0), 700.0, false));
        assert_eq!(c.event_at, 0.0);
        assert_eq!(c.ambiguous_at, 700.0);
        assert!(c.observe(measure(0.0, 1000.0), 1000.0, false));
        assert!(c.observe(measure(1.0, 1300.0), 1300.0, false));
        assert_eq!(c.event_at, 0.0);
        assert!(!c.observe(measure(1.0, 1300.0), 1500.0, false));
        assert_eq!(c.low_samples, 2);
        assert!(c.observe(measure(1.0, 1600.0), 1600.0, false));
        assert_eq!(c.event_delta, 1.0);
        assert_eq!(c.event_at, 1600.0);
        assert!(c.observe(measure(11.0, 1900.0), 1900.0, true));
        assert_eq!(c.event_delta, 2.0);
        assert_eq!(c.ambiguous_at, 1900.0);
    }

    #[test]
    fn long_gap_does_not_confirm_a_low_segment() {
        let mut c = Counter::new(measure(9.0, 100.0), 100.0);
        c.observe(measure(0.0, 400.0), 400.0, false);
        c.observe(measure(1.0, 700.0), 700.0, false);
        c.observe(measure(2.0, 1400.0), 1400.0, false);
        assert_eq!(c.low_samples, 1);
        assert_eq!(c.event_at, 0.0);
        assert_eq!(c.buffer, 0.0);
    }

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let parent = std::env::temp_dir();
            let path = temp_path(&parent.join("gpu-test")).unwrap();
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn round(identity: &str, marker: &str, value: f64, at: f64) -> Round {
        Round {
            gpus: BTreeMap::from([(
                "GPU_0".into(),
                Gpu {
                    pending: Some(false),
                    failed: None,
                    identity: Some(identity.into()),
                    marker: Some(marker.into()),
                    counters: [Some(measure(value, at)); 4],
                },
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn imports_python_state_and_restores_without_replaying_events() {
        let temp = TestDir::new();
        let path = temp.0.join("baseline.json");
        let mut file = create_private(&path).unwrap();
        serde_json::to_writer(&mut file,&json!({"version":1,"gpus":{"10.0.0.1/GPU_0":{
            "identity":"uuid|serial","marker":"{\"ResetCount\": 1}","counters":{
                "ecc_uncorrectable_total":{"last":9.0,"peak":9.0,"low_samples":0,"buffer":0,"event_at":0,"event_delta":0,"ambiguous_at":0,"seen":100.0}
            }
        }}})).unwrap();
        drop(file);
        let mut t = Tracker::load(path.clone()).unwrap();
        assert_eq!(t.floors("10.0.0.1")["GPU_0"].1[1], Some(100.0));
        let snapshot = t
            .apply(
                "B300-01",
                "10.0.0.1",
                round("uuid|serial", "{\"ResetCount\":1}", 10.0, 400.0),
                400.0,
            )
            .unwrap();
        let body = metrics::render_gpu_memory(&snapshot.state);
        assert!(body.contains("gpu_memory_state_ecc_uncorrectable_event_delta{host=\"B300-01\",instance=\"10.0.0.1\",gpu=\"GPU_0\"} 1\n"));
        assert!(body.contains("gpu_memory_state_ecc_uncorrectable_discontinuity_timestamp_seconds{host=\"B300-01\",instance=\"10.0.0.1\",gpu=\"GPU_0\"} 0\n"));
        assert!(!body.contains("gpu_memory_state_failed{"));
        assert!(body.contains(
            "gpu_memory_state_scrape_success{host=\"B300-01\",instance=\"10.0.0.1\"} 0\n"
        ));
        assert!(!body.contains("target="));
        let mut t = Tracker::load(path).unwrap();
        t.apply(
            "B300-01",
            "10.0.0.1",
            round("uuid|serial", "{\"ResetCount\":1}", 10.0, 700.0),
            700.0,
        )
        .unwrap();
        let c = &t.state.gpus["10.0.0.1/GPU_0"].counters["ecc_uncorrectable_total"];
        assert_eq!(c.event_at, 400.0);
        t.apply(
            "B300-01",
            "10.0.0.1",
            round("new|serial", "{\"ResetCount\":0}", 50.0, 1000.0),
            1000.0,
        )
        .unwrap();
        assert_eq!(
            t.state.gpus["10.0.0.1/GPU_0"].counters["ecc_uncorrectable_total"].event_at,
            0.0
        );
    }

    #[test]
    fn corrupt_or_unwritable_state_does_not_publish_or_rebase() {
        let temp = TestDir::new();
        let path = temp.0.join("baseline.json");
        let mut t = Tracker::load(path.clone()).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(t
            .apply(
                "a",
                "10.0.0.1",
                round("uuid|serial", "{}", 2.0, 100.0),
                100.0
            )
            .is_err());
        assert!(t.state.gpus.is_empty());
        fs::remove_dir(&path).unwrap();
        create_private(&path)
            .unwrap()
            .write_all(b"{broken")
            .unwrap();
        assert!(Tracker::load(path).is_err());
    }
}
