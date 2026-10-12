//! Bounded 1U12E sensor directory discovery and timestamp-preserving rolling reads.
//! No expansion, background session, new endpoint, or raised baseline budget.
use super::{metrics, safe_path, Collection, Metric, Target, MAX_REQUESTS_PER_SCRAPE};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const MAX_SENSORS: usize = 256;
const MAX_PAGES: usize = 8;
const READS_PER_ROUND: usize = 48;
const MAX_AGE: Duration = Duration::from_secs(1800);

#[derive(Default)]
pub(super) struct Cache {
    inventory: Vec<String>,
    cursor: usize,
    samples: BTreeMap<String, Snapshot>,
}

#[cfg(test)]
impl Cache {
    pub(super) fn age_by(&mut self, age: Duration) {
        for snapshot in self.samples.values_mut() {
            snapshot.observed -= age;
        }
    }
}

struct Snapshot {
    observed: Instant,
    samples: Vec<Metric>,
}

fn incomplete(out: &mut Collection) {
    out.failed_groups.insert("power");
    out.errors = out.errors.saturating_add(1);
}

/// Only numeric pagination parameters on the exact advertised collection.
/// In particular, a nextLink cannot switch origin/resource or enable $expand.
fn page_offset(path: &str, root: &str) -> Option<usize> {
    if path.len() > 512 || !safe_path(path) {
        return None;
    }
    if path == root {
        return Some(0);
    }
    let (resource, query) = path.split_once('?')?;
    if resource != root {
        return None;
    }
    let mut keys = BTreeSet::new();
    let mut offset = None;
    for pair in query.split('&') {
        let (key, raw) = pair.split_once('=')?;
        if !keys.insert(key)
            || raw.is_empty()
            || raw.len() > 6
            || !raw.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let number = raw.parse::<usize>().ok()?;
        match key {
            "$skip" if number > 0 && number <= MAX_SENSORS => offset = Some(number),
            "$top" if number > 0 && number <= MAX_SENSORS => {}
            _ => return None,
        }
    }
    offset
}

async fn directory(
    name: &str,
    target: &Target,
    inflight: &Semaphore,
    out: &mut Collection,
    root: &str,
) -> Option<Vec<String>> {
    if root.contains('?')
        || root.len() > 256
        || !safe_path(root)
        || !root.starts_with("/redfish/v1/Chassis/")
        || !root.ends_with("/Sensors")
    {
        incomplete(out);
        return None;
    }
    let mut next = root.to_owned();
    let mut previous_offset = None;
    let mut paths = BTreeSet::new();
    let mut advertised = None;
    for _ in 0..MAX_PAGES {
        let Some(offset) = page_offset(&next, root)
            .filter(|offset| previous_offset.is_none_or(|previous| *offset > previous))
        else {
            incomplete(out);
            return None;
        };
        previous_offset = Some(offset);
        let page = out.fetch(name, target, inflight, &next).await?;
        let Some(members) = page["Members"].as_array() else {
            incomplete(out);
            return None;
        };
        if let Some(raw) = page.get("Members@odata.count") {
            let Some(count) = raw.as_u64().filter(|count| *count <= MAX_SENSORS as u64) else {
                incomplete(out);
                return None;
            };
            if advertised.is_some_and(|previous| previous != count) {
                incomplete(out);
                return None;
            }
            advertised = Some(count);
        }
        for member in members {
            let Some(path) = member["@odata.id"].as_str().filter(|path| {
                path.starts_with(&format!("{root}/"))
                    && path.len() <= root.len() + 129
                    && safe_path(path)
                    && !path.contains('?')
            }) else {
                incomplete(out);
                return None;
            };
            if !paths.insert(path.to_owned()) || paths.len() > MAX_SENSORS {
                incomplete(out);
                return None;
            }
        }
        match page.get("Members@odata.nextLink").filter(|v| !v.is_null()) {
            Some(link) => {
                let Some(path) = link.as_str().filter(|_| !members.is_empty()) else {
                    incomplete(out);
                    return None;
                };
                next = path.to_owned();
            }
            None => {
                if advertised.is_some_and(|count| count != paths.len() as u64) {
                    incomplete(out);
                    return None;
                }
                return Some(paths.into_iter().collect());
            }
        }
    }
    incomplete(out);
    None
}

pub(super) async fn collect(
    name: &str,
    target: &Target,
    inflight: &Semaphore,
    out: &mut Collection,
    root: &str,
) {
    let Some(inventory) = directory(name, target, inflight, out, root).await else {
        return;
    };
    let mut cache = target.sensors.lock().await;
    if cache.inventory != inventory {
        cache.inventory = inventory;
        cache.cursor = 0;
        cache.samples.clear();
    }
    cache
        .samples
        .retain(|_, snapshot| snapshot.observed.elapsed() < MAX_AGE);
    let requests_before = out.requests;
    let reads = READS_PER_ROUND
        .min(cache.inventory.len().saturating_sub(cache.cursor))
        .min(MAX_REQUESTS_PER_SCRAPE.saturating_sub(out.requests));
    for _ in 0..reads {
        let path = cache.inventory[cache.cursor].clone();
        cache.cursor = (cache.cursor + 1) % cache.inventory.len();
        match out.fetch(name, target, inflight, &path).await {
            Some(value) if value["Name"].as_str().is_some_and(|name| !name.is_empty()) => {
                let id = &path[root.len() + 1..];
                cache.samples.insert(
                    path.clone(),
                    Snapshot {
                        observed: Instant::now(),
                        samples: metrics::sensor_observation(&value, id, metrics::epoch()),
                    },
                );
            }
            Some(value) => {
                cache.samples.remove(&path);
                let name_state = match value.get("Name") {
                    None => "missing",
                    Some(Value::Null) => "null",
                    Some(Value::String(_)) => "empty",
                    Some(_) => "non_string",
                };
                tracing::warn!(
                    target = name,
                    sensor_id = &path[root.len() + 1..],
                    name_state,
                    object = value.is_object(),
                    id_present = value["Id"].is_string(),
                    reading_present = !value["Reading"].is_null(),
                    health_present = value["Status"]["Health"].is_string(),
                    "sensor detail has no usable name"
                );
                incomplete(out);
            }
            None => {
                cache.samples.remove(&path);
            }
        }
        if out.blocked {
            break;
        }
    }
    cache
        .samples
        .retain(|_, snapshot| snapshot.observed.elapsed() < MAX_AGE);
    metrics::sensor_coverage(&mut out.samples, cache.inventory.len(), cache.samples.len());
    for snapshot in cache.samples.values() {
        out.samples.extend(snapshot.samples.iter().cloned());
    }
    if cache.samples.len() != cache.inventory.len() {
        incomplete(out);
    }
    tracing::info!(
        target = name,
        inventory = cache.inventory.len(),
        cached = cache.samples.len(),
        requests_before,
        read_budget = reads,
        requests_after = out.requests,
        "bounded sensor read budget"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::{StatusCode, Uri},
        response::IntoResponse,
        routing::get,
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex as StdMutex};
    const ROOT: &str = "/redfish/v1/Chassis/Self/Sensors";

    #[test]
    fn pagination_is_numeric_same_resource_and_never_expands() {
        assert_eq!(page_offset(ROOT, ROOT), Some(0));
        assert_eq!(
            page_offset(&format!("{ROOT}?$skip=50&$top=50"), ROOT),
            Some(50)
        );
        for path in [
            format!("https://other.example{ROOT}?$skip=50"),
            "/redfish/v1/Managers/Self?$skip=50".into(),
            format!("{ROOT}?$skip=50&$expand=*"),
            format!("{ROOT}?$skip=50&$skip=100"),
            format!("{ROOT}?$skip=0"),
            format!("{ROOT}?$skip=-1"),
            format!("{ROOT}?$skip=257"),
            format!("{ROOT}?$top=50"),
            format!("{ROOT}?$skip=50#ignored"),
            format!("{ROOT}?$skip=%35%30"),
        ] {
            assert_eq!(page_offset(&path, ROOT), None, "{path}");
        }
    }

    async fn mock(
        page: Value,
        fail: bool,
    ) -> (
        Target,
        Arc<StdMutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let captured = seen.clone();
        let app = Router::new().fallback(get(move |uri: Uri| {
            captured.lock().unwrap().push(uri.to_string());
            let page = page.clone();
            async move {
                if uri.path() == ROOT { return Json(page).into_response(); }
                if fail { return StatusCode::FORBIDDEN.into_response(); }
                Json(json!({"Name":uri.path().rsplit('/').next().unwrap(),"Status":{"Health":"OK"},"Reading":42,"ReadingType":"Temperature","ReadingUnits":"Cel"})).into_response()
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
            seen,
            task,
        )
    }

    fn collection() -> Collection {
        let mut out = Collection::failed();
        out.up = true;
        out.errors = 0;
        out
    }

    #[tokio::test]
    async fn invalid_or_truncated_directories_never_fetch_details_or_claim_coverage() {
        let member = json!({"@odata.id":format!("{ROOT}/0")});
        let cases = [
            json!({"Members":[member.clone()],"Members@odata.nextLink":"https://other.example/redfish/v1/Chassis/Self/Sensors?$skip=1"}),
            json!({"Members":[member.clone()],"Members@odata.nextLink":format!("{ROOT}?$skip=1&$expand=*")}),
            json!({"Members":[member.clone()],"Members@odata.nextLink":format!("{ROOT}?$skip=1")}), // repeated page/duplicate member
            json!({"Members":[member.clone(),member.clone()]}),
            json!({"Members":[member.clone()],"Members@odata.count":257}),
            json!({"Members":[member.clone()],"Members@odata.count":136}),
            json!({"Members":[{"@odata.id":"/redfish/v1/Managers/Self"}]}),
            json!({"Members":[member.clone()],"Members@odata.count":"136"}),
            json!({"Members":[],"Members@odata.nextLink":format!("{ROOT}?$skip=50")}),
        ];
        for page in cases {
            let (target, seen, task) = mock(page, false).await;
            let mut out = collection();
            collect("fixture", &target, &Semaphore::new(1), &mut out, ROOT).await;
            assert!(out.errors > 0 && out.failed_groups.contains("power"));
            let text = metrics::render("fixture", out, 0.1);
            assert!(!text.contains("redfish_sensor_inventory_count"));
            assert!(!text.contains("redfish_sensor_temperature_celsius"));
            assert!(seen
                .lock()
                .unwrap()
                .iter()
                .all(|p| p.split('?').next() == Some(ROOT)));
            task.abort();
        }
    }

    #[tokio::test]
    async fn page_limit_is_enforced_before_any_detail_reads() {
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = requests.clone();
        let app = Router::new().fallback(get(move |uri: Uri| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                let offset = uri.query().and_then(|q| q.strip_prefix("$skip="))
                    .and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                Json(json!({"Members@odata.count":9,"Members":[{"@odata.id":format!("{ROOT}/{offset}")}],"Members@odata.nextLink":format!("{ROOT}?$skip={}",offset+1)}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let target = Target::mock_profile(
            &format!("http://{addr}/"),
            super::super::Profile::Asrockrack1u12e,
        );
        let mut out = collection();
        collect("fixture", &target, &Semaphore::new(1), &mut out, ROOT).await;
        assert!(out.errors > 0 && out.samples.is_empty());
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            MAX_PAGES
        );
        task.abort();
    }

    #[tokio::test]
    async fn stale_values_are_omitted_and_remaining_baseline_budget_is_respected() {
        let inventory = (0..3).map(|i| format!("{ROOT}/{i}")).collect::<Vec<_>>();
        let (target, seen, task) = mock(json!({"Members@odata.count":3,"Members":inventory.iter().map(|path| json!({"@odata.id":path})).collect::<Vec<_>>()}), false).await;
        {
            let mut cache = target.sensors.lock().await;
            cache.inventory = inventory.clone();
            cache.cursor = 2;
            for path in &inventory {
                cache.samples.insert(path.clone(), Snapshot {
                    observed: Instant::now() - MAX_AGE,
                    samples: metrics::sensor_observation(&json!({"Name":"stale","Reading":999,"ReadingType":"Temperature","ReadingUnits":"Cel"}), "old", 1.),
                });
            }
        }
        let mut out = collection();
        out.requests = MAX_REQUESTS_PER_SCRAPE - 2;
        collect("fixture", &target, &Semaphore::new(1), &mut out, ROOT).await;
        assert_eq!(out.requests, MAX_REQUESTS_PER_SCRAPE);
        assert!(out.errors > 0);
        let text = metrics::render("fixture", out, 0.1);
        assert!(!text.contains("name=\"stale\""));
        assert!(text.contains("redfish_sensor_cached_count{target=\"fixture\"} 1"));
        assert!(text.contains("redfish_sensor_snapshot_complete{target=\"fixture\"} 0"));
        assert_eq!(seen.lock().unwrap().len(), 2);
        task.abort();
    }

    // Diagnostic reproduction of the v0.2.4 live canary: this deliberately
    // asserts truthful partial coverage, not successful fleet acceptance.
    #[tokio::test]
    async fn constrained_sensor_budget_cannot_converge_before_cache_expiry() {
        let inventory = (0..136)
            .map(|i| json!({"@odata.id":format!("{ROOT}/{i}")}))
            .collect::<Vec<_>>();
        let (target, seen, task) = mock(
            json!({"Members@odata.count":136,"Members":inventory}),
            false,
        )
        .await;
        let mut maximum_cached = 0;
        for round in 0..12 {
            if round > 0 {
                // Advance observation age only: no sleep and no live BMC.
                for snapshot in target.sensors.lock().await.samples.values_mut() {
                    snapshot.observed -= Duration::from_secs(600);
                }
            }
            seen.lock().unwrap().clear();
            let mut out = collection();
            // A single directory GET consumes the 69th request, leaving 27.
            // This models the observed remaining budget, not its device-level
            // breakdown (which is not currently exposed in live telemetry).
            out.requests = 68;
            collect("fixture", &target, &Semaphore::new(1), &mut out, ROOT).await;
            assert!(out.requests <= MAX_REQUESTS_PER_SCRAPE);
            assert!(seen.lock().unwrap().len() <= 28);
            let cached = target.sensors.lock().await.samples.len();
            maximum_cached = maximum_cached.max(cached);
            assert!(cached <= 81);
            assert!(out.failed_groups.contains("power"));
            let text = metrics::render("fixture", out, 0.1);
            assert!(text.contains("redfish_sensor_snapshot_complete{target=\"fixture\"} 0"));
        }
        assert_eq!(maximum_cached, 81);
        task.abort();
    }

    #[tokio::test]
    async fn inventory_change_invalidates_old_values_and_403_stops_this_and_next_round() {
        for fail in [false, true] {
            let (target, seen, task) = mock(
                json!({"Members@odata.count":1,"Members":[{"@odata.id":format!("{ROOT}/new")}]}),
                fail,
            )
            .await;
            {
                let mut cache = target.sensors.lock().await;
                cache.inventory = vec![format!("{ROOT}/old")];
                cache.samples.insert(
                    format!("{ROOT}/old"),
                    Snapshot {
                        observed: Instant::now(),
                        samples: metrics::sensor_observation(
                            &json!({"Name":"removed","Status":{"Health":"OK"}}),
                            "old",
                            1.,
                        ),
                    },
                );
            }
            let mut out = collection();
            collect("fixture", &target, &Semaphore::new(1), &mut out, ROOT).await;
            assert_eq!(out.blocked, fail);
            assert_eq!(
                target.sensors.lock().await.samples.len(),
                usize::from(!fail)
            );
            assert!(!metrics::render("fixture", out, 0.1).contains("removed"));
            if fail {
                let blocked = super::super::collect("fixture", &target, &Semaphore::new(1)).await;
                assert!(blocked.blocked);
                assert_eq!(seen.lock().unwrap().len(), 2);
            }
            task.abort();
        }
    }
}
