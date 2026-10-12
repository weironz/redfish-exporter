//! 1U12E-only bounded rolling CPU inventory. All GETs share the baseline budget.
//! No background worker, raw response cache, renewed source time, or longer TTL.
use super::{metrics, safe_path, Collection, Target, MAX_REQUESTS_PER_SCRAPE};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const MAX_AGE: Duration = Duration::from_secs(1800);

pub(super) struct Spec {
    kind: &'static str,
    root: String,
    limit: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::OriginalUri, http::StatusCode, response::IntoResponse, routing::get, Json, Router,
    };
    use std::sync::{Arc, Mutex as StdMutex};

    const ROOT: &str = "/redfish/v1/Systems/Self/Memory";
    type Fixture = (
        Target,
        Arc<StdMutex<(Value, Option<u16>)>>,
        Arc<StdMutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    );

    async fn mock() -> Fixture {
        let state = Arc::new(StdMutex::new((
            json!({"Members@odata.count":2,"Members":[{"@odata.id":format!("{ROOT}/0")},{"@odata.id":format!("{ROOT}/1")}]}),
            None,
        )));
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let responses = state.clone();
        let requests = seen.clone();
        let app=Router::new().fallback(get(move |OriginalUri(uri): OriginalUri| {
            requests.lock().unwrap().push(uri.to_string());
            let (directory,code)=responses.lock().unwrap().clone();
            async move {
                if uri.path()==ROOT { return Json(directory).into_response(); }
                if let Some(code)=code { return StatusCode::from_u16(code).unwrap().into_response(); }
                Json(json!({"Name":uri.path(),"CapacityMiB":32768,"Status":{"Health":"OK"},"Password":"must-not-be-cached"})).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            Target::mock_profile(
                &format!("http://{addr}/"),
                super::super::Profile::Asrockrack1u12e,
            ),
            state,
            seen,
            task,
        )
    }

    async fn read(target: &Target, spent: usize) -> Collection {
        let mut out = Collection::failed();
        out.up = true;
        out.errors = 0;
        out.requests = spent;
        collect(
            "CPU",
            target,
            &Semaphore::new(1),
            &mut out,
            vec![Spec::new("memory", ROOT.into(), 64)],
        )
        .await;
        out
    }

    #[test]
    fn summaries_are_bounded_whitelisted_and_do_not_accept_unknown_counts() {
        let value = json!({"Name":"x".repeat(10000),"CapacityMiB":"32768","Status":{"Health":"OK","Secret":"private"},"Password":"private","Oem":{"Token":"private"}});
        let slim = summary(&value, "memory").unwrap();
        assert_eq!(slim["Name"].as_str().unwrap().len(), 128);
        assert_eq!(slim["CapacityMiB"], 32768.);
        assert!(
            slim["Password"].is_null()
                && slim["Oem"].is_null()
                && slim["Status"]["Secret"].is_null()
        );
        for capacity in [json!(null), json!("NaN"), json!(-1)] {
            assert!(summary(&json!({"CapacityMiB":capacity}), "memory").is_none());
        }
        assert!(summary(&json!({}), "processors").is_none());
        assert!(summary(&json!({}), "onboard").is_none());
    }

    #[tokio::test]
    async fn partial_inventory_has_no_aggregate_count_and_failed_detail_evicts_old_data() {
        let (target, state, seen, task) = mock().await;
        let out = read(&target, MAX_REQUESTS_PER_SCRAPE - 2).await;
        assert_eq!(out.requests, MAX_REQUESTS_PER_SCRAPE);
        assert_eq!(seen.lock().unwrap().len(), 2);
        let text = metrics::render("CPU", out, 0.1);
        assert!(text.contains("redfish_dimm_health{") && !text.contains("redfish_dimm_count{"));
        assert!(text.contains("redfish_scrape_complete{target=\"CPU\"} 0"));
        let out = read(&target, 0).await;
        assert!(metrics::render("CPU", out, 0.1).contains("redfish_dimm_count{target=\"CPU\"} 2"));
        state.lock().unwrap().1 = Some(500);
        let out = read(&target, 0).await;
        let text = metrics::render("CPU", out, 0.1);
        assert!(!text.contains("redfish_dimm_health{") && !text.contains("redfish_dimm_count{"));
        assert!(target.cpu_members.lock().await.collections["memory"]
            .samples
            .is_empty());
        task.abort();
    }

    #[tokio::test]
    async fn invalid_directory_clears_cache_without_following_details() {
        let (target, state, seen, task) = mock().await;
        let member = json!({"@odata.id":format!("{ROOT}/0")});
        for directory in [
            json!({"Members":[member.clone()],"Members@odata.count":2}),
            json!({"Members":[member.clone()],"Members@odata.count":"1"}),
            json!({"Members":[member.clone()],"Members@odata.nextLink":format!("{ROOT}?$skip=1")}),
            json!({"Members":[member.clone(),member.clone()]}),
            json!({"Members":[{"@odata.id":"https://other.example/redfish/v1/Systems/Self/Memory/0"}]}),
            json!({"Members":[{"@odata.id":"/redfish/v1/Managers/Self"}]}),
            json!({"Members":[{"@odata.id":format!("{ROOT}/0?$expand=*")}]}),
            json!({"Members":(0..65).map(|i| json!({"@odata.id":format!("{ROOT}/{i}")})).collect::<Vec<_>>()}),
        ] {
            state.lock().unwrap().0 = json!({"Members":[member.clone()]});
            assert!(read(&target, 0).await.errors == 0);
            state.lock().unwrap().0 = directory;
            seen.lock().unwrap().clear();
            let out = read(&target, 0).await;
            assert!(out.errors > 0 && out.failed_groups.contains("compute"));
            assert_eq!(*seen.lock().unwrap(), vec![ROOT.to_string()]);
            assert!(target.cpu_members.lock().await.collections.is_empty());
            assert!(!metrics::render("CPU", out, 0.1).contains("redfish_dimm_count{"));
        }
        task.abort();
    }

    #[tokio::test]
    async fn changed_inventory_and_expired_observations_cannot_replay_old_values() {
        let (target, state, _, task) = mock().await;
        assert_eq!(read(&target, 0).await.errors, 0);
        state.lock().unwrap().0 =
            json!({"Members":[{"@odata.id":format!("{ROOT}/new")}],"Members@odata.count":1});
        let out = read(&target, MAX_REQUESTS_PER_SCRAPE - 1).await;
        assert!(out.errors > 0);
        let text = metrics::render("CPU", out, 0.1);
        assert!(!text.contains("redfish_dimm_health{") && !text.contains("redfish_dimm_count{"));
        assert_eq!(read(&target, 0).await.errors, 0);
        target.cpu_members.lock().await.age_by(MAX_AGE);
        let out = read(&target, MAX_REQUESTS_PER_SCRAPE - 1).await;
        assert!(out.errors > 0);
        assert!(!metrics::render("CPU", out, 0.1).contains("redfish_dimm_count{"));
        task.abort();
    }

    #[tokio::test]
    async fn forbidden_member_stops_current_round_and_next_baseline_probe() {
        let (target, state, seen, task) = mock().await;
        state.lock().unwrap().1 = Some(403);
        let out = read(&target, 0).await;
        assert!(out.blocked && out.errors > 0);
        assert_eq!(seen.lock().unwrap().len(), 2);
        seen.lock().unwrap().clear();
        let out = super::super::collect("CPU", &target, &Semaphore::new(1)).await;
        assert!(out.blocked && seen.lock().unwrap().is_empty());
        task.abort();
    }
}

impl Spec {
    pub(super) fn new(kind: &'static str, root: String, limit: usize) -> Self {
        Self { kind, root, limit }
    }
    fn group(&self) -> &'static str {
        if matches!(self.kind, "processors" | "memory") {
            "compute"
        } else {
            "network"
        }
    }
}

#[derive(Default)]
pub(super) struct Cache {
    collections: BTreeMap<&'static str, Inventory>,
}

#[cfg(test)]
impl Cache {
    pub(super) fn age_by(&mut self, age: Duration) {
        for inventory in self.collections.values_mut() {
            for snapshot in inventory.samples.values_mut() {
                snapshot.observed -= age;
            }
        }
    }
}

struct Inventory {
    root: String,
    paths: Vec<String>,
    samples: BTreeMap<String, Snapshot>,
}

struct Snapshot {
    observed: Instant,
    source_ms: u64,
    // Only whitelisted scalar fields used by CPU/NIC metric encoders.
    value: Value,
}

fn incomplete(out: &mut Collection, group: &'static str) {
    out.failed_groups.insert(group);
    out.errors = out.errors.saturating_add(1);
}

fn summary(value: &Value, kind: &str) -> Option<Value> {
    let numeric = |key: &str| {
        value[key]
            .as_f64()
            .or_else(|| value[key].as_str()?.parse::<f64>().ok())
            .filter(|v| v.is_finite())
    };
    match kind {
        "processors" if value["ProcessorType"].as_str().is_none() => return None,
        "memory" if numeric("CapacityMiB").is_none_or(|v| v < 0.) => return None,
        "onboard" | "bmc" if value["LinkStatus"].as_str().is_none() => return None,
        _ => {}
    }
    let mut out = json!({});
    for key in [
        "Name",
        "Id",
        "Socket",
        "ProcessorType",
        "MemoryDeviceType",
        "MACAddress",
        "LinkStatus",
    ] {
        if let Some(text) = value[key].as_str() {
            out[key] = json!(text.chars().take(128).collect::<String>());
        }
    }
    for key in ["CapacityMiB", "MaxSpeedMHz", "SpeedMbps"] {
        if let Some(number) = numeric(key) {
            out[key] = json!(number);
        }
    }
    if let Some(health) = value["Status"]["Health"].as_str() {
        out["Status"] = json!({"Health":health.chars().take(128).collect::<String>()});
    }
    Some(out)
}

async fn directory(
    name: &str,
    target: &Target,
    inflight: &Semaphore,
    out: &mut Collection,
    spec: &Spec,
) -> Option<Vec<String>> {
    if !safe_path(&spec.root) || spec.root.contains('?') || spec.root.len() > 256 {
        incomplete(out, spec.group());
        return None;
    }
    let value = out.fetch(name, target, inflight, &spec.root).await?;
    let valid = || -> Option<Vec<String>> {
        let members = value["Members"].as_array()?;
        if members.len() > spec.limit || !value["Members@odata.nextLink"].is_null() {
            return None;
        }
        if let Some(count) = value.get("Members@odata.count") {
            if count.as_u64()? != members.len() as u64 {
                return None;
            }
        }
        let mut paths = BTreeSet::new();
        for member in members {
            let path = member["@odata.id"].as_str()?;
            if path.len() > 384
                || !safe_path(path)
                || path.contains('?')
                || !path.starts_with(&format!("{}/", spec.root))
                || !paths.insert(path.to_owned())
            {
                return None;
            }
        }
        Some(paths.into_iter().collect())
    };
    let paths = valid();
    if paths.is_none() {
        incomplete(out, spec.group());
    }
    paths
}

pub(super) async fn collect(
    name: &str,
    target: &Target,
    inflight: &Semaphore,
    out: &mut Collection,
    specs: Vec<Spec>,
) {
    let mut cache = target.cpu_members.lock().await;
    cache
        .collections
        .retain(|kind, _| specs.iter().any(|spec| spec.kind == *kind));
    for spec in &specs {
        let Some(paths) = directory(name, target, inflight, out, spec).await else {
            // A failed directory cannot authorize old inventory or counts.
            cache.collections.remove(spec.kind);
            continue;
        };
        let inventory = cache
            .collections
            .entry(spec.kind)
            .or_insert_with(|| Inventory {
                root: spec.root.clone(),
                paths: Vec::new(),
                samples: BTreeMap::new(),
            });
        if inventory.root != spec.root || inventory.paths != paths {
            inventory.root = spec.root.clone();
            inventory.paths = paths;
            inventory.samples.clear();
        }
        inventory
            .samples
            .retain(|_, sample| sample.observed.elapsed() < MAX_AGE);
    }
    // Across all collections, missing members first, then oldest observations.
    // Sorting a global work list prevents one large DIMM collection starving NICs.
    let mut work = Vec::new();
    for (kind, inventory) in &cache.collections {
        for (index, path) in inventory.paths.iter().enumerate() {
            work.push((
                inventory.samples.get(path).map(|s| s.observed),
                index,
                *kind,
                path.clone(),
            ));
        }
    }
    work.sort();
    let reads = MAX_REQUESTS_PER_SCRAPE.saturating_sub(out.requests);
    for (_, _, kind, path) in work.into_iter().take(reads) {
        if out.blocked {
            break;
        }
        let Some(inventory) = cache.collections.get_mut(kind) else {
            continue;
        };
        match out.fetch(name, target, inflight, &path).await {
            Some(value) => match summary(&value, kind) {
                Some(value) => {
                    inventory.samples.insert(
                        path,
                        Snapshot {
                            observed: Instant::now(),
                            source_ms: (metrics::epoch() * 1000.) as u64,
                            value,
                        },
                    );
                }
                None => {
                    inventory.samples.remove(&path);
                    incomplete(
                        out,
                        if matches!(kind, "processors" | "memory") {
                            "compute"
                        } else {
                            "network"
                        },
                    );
                }
            },
            None => {
                inventory.samples.remove(&path);
            }
        }
    }
    for spec in &specs {
        let Some(inventory) = cache.collections.get_mut(spec.kind) else {
            continue;
        };
        inventory
            .samples
            .retain(|_, sample| sample.observed.elapsed() < MAX_AGE);
        let complete = inventory.paths.len() == inventory.samples.len();
        if !complete {
            incomplete(out, spec.group());
        }
        let values = inventory
            .samples
            .iter()
            .map(|(path, sample)| (path.as_str(), &sample.value, sample.source_ms))
            .collect::<Vec<_>>();
        metrics::cpu_inventory_observations(&mut out.samples, spec.kind, &values, complete);
        tracing::info!(
            target = name,
            collection = spec.kind,
            inventory = inventory.paths.len(),
            cached = inventory.samples.len(),
            complete,
            requests = out.requests,
            "bounded CPU inventory coverage"
        );
    }
}
