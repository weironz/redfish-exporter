//! Standalone, read-only multi-model Redfish exporter.
mod bmc;
mod metrics;

use anyhow::{bail, Context, Result};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
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

async fn scrape(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(name) = q.get("target") else {
        return (StatusCode::BAD_REQUEST, "missing target").into_response();
    };
    let Some(target) = app.targets.get(name) else {
        return (StatusCode::NOT_FOUND, "unknown target").into_response();
    };
    // A slow prior scrape must never open a second BMC session for the same target.
    let Ok(_guard) = target.scrape_lock.try_lock() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "scrape already running").into_response();
    };
    let start = std::time::Instant::now();
    let result = match tokio::time::timeout(
        Duration::from_secs(140),
        bmc::collect(name, target, &app.inflight),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            target.note_timeout().await;
            bmc::Collection::failed()
        }
    };
    let body = metrics::render(name, result, start.elapsed().as_secs_f64());
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
    };
    tracing::info!(targets = app.targets.len(), %bind, "read-only Redfish exporter starting");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(
        listener,
        Router::new()
            .route("/healthz", get(healthz))
            .route("/metrics", get(scrape))
            .with_state(app),
    )
    .await?;
    Ok(())
}
