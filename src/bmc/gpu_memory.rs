use super::{FetchError, Target};
use crate::{
    gpu_memory::{Floors, Gpu, Measurement, Round, COUNTERS},
    metrics,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use tokio::{
    sync::Semaphore,
    time::{self, Duration, Instant},
};

const PROCESSORS: &str = "/redfish/v1/Systems/HGX_Baseboard_0/Processors";
const MEMORY: &str = "/redfish/v1/Systems/HGX_Baseboard_0/Memory";
const REPORT: &str = "/redfish/v1/TelemetryService/MetricReports/HGX_MemoryMetrics_0";
const LIMIT: u8 = 35;

struct Reader<'a> {
    target: &'a Target,
    inflight: &'a Semaphore,
    deadline: Instant,
    requests: u8,
    stopped: bool,
}

impl Reader<'_> {
    async fn read(&mut self, path: &str) -> Option<Value> {
        if self.stopped || self.requests >= LIMIT || self.deadline <= Instant::now() {
            return None;
        }
        self.requests += 1;
        // Queue behind a baseline scrape, but give it the fair mutex again
        // between reads. Time spent waiting counts against the total budget.
        let result = time::timeout_at(self.deadline, async {
            let _guard = self.target.scrape_lock.lock().await;
            if self.target.blocked().await.is_some() {
                return None;
            }
            let value = self
                .target
                .get_with_timeout(path, self.inflight, Duration::from_secs(4))
                .await;
            if matches!(value, Err(FetchError::Http(403))) {
                self.target.note_forbidden().await;
            }
            Some(value)
        })
        .await;
        match result {
            Ok(Some(Ok(value))) => Some(value),
            Ok(Some(Err(FetchError::Http(403)))) | Ok(None) | Err(_) => {
                self.stopped = true;
                None
            }
            Ok(Some(Err(_))) => None,
        }
    }
}

fn gpu_slot(id: &str) -> Option<&str> {
    let slot = id.strip_prefix("GPU_")?;
    (slot.len() == 1 && matches!(slot.as_bytes()[0], b'0'..=b'7')).then_some(id)
}

fn count(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
}

fn time_seconds(s: &str) -> Option<f64> {
    let time = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    Some(time.timestamp() as f64 + f64::from(time.timestamp_subsec_nanos()) / 1e9)
}

fn report_counters(doc: &Value, now: f64) -> BTreeMap<String, [Option<Measurement>; 4]> {
    let mut out = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for row in doc["MetricValues"].as_array().into_iter().flatten() {
        let Some((id, path)) = row["MetricProperty"].as_str().and_then(|s| {
            s.strip_prefix(MEMORY)?
                .strip_prefix('/')?
                .split_once("_DRAM_0/MemoryMetrics#")
        }) else {
            continue;
        };
        let Some(id) = gpu_slot(id) else { continue };
        let Some(idx) = COUNTERS.iter().position(|(_, p)| *p == path) else {
            continue;
        };
        let Some(value) = count(&row["MetricValue"]) else {
            continue;
        };
        let Some(source_at) = row["Timestamp"].as_str().and_then(time_seconds) else {
            continue;
        };
        if !(-60.0..900.0).contains(&(now - source_at)) {
            continue;
        }
        let counters = out.entry(id.to_owned()).or_insert([None; 4]);
        // Ambiguous duplicate rows fall back to the direct resource.
        counters[idx] = seen
            .insert((id, idx))
            .then_some(Measurement { value, source_at });
    }
    out
}

fn direct_counters(doc: &Value, now: f64) -> [Option<Measurement>; 4] {
    std::array::from_fn(|i| {
        let value = count(doc.pointer(COUNTERS[i].1)?)?;
        Some(Measurement {
            value,
            source_at: now,
        })
    })
}

fn complete_members(doc: &Value, n: usize) -> bool {
    doc["Members"]
        .as_array()
        .is_some_and(|items| items.len() == n)
        && doc["Members@odata.nextLink"].is_null()
        && doc["Members@odata.count"]
            .as_u64()
            .is_none_or(|v| v == n as u64)
}

// Some B300 firmware advertises a non-GPU processor alongside the eight GPUs.
// Pagination and the reported count apply to the whole collection, not the GPU subset.
fn complete_processor_collection(doc: &Value) -> bool {
    doc["Members"]
        .as_array()
        .is_some_and(|members| members.len() <= 16 && complete_members(doc, members.len()))
}

fn identity(processor: &Value, id: &str) -> Option<String> {
    let uuid = processor["UUID"].as_str()?;
    let serial = processor["SerialNumber"].as_str()?;
    (processor["Id"] == id
        && !uuid.is_empty()
        && uuid.len() <= 128
        && !serial.is_empty()
        && serial.len() <= 128)
        .then(|| format!("{uuid}|{serial}"))
}

fn reset_marker(doc: &Value) -> Option<String> {
    let values: BTreeMap<_, _> = doc
        .as_object()?
        .iter()
        .filter(|(k, v)| k.ends_with("Count") && (v.is_i64() || v.is_u64()))
        .collect();
    (!values.is_empty()).then(|| serde_json::to_string(&values).expect("JSON map"))
}

pub async fn collect(target: &Target, inflight: &Semaphore, floors: &Floors) -> Round {
    let mut reader = Reader {
        target,
        inflight,
        deadline: Instant::now() + Duration::from_secs(45),
        requests: 0,
        stopped: false,
    };
    let mut out = Round::default();
    let Some(memory) = reader.read(&format!("{MEMORY}?$expand=*($levels=1)")).await else {
        return out;
    };
    let mut members = BTreeMap::new();
    let mut duplicate = BTreeSet::new();
    for entry in memory["Members"].as_array().into_iter().flatten().take(8) {
        let item = match entry["Id"].as_str() {
            Some(_) => entry.clone(),
            None => {
                let Some(path) = entry["@odata.id"].as_str().filter(|p| {
                    p.strip_prefix(MEMORY)
                        .and_then(|v| v.strip_prefix('/'))
                        .and_then(|v| v.strip_suffix("_DRAM_0"))
                        .and_then(gpu_slot)
                        .is_some()
                }) else {
                    continue;
                };
                let Some(item) = reader.read(path).await else {
                    continue;
                };
                // Expanded result must be the exact advertised slot.
                if item["Id"].as_str() != path.rsplit('/').next() {
                    continue;
                }
                item
            }
        };
        let Some(id) = item["Id"]
            .as_str()
            .and_then(|v| v.strip_suffix("_DRAM_0"))
            .and_then(gpu_slot)
        else {
            continue;
        };
        let flags = Gpu {
            pending: item
                .pointer("/Oem/Nvidia/RowRemappingPending")
                .and_then(Value::as_bool),
            failed: item
                .pointer("/Oem/Nvidia/RowRemappingFailed")
                .and_then(Value::as_bool),
            ..Default::default()
        };
        if members.insert(id.to_owned(), flags).is_some() {
            duplicate.insert(id.to_owned());
        }
    }
    for id in duplicate {
        members.remove(&id);
    }
    if members.is_empty() {
        return out;
    }

    let processors = reader.read(PROCESSORS).await;
    let mut details = BTreeMap::new();
    let mut repeated = BTreeSet::new();
    let mut advertised = BTreeSet::new();
    if let Some(doc) = &processors {
        for m in doc["Members"].as_array().into_iter().flatten().take(16) {
            let Some(id) = m["@odata.id"]
                .as_str()
                .and_then(|p| p.strip_prefix(PROCESSORS))
                .and_then(|p| p.strip_prefix('/'))
                .and_then(gpu_slot)
            else {
                continue;
            };
            if !advertised.insert(id) {
                repeated.insert(id);
            }
        }
        for id in advertised.iter().filter(|id| !repeated.contains(*id)) {
            let path = format!("{PROCESSORS}/{id}");
            if let Some(processor) = reader.read(&path).await.filter(|v| v["Id"] == *id) {
                details.insert((*id).to_owned(), (path, processor));
            }
        }
    }
    if !advertised.is_empty() && repeated.is_empty() {
        metrics::gpu_slots(&mut out.inventory, advertised.len());
        let known: Vec<_> = details
            .values()
            .map(|(p, v)| (p.as_str(), v.clone()))
            .collect();
        // Incomplete collection may expose slots, but not a false online total.
        if processors
            .as_ref()
            .is_some_and(complete_processor_collection)
        {
            metrics::gpu_details(&mut out.inventory, &known, advertised.len());
        }
    }
    let raw = reader
        .read(REPORT)
        .await
        .map(|doc| report_counters(&doc, metrics::epoch()))
        .unwrap_or_default();
    for (id, gpu) in &mut members {
        let Some((path, processor)) = details.get(id) else {
            continue;
        };
        let Some(gpu_id) = identity(processor, id) else {
            continue;
        };
        gpu.identity = Some(gpu_id.clone());
        gpu.counters = raw.get(id).copied().unwrap_or_default();
        // A report belongs to a slot, not necessarily the newly installed GPU.
        if floors.get(id).is_some_and(|(old, _)| *old != gpu_id) {
            gpu.counters = [None; 4];
        }
        if let Some((_, old)) = floors.get(id).filter(|(identity, _)| *identity == gpu_id) {
            for (m, floor) in gpu.counters.iter_mut().zip(old) {
                if m.zip(*floor).is_some_and(|(m, floor)| m.source_at <= floor) {
                    *m = None;
                }
            }
        }
        if gpu.counters.iter().any(Option::is_none) {
            if let Some(doc) = reader
                .read(&format!("{MEMORY}/{id}_DRAM_0/MemoryMetrics"))
                .await
            {
                let fresh = direct_counters(&doc, metrics::epoch());
                // Keep one coherent source per GPU when the direct read is
                // complete; otherwise replace only missing measurements.
                if fresh.iter().all(Option::is_some) {
                    gpu.counters = fresh;
                } else {
                    for (have, value) in gpu.counters.iter_mut().zip(fresh) {
                        *have = have.or(value);
                    }
                }
            }
        }
        let expected = format!("{path}/Oem/Nvidia/ProcessorResetMetrics");
        if processor
            .pointer("/Oem/Nvidia/ProcessorResetMetrics/@odata.id")
            .and_then(Value::as_str)
            == Some(expected.as_str())
        {
            gpu.marker = reader.read(&expected).await.as_ref().and_then(reset_marker);
        }
    }
    out.complete = complete_members(&memory, 8)
        && members.len() == 8
        && advertised.len() == 8
        && repeated.is_empty()
        && members.keys().eq(details.keys())
        && !reader.stopped
        && processors
            .as_ref()
            .is_some_and(complete_processor_collection);
    out.gpus = members;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_stale_missing_future_and_duplicate_report_samples() {
        let path = format!("{MEMORY}/GPU_0_DRAM_0/MemoryMetrics#{}", COUNTERS[1].1);
        let row = |stamp| json!({"MetricProperty":path,"MetricValue":"7","Timestamp":stamp});
        let now = 1000.0;
        let fresh = row("1970-01-01T00:16:39Z");
        assert_eq!(
            report_counters(&json!({"MetricValues":[fresh]}), now)["GPU_0"][1]
                .unwrap()
                .value,
            7.0
        );
        assert!(report_counters(&json!({"MetricValues":[row("1970-01-01T00:01:40Z"),row("1970-01-01T00:18:00Z"),row("invalid"),row("1970-01-01T00:16:39")]}),now).is_empty());
        let result = report_counters(&json!({"MetricValues":[fresh.clone(),fresh]}), now);
        assert!(result["GPU_0"][1].is_none());
        let direct = direct_counters(
            &json!({"LifeTime":{"UncorrectableECCErrorCount":7,"CorrectableECCErrorCount":false}}),
            now,
        );
        assert!(direct[0].is_none());
        assert_eq!(direct[1].unwrap().source_at, now);
    }
    #[tokio::test]
    async fn reads_one_bounded_round_and_baseline_reuses_memory_reads() {
        use axum::{
            extract::{OriginalUri, State},
            routing::get,
            Json, Router,
        };
        use std::sync::{Arc, Mutex};
        async fn fixture(
            State(seen): State<Arc<Mutex<Vec<String>>>>,
            OriginalUri(uri): OriginalUri,
        ) -> Json<Value> {
            let path = uri.path();
            seen.lock().unwrap().push(path.to_owned());
            if path == MEMORY {
                return Json(
                    json!({"Members":(0..8).map(|i|json!({"@odata.id":format!("{MEMORY}/GPU_{i}_DRAM_0")})).collect::<Vec<_>>()}),
                );
            }
            if path == PROCESSORS {
                let mut members = (0..8)
                    .map(|i| json!({"@odata.id":format!("{PROCESSORS}/GPU_{i}")}))
                    .collect::<Vec<_>>();
                members.push(json!({"@odata.id":format!("{PROCESSORS}/OtherProcessor")}));
                return Json(json!({"Members":members,"Members@odata.count":9}));
            }
            if path == REPORT {
                return Json(json!({"MetricValues":[]}));
            }
            if path.starts_with(MEMORY) && path.ends_with("/MemoryMetrics") {
                return Json(
                    json!({"LifeTime":{"CorrectableECCErrorCount":1,"UncorrectableECCErrorCount":2},"Oem":{"Nvidia":{"RowRemapping":{"CorrectableRowRemappingCount":3,"UncorrectableRowRemappingCount":4}}}}),
                );
            }
            if path.starts_with(MEMORY) && path.ends_with("_DRAM_0") {
                return Json(
                    json!({"Id":path.rsplit('/').next(),"Oem":{"Nvidia":{"RowRemappingPending":false,"RowRemappingFailed":false}}}),
                );
            }
            if path.starts_with(PROCESSORS) && path.ends_with("/ProcessorResetMetrics") {
                return Json(json!({"ResetCount": 1}));
            }
            if let Some(id) = path
                .strip_prefix(PROCESSORS)
                .and_then(|p| p.strip_prefix('/'))
                .and_then(gpu_slot)
            {
                return Json(
                    json!({"Id":id,"UUID":format!("uuid-{id}"),"SerialNumber":format!("serial-{id}"),"Status":{"State":"Enabled"},"MemorySummary":{"TotalMemorySizeMiB":288000},"Oem":{"Nvidia":{"ProcessorResetMetrics":{"@odata.id":format!("{PROCESSORS}/{id}/Oem/Nvidia/ProcessorResetMetrics")}}}}),
                );
            }
            Json(json!({}))
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let router = Router::new()
            .fallback(get(fixture))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut t = Target::mock(&format!("http://{addr}/"));
        t.gpu_memory_state = true;
        let inflight = Semaphore::new(4);
        let round = collect(&t, &inflight, &Floors::new()).await;
        assert!(round.complete);
        assert_eq!(seen.lock().unwrap().len(), usize::from(LIMIT));
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.ends_with("/OtherProcessor")));
        assert_eq!(round.gpus.len(), 8);
        assert_eq!(
            round.gpus["GPU_0"].identity.as_deref(),
            Some("uuid-GPU_0|serial-GPU_0")
        );
        assert_eq!(
            round.gpus["GPU_0"].marker.as_deref(),
            Some("{\"ResetCount\":1}")
        );
        assert_eq!(round.gpus["GPU_0"].counters[3].unwrap().value, 4.0);
        *t.gpu_memory_snapshot.lock().await = Some(crate::gpu_memory::Snapshot {
            observed_at: metrics::epoch(),
            state: vec![],
            inventory: round.inventory,
        });
        seen.lock().unwrap().clear();
        let _ = super::super::collect("bmc", &t, &inflight).await;
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|s| s == "/redfish/v1/Systems/System_0"));
        assert!(!seen
            .iter()
            .any(|s| s == REPORT || s.starts_with(PROCESSORS) || s.starts_with(MEMORY)));
        task.abort();
    }
}
