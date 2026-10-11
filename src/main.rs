//! Standalone, read-only multi-model Redfish exporter.
mod bmc;
mod capabilities;
mod metrics;

use anyhow::{bail, Context, Result};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::{collections::HashMap, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetsFile {
    targets: HashMap<String, bmc::TargetConfig>,
}

#[derive(Clone)]
struct App {
    targets: Arc<HashMap<String, Arc<bmc::Target>>>,
    inflight: Arc<Semaphore>,
    slow_inflight: Arc<Semaphore>,
}

fn load_targets(path: &Path) -> Result<HashMap<String, Arc<bmc::Target>>> {
    let metadata = std::fs::metadata(path).with_context(|| format!("targets file {path:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("targets file must not be readable by group or others");
        }
    }
    if metadata.len() > 256 * 1024 {
        bail!("targets file exceeds 256 KiB");
    }
    let file: TargetsFile = serde_json::from_slice(&std::fs::read(path)?)?;
    if file.targets.is_empty() || file.targets.len() > 256 {
        bail!("targets file must contain 1..=256 targets");
    }
    file.targets
        .into_iter()
        .map(|(name, config)| {
            if name.is_empty()
                || name.len() > 96
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            {
                bail!("invalid target name");
            }
            Ok((name, Arc::new(bmc::Target::new(config)?)))
        })
        .collect()
}

async fn healthz() -> &'static str {
    "ok\n"
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingCheck {
    target: String,
    address: String,
    profile: String,
    credential_ref: String,
    #[serde(default)]
    slow_drive_wear: bool,
}
/// Local config comparison only; deliberately never opens a BMC session.
async fn binding_check(State(app): State<App>, Json(check): Json<BindingCheck>) -> Json<bool> {
    Json(app.targets.get(&check.target).is_some_and(|target| {
        target.binding_matches(&check.address, &check.profile, &check.credential_ref)
            && (!check.slow_drive_wear || target.slow_drive_wear)
    }))
}
async fn capability(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    match q.get("target").and_then(|alias| app.targets.get(alias)) {
        Some(target) => axum::Json(target.capabilities()).into_response(),
        None => (StatusCode::NOT_FOUND, "configured target alias required").into_response(),
    }
}

async fn scrape(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(name) = q.get("target") else {
        return (StatusCode::BAD_REQUEST, "missing target").into_response();
    };
    let Some(target) = app.targets.get(name) else {
        return (StatusCode::NOT_FOUND, "unknown target").into_response();
    };
    // A slow prior scrape must never open a second BMC session for the same target.
    let Ok(_fast_guard) = target.fast_lock.try_lock() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "scrape already running").into_response();
    };
    let start = std::time::Instant::now();
    let result = match tokio::time::timeout(Duration::from_secs(140), async {
        let _guard = target.scrape_lock.lock().await;
        bmc::collect(name, target, &app.inflight).await
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            target.note_timeout().await;
            bmc::Collection::failed()
        }
    };
    let schedule_slow = result.up && !result.blocked && result.errors == 0;
    let body = metrics::render(name, result, start.elapsed().as_secs_f64());
    if target.slow_drive_wear && schedule_slow {
        let mut due = target.slow_due.lock().await;
        if due.is_none_or(|last| last.elapsed() >= Duration::from_secs(1800)) {
            *due = Some(std::time::Instant::now());
            let target = Arc::clone(target);
            let name = name.clone();
            let inflight = Arc::clone(&app.slow_inflight);
            tokio::spawn(async move {
                let Ok(_slow_guard) = target.slow_lock.try_lock() else {
                    return;
                };
                let result = tokio::time::timeout(
                    Duration::from_secs(90),
                    bmc::collect_drive_wear(&name, &target, &inflight),
                )
                .await
                .unwrap_or_else(|_| bmc::Collection::failed());
                *target.slow_snapshot.lock().await = Some(metrics::render_slow(&name, result));
            });
        }
    }
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

async fn slow_metrics(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(target) = q.get("target").and_then(|name| app.targets.get(name)) else {
        return (StatusCode::NOT_FOUND, "configured target alias required").into_response();
    };
    if !target.slow_drive_wear {
        return (StatusCode::NOT_FOUND, "slow group disabled").into_response();
    }
    let body = target
        .slow_snapshot
        .lock()
        .await
        .clone()
        .unwrap_or_else(|| {
            "# TYPE redfish_group_success gauge\nredfish_group_success{group=\"drive_wear\"} 0\n"
                .into()
        });
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!(
            "redfish-exporter v{} ({})",
            env!("CARGO_PKG_VERSION"),
            std::env::var("REDFISH_EXPORTER_BUILD_SHA").unwrap_or_else(|_| "unknown".into())
        );
        return Ok(());
    }
    tracing_subscriber::fmt::init();
    let path = std::env::var("REDFISH_EXPORTER_TARGETS_FILE")
        .unwrap_or_else(|_| "/run/secrets/redfish-targets.json".into());
    let bind: SocketAddr = std::env::var("REDFISH_EXPORTER_BIND")
        .unwrap_or_else(|_| "127.0.0.1:19626".into())
        .parse()
        .context("invalid REDFISH_EXPORTER_BIND")?;
    if !bind.ip().is_loopback() {
        bail!("exporter must bind to loopback");
    }
    let app = App {
        targets: Arc::new(load_targets(Path::new(&path))?),
        inflight: Arc::new(Semaphore::new(4)),
        slow_inflight: Arc::new(Semaphore::new(1)),
    };
    tracing::info!(targets = app.targets.len(), %bind, "read-only Redfish exporter starting");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(
        listener,
        Router::new()
            .route("/healthz", get(healthz))
            .route("/capabilities", get(capability))
            .route("/binding-check", post(binding_check))
            .route("/metrics", get(scrape))
            .route("/slow-metrics", get(slow_metrics))
            .layer(axum::extract::DefaultBodyLimit::max(4096))
            .with_state(app),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn slow_endpoint_is_cache_only_even_when_bmc_lock_is_held() {
        let target = Arc::new(bmc::Target::new(serde_json::from_value(serde_json::json!({
            "address":"10.0.0.1","username":"fixture","password":"fixture", "slow_drive_wear":true
        })).unwrap()).unwrap());
        let _guard = target.scrape_lock.lock().await;
        let app = App {
            targets: Arc::new(HashMap::from([("fixture".into(), target.clone())])),
            inflight: Arc::new(Semaphore::new(1)),
            slow_inflight: Arc::new(Semaphore::new(1)),
        };
        let response = tokio::time::timeout(
            Duration::from_millis(100),
            slow_metrics(
                State(app),
                Query(HashMap::from([("target".into(), "fixture".into())])),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(target.slow_due.lock().await.is_none());
    }
    #[tokio::test]
    async fn binding_check_is_exact_fail_closed_and_never_probes_bmc() {
        let target = bmc::Target::new(
            serde_json::from_value(serde_json::json!({
                "address":"10.0.0.1","username":"fixture","password":"fixture-only",
                "credential_ref":"rack-a","profile":"tyan_b8037"
            }))
            .unwrap(),
        )
        .unwrap();
        let app = App {
            targets: Arc::new(HashMap::from([("cpu-a".into(), Arc::new(target))])),
            inflight: Arc::new(Semaphore::new(1)),
            slow_inflight: Arc::new(Semaphore::new(1)),
        };
        let check = || BindingCheck {
            target: "cpu-a".into(),
            address: "10.0.0.1".into(),
            profile: "tyan_b8037".into(),
            credential_ref: "rack-a".into(),
            slow_drive_wear: false,
        };
        assert!(binding_check(State(app.clone()), Json(check())).await.0);
        let mut wrong = check();
        wrong.address = "10.0.0.2".into();
        assert!(!binding_check(State(app.clone()), Json(wrong)).await.0);
        let mut wrong = check();
        wrong.credential_ref = "rack-b".into();
        assert!(!binding_check(State(app.clone()), Json(wrong)).await.0);
        let mut wrong = check();
        wrong.profile = "asrockrack_1u10e".into();
        assert!(!binding_check(State(app.clone()), Json(wrong)).await.0);
        let mut wrong = check();
        wrong.target = "cpu-b".into();
        assert!(!binding_check(State(app), Json(wrong)).await.0);
    }
}
