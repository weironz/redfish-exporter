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
    invalid: BTreeMap<String, Snapshot>,
}

#[cfg(test)]
impl Cache {
    pub(super) fn age_by(&mut self, age: Duration) {
        for snapshot in self.samples.values_mut() {
            snapshot.observed -= age;
        }
        for snapshot in self.invalid.values_mut() {
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
        cache.invalid.clear();
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
            Some(value)
                if value["Name"]
                    .as_str()
                    .is_some_and(|name| !name.trim().is_empty()) =>
            {
                let id = &path[root.len() + 1..];
                cache.invalid.remove(&path);
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
                let (name_state, reason) = match value.get("Name") {
                    None => ("missing", "name_missing"),
                    Some(Value::Null) => ("null", "name_null"),
                    Some(Value::String(_)) => ("empty", "name_empty"),
                    Some(_) => ("non_string", "name_non_string"),
                };
                cache.invalid.insert(
                    path.clone(),
                    Snapshot {
                        observed: Instant::now(),
                        samples: metrics::sensor_invalid_observation(
                            &value,
                            &path[root.len() + 1..],
                            reason,
                            metrics::epoch(),
                        ),
                    },
                );
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
            }
            None => {
                cache.samples.remove(&path);
                cache.invalid.remove(&path);
            }
        }
        if out.blocked {
            break;
        }
    }
    cache
        .samples
        .retain(|_, snapshot| snapshot.observed.elapsed() < MAX_AGE);
    cache
        .invalid
        .retain(|_, snapshot| snapshot.observed.elapsed() < MAX_AGE);
    metrics::sensor_coverage(&mut out.samples, cache.inventory.len(), cache.samples.len());
    metrics::sensor_detail_coverage(
        &mut out.samples,
        cache.inventory.len(),
        cache.samples.len(),
        cache.invalid.len(),
    );
    for snapshot in cache.samples.values() {
        out.samples.extend(snapshot.samples.iter().cloned());
    }
    for snapshot in cache.invalid.values() {
        out.samples.extend(snapshot.samples.iter().cloned());
    }
    // Invalid resource evidence remains an error while fresh, including rounds
    // that replay it rather than read that member. Read completeness is not
    // usable coverage and must never promote the power group to healthy.
    if !cache.invalid.is_empty() {
        out.failed_groups.insert("power");
        // Directory validation caps this map at MAX_SENSORS (256).
        out.errors = out.errors.saturating_add(cache.invalid.len() as u32);
    }
    if cache.samples.len() != cache.inventory.len() {
        incomplete(out);
    }
    tracing::info!(
        target = name,
        inventory = cache.inventory.len(),
        cached = cache.samples.len(),
        invalid = cache.invalid.len(),
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

    // Reproduce the live response shape without altering directory size or
    // promoting successful HTTP reads to usable sensor observations.
    #[tokio::test]
    async fn empty_details_remain_diagnosed_partial_across_natural_rounds() {
        let seen = Arc::new(StdMutex::new(Vec::<String>::new()));
        let captured = seen.clone();
        let app = Router::new().fallback(get(move |uri: Uri| {
            captured.lock().unwrap().push(uri.to_string());
            async move {
                if uri.path() == ROOT {
                    let paths = (0..133).filter(|id| *id != 99).map(|id| id.to_string())
                        .chain(["200", "207", "232", "233"].map(str::to_owned))
                        .map(|id| format!("{ROOT}/{id}")).collect::<BTreeSet<_>>()
                        .into_iter().collect::<Vec<_>>();
                    let skip = uri.query().and_then(|q| q.strip_prefix("$skip="))
                        .and_then(|q| q.parse::<usize>().ok()).unwrap_or(0);
                    let end = (skip + 50).min(paths.len());
                    let mut page = json!({"Members@odata.count":136,"Members":paths[skip..end].iter().map(|p|json!({"@odata.id":p})).collect::<Vec<_>>()});
                    if end < paths.len() {
                        page["Members@odata.nextLink"] = json!(format!("{ROOT}?$skip={end}"));
                    }
                    return Json(page);
                }
                let id = uri.path().rsplit('/').next().unwrap();
                if ["207", "232", "233"].contains(&id) {
                    return Json(json!({"Id":id,"Name":""}));
                }
                Json(json!({"Name":"same display name","Reading":42,"ReadingType":"Temperature","ReadingUnits":"Cel","Status":{"Health":"OK"}}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let target = Target::mock_profile(
            &format!("http://{addr}/"),
            super::super::Profile::Asrockrack1u12e,
        );
        let mut first_diagnostic = None;
        for round in 0..4 {
            if round > 0 {
                target.sensors.lock().await.age_by(Duration::from_secs(600));
            }
            seen.lock().unwrap().clear();
            let mut out = collection();
            collect("fixture", &target, &Semaphore::new(1), &mut out, ROOT).await;
            assert!(out.requests <= MAX_REQUESTS_PER_SCRAPE);
            assert!(seen.lock().unwrap().len() <= 51);
            if round >= 1 {
                assert!(out.failed_groups.contains("power"));
                assert!(out.errors > 0);
            }
            let text = metrics::render("fixture", out, 0.1);
            let valid = [48, 93, 133, 133][round];
            let read = [48, 96, 136, 136][round];
            let invalid = if round == 0 { 0 } else { 3 };
            for (metric, count) in [
                ("redfish_sensor_inventory_count", 136),
                ("redfish_sensor_cached_count", valid),
                ("redfish_sensor_detail_count", read),
                ("redfish_sensor_invalid_count", invalid),
            ] {
                assert!(
                    text.contains(&format!("{metric}{{target=\"fixture\"}} {count}")),
                    "round {round}: missing {metric}={count}"
                );
            }
            assert!(text.contains("redfish_sensor_snapshot_complete{target=\"fixture\"} 0"));
            assert!(text.contains(&format!(
                "redfish_sensor_detail_complete{{target=\"fixture\"}} {}",
                usize::from(round >= 2)
            )));
            if round >= 1 {
                let diagnostic = text
                    .lines()
                    .find(|line| {
                        line.starts_with("redfish_sensor_invalid_info{")
                            && line.contains("sensor_id=\"207\"")
                    })
                    .unwrap();
                assert!(diagnostic.contains("reason=\"name_empty\""));
                assert!(!text.lines().any(|line| line
                    .starts_with("redfish_sensor_temperature_celsius{")
                    && line.contains("sensor_id=\"207\"")));
                if round == 1 {
                    first_diagnostic = Some(diagnostic.to_owned());
                } else {
                    assert_eq!(
                        Some(diagnostic),
                        first_diagnostic.as_deref(),
                        "cache replay must retain diagnostic source time"
                    );
                }
            }
        }
        task.abort();
    }

    #[tokio::test]
    async fn invalid_detail_diagnostics_expire_recover_and_invalidate_with_directory() {
        let response = Arc::new(StdMutex::new(json!({"Id":"207","Name":""})));
        let removed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fail_read = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let current = response.clone();
        let absent = removed.clone();
        let failing = fail_read.clone();
        let app = Router::new().fallback(get(move |uri: Uri| {
            let value = current.lock().unwrap().clone();
            let empty = absent.load(std::sync::atomic::Ordering::SeqCst);
            let failed = failing.load(std::sync::atomic::Ordering::SeqCst);
            async move {
                if uri.path() != ROOT && failed {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Json(if uri.path() == ROOT {
                    if empty { json!({"Members":[],"Members@odata.count":0}) }
                    else { json!({"Members":[{"@odata.id":format!("{ROOT}/207")}],"Members@odata.count":1}) }
                } else { value }).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let target = Target::mock_profile(
            &format!("http://{addr}/"),
            super::super::Profile::Asrockrack1u12e,
        );
        for (value, reason) in [
            (json!({"Id":"207"}), "name_missing"),
            (json!({"Name":null}), "name_null"),
            (json!({"Name":" \t "}), "name_empty"),
            (
                json!({"Name":{"private":"do-not-export"},"Password":"do-not-export"}),
                "name_non_string",
            ),
            (json!("do-not-export"), "name_missing"),
        ] {
            *response.lock().unwrap() = value;
            let mut out = collection();
            collect("fixture", &target, &Semaphore::new(1), &mut out, ROOT).await;
            let text = metrics::render("fixture", out, 0.1);
            assert!(text.contains(&format!("reason=\"{reason}\"")));
            assert!(!text.contains("do-not-export"));
            assert!(text.contains("redfish_sensor_cached_count{target=\"fixture\"} 0"));
            assert!(text.contains("redfish_sensor_detail_complete{target=\"fixture\"} 1"));
            assert!(text.contains("redfish_scrape_complete{target=\"fixture\"} 0"));
        }
        target.sensors.lock().await.age_by(MAX_AGE);
        let mut expired = collection();
        expired.requests = MAX_REQUESTS_PER_SCRAPE - 1; // Directory only, no detail refresh.
        collect("fixture", &target, &Semaphore::new(1), &mut expired, ROOT).await;
        let text = metrics::render("fixture", expired, 0.1);
        assert!(!text.contains("redfish_sensor_invalid_info"));
        assert!(text.contains("redfish_sensor_detail_count{target=\"fixture\"} 0"));
        assert!(text.contains("redfish_sensor_detail_complete{target=\"fixture\"} 0"));
        *response.lock().unwrap() = json!({"Name":"real-name","Status":{"Health":"OK"}});
        let mut recovered = collection();
        collect("fixture", &target, &Semaphore::new(1), &mut recovered, ROOT).await;
        let text = metrics::render("fixture", recovered, 0.1);
        assert!(!text.contains("redfish_sensor_invalid_info"));
        assert!(text.contains("redfish_sensor_invalid_count{target=\"fixture\"} 0"));
        assert!(text.contains("redfish_sensor_snapshot_complete{target=\"fixture\"} 1"));
        *response.lock().unwrap() = json!({"Name":""});
        let mut failed = collection();
        collect("fixture", &target, &Semaphore::new(1), &mut failed, ROOT).await;
        assert_eq!(target.sensors.lock().await.invalid.len(), 1);
        fail_read.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut failed_get = collection();
        collect(
            "fixture",
            &target,
            &Semaphore::new(1),
            &mut failed_get,
            ROOT,
        )
        .await;
        assert!(target.sensors.lock().await.invalid.is_empty());
        assert!(
            !metrics::render("fixture", failed_get, 0.1).contains("redfish_sensor_invalid_info")
        );
        fail_read.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut invalid_again = collection();
        collect(
            "fixture",
            &target,
            &Semaphore::new(1),
            &mut invalid_again,
            ROOT,
        )
        .await;
        assert_eq!(target.sensors.lock().await.invalid.len(), 1);
        removed.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut changed = collection();
        collect("fixture", &target, &Semaphore::new(1), &mut changed, ROOT).await;
        assert!(target.sensors.lock().await.invalid.is_empty());
        assert!(!metrics::render("fixture", changed, 0.1).contains("redfish_sensor_invalid_info"));
        task.abort();
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
