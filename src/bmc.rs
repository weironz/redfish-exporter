use crate::metrics::{self, Metric};
use anyhow::{bail, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::{
    net::Ipv4Addr,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};

const MAX_BODY: usize = 4 * 1024 * 1024;
const BACKOFF_BASE: u64 = 180;
const BACKOFF_MAX: u64 = 1800;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    address: String,
    username: String,
    password: String,
    #[serde(default)]
    insecure_tls: bool,
}

#[derive(Default)]
struct Circuit {
    until: Option<Instant>,
    forbidden: bool,
    strikes: u32,
    timeouts: u32,
}

pub struct Target {
    base: reqwest::Url,
    username: String,
    password: String,
    client: reqwest::Client,
    pub scrape_lock: Mutex<()>,
    circuit: Mutex<Circuit>,
}

#[derive(Debug)]
enum FetchError {
    Http(u16),
    Timeout,
    Transport,
    InvalidJson,
    TooLarge,
}

impl FetchError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Http(403) => "http_403",
            Self::Http(_) => "http_error",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::InvalidJson => "invalid_json",
            Self::TooLarge => "body_too_large",
        }
    }
}

impl Target {
    pub fn new(config: TargetConfig) -> Result<Self> {
        let address: Ipv4Addr = config.address.parse()?;
        if !address.is_private()
            || address.is_loopback()
            || address.is_link_local()
            || address.is_unspecified()
            || config.username.is_empty()
            || config.password.is_empty()
        {
            bail!("invalid private BMC target or missing credentials");
        }
        let base = reqwest::Url::parse(&format!("https://{address}/"))?;
        Self::with_base(config, base)
    }

    fn with_base(config: TargetConfig, base: reqwest::Url) -> Result<Self> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(config.insecure_tls)
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(0)
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            base,
            username: config.username,
            password: config.password,
            client,
            scrape_lock: Mutex::new(()),
            circuit: Mutex::new(Circuit::default()),
        })
    }

    #[cfg(test)]
    fn mock(base: &str) -> Self {
        Self::with_base(
            TargetConfig {
                address: "10.0.0.1".into(),
                username: "test".into(),
                password: "secret".into(),
                insecure_tls: false,
            },
            reqwest::Url::parse(base).expect("mock URL"),
        )
        .expect("mock target")
    }

    async fn get(&self, path: &str, inflight: &Semaphore) -> Result<Value, FetchError> {
        if !safe_path(path) {
            return Err(FetchError::Transport);
        }
        let url = self.base.join(path).map_err(|_| FetchError::Transport)?;
        let _permit = inflight
            .acquire()
            .await
            .map_err(|_| FetchError::Transport)?;
        let response = self
            .client
            .get(url)
            .basic_auth(&self.username, Some(&self.password))
            .header(reqwest::header::CONNECTION, "close")
            .send()
            .await
            .map_err(classify_request)?;
        if !response.status().is_success() {
            return Err(FetchError::Http(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BODY as u64)
        {
            return Err(FetchError::TooLarge);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(classify_request)?;
            if body.len().saturating_add(chunk.len()) > MAX_BODY {
                return Err(FetchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| FetchError::InvalidJson)
    }

    async fn blocked(&self) -> Option<bool> {
        let state = self.circuit.lock().await;
        state
            .until
            .filter(|until| *until > Instant::now())
            .map(|_| state.forbidden)
    }

    async fn note_forbidden(&self) {
        let mut state = self.circuit.lock().await;
        state.strikes = state.strikes.saturating_add(1);
        state.timeouts = 0;
        state.forbidden = true;
        state.until = Some(Instant::now() + backoff(state.strikes));
    }

    pub async fn note_timeout(&self) {
        let mut state = self.circuit.lock().await;
        state.timeouts = state.timeouts.saturating_add(1);
        if state.timeouts >= 2 {
            state.strikes = state.strikes.saturating_add(1);
            state.forbidden = false;
            state.until = Some(Instant::now() + backoff(state.strikes));
        }
    }

    async fn clear_circuit(&self) {
        *self.circuit.lock().await = Circuit::default();
    }
}

fn backoff(strikes: u32) -> Duration {
    Duration::from_secs(
        BACKOFF_BASE
            .saturating_mul(1_u64 << strikes.saturating_sub(1).min(4))
            .min(BACKOFF_MAX),
    )
}

fn classify_request(error: reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Transport
    }
}

fn safe_path(path: &str) -> bool {
    path.starts_with("/redfish/v1/")
        && !path.contains("..")
        && path
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/_-.$?=()*".contains(&c))
}

fn refs(value: &Value, key: &str, max: usize) -> Vec<String> {
    value[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["@odata.id"].as_str())
        .filter(|path| safe_path(path))
        .take(max)
        .map(str::to_owned)
        .collect()
}

pub struct Collection {
    pub samples: Vec<Metric>,
    pub up: bool,
    pub blocked: bool,
    pub errors: u32,
}

impl Collection {
    pub fn failed() -> Self {
        Self {
            samples: Vec::new(),
            up: false,
            blocked: false,
            errors: 1,
        }
    }

    async fn fetch(
        &mut self,
        name: &str,
        target: &Target,
        inflight: &Semaphore,
        path: &str,
    ) -> Option<Value> {
        if self.blocked {
            return None;
        }
        match target.get(path, inflight).await {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(
                    target = name,
                    reason = error.reason(),
                    "Redfish read failed"
                );
                self.errors = self.errors.saturating_add(1);
                if matches!(error, FetchError::Http(403)) {
                    target.note_forbidden().await;
                    self.blocked = true;
                }
                None
            }
        }
    }
}

pub async fn collect(name: &str, target: &Target, inflight: &Semaphore) -> Collection {
    if let Some(forbidden) = target.blocked().await {
        return Collection {
            samples: Vec::new(),
            up: forbidden,
            blocked: forbidden,
            errors: 1,
        };
    }
    let mut out = Collection {
        samples: Vec::new(),
        up: false,
        blocked: false,
        errors: 0,
    };
    let probe = tokio::time::timeout(
        Duration::from_secs(10),
        target.get("/redfish/v1/Systems/System_0", inflight),
    )
    .await;
    let system = match probe {
        Ok(Ok(value)) => value,
        Ok(Err(FetchError::Http(403))) => {
            target.note_forbidden().await;
            out.up = true;
            out.blocked = true;
            out.errors = 1;
            return out;
        }
        Err(_) | Ok(Err(FetchError::Timeout)) => {
            target.note_timeout().await;
            out.errors = 1;
            return out;
        }
        Ok(Err(FetchError::Http(_))) => {
            out.up = true;
            out.errors = 1;
            return out;
        }
        Ok(Err(_)) => {
            out.errors = 1;
            return out;
        }
    };
    target.clear_circuit().await;
    out.up = true;
    metrics::system(&mut out.samples, &system);

    // One request at a time per BMC. Do not parallelize these: AMI returns 403
    // and may block the caller's IP when the management controller is crowded.
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/Systems/System_0/Processors?$expand=*($levels=1)",
        )
        .await
    {
        metrics::processors(&mut out.samples, &v);
    }
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/Systems/System_0/Memory?$expand=*($levels=1)",
        )
        .await
    {
        metrics::memory(&mut out.samples, &v);
    }
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/Systems/System_0/Storage?$expand=*($levels=1)",
        )
        .await
    {
        let mut drive_refs = Vec::new();
        for unit in v["Members"].as_array().into_iter().flatten() {
            drive_refs.extend(refs(unit, "Drives", 32));
        }
        metrics::storage(&mut out.samples, &drive_refs);
        for reference in drive_refs.iter().take(32) {
            if let Some(drive) = out.fetch(name, target, inflight, reference).await {
                metrics::drive(&mut out.samples, &drive);
            }
        }
    }
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/Systems/System_0/EthernetInterfaces?$expand=*($levels=1)",
        )
        .await
    {
        metrics::nics(&mut out.samples, &v, "onboard");
    }
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/Managers/BMC_0/EthernetInterfaces?$expand=*($levels=1)",
        )
        .await
    {
        metrics::nics(&mut out.samples, &v, "bmc");
    }
    if let Some(v) = out
        .fetch(name, target, inflight, "/redfish/v1/Managers/BMC_0")
        .await
    {
        metrics::manager(&mut out.samples, &v);
    }
    if let Some(v) = out
        .fetch(name, target, inflight, "/redfish/v1/Chassis/BMC_0/Thermal")
        .await
    {
        metrics::thermal(&mut out.samples, &v);
    }
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/Chassis/BMC_0/PCIeDevices",
        )
        .await
    {
        metrics::pcie(&mut out.samples, &v);
    }
    if let Some(v) = out
        .fetch(name, target, inflight, "/redfish/v1/Chassis/HGX_Chassis_0")
        .await
    {
        metrics::gpu_board(&mut out.samples, &v);
    }
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/Chassis/HGX_Chassis_0/EnvironmentMetrics",
        )
        .await
    {
        metrics::board_power(&mut out.samples, &v);
    }
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/Systems/HGX_Baseboard_0/Processors",
        )
        .await
    {
        let gpu_refs: Vec<String> = refs(&v, "Members", 16)
            .into_iter()
            .filter(|path| {
                path.rsplit('/')
                    .next()
                    .is_some_and(|id| id.starts_with("GPU_"))
            })
            .collect();
        metrics::gpu_slots(&mut out.samples, gpu_refs.len());
        let mut gpu_details = Vec::new();
        for reference in &gpu_refs {
            if let Some(gpu) = out.fetch(name, target, inflight, reference).await {
                gpu_details.push((reference.as_str(), gpu));
            }
        }
        metrics::gpu_details(&mut out.samples, &gpu_details, gpu_refs.len());
    }
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/TelemetryService/MetricReports/HGX_PlatformEnvironmentMetrics_0",
        )
        .await
    {
        metrics::gpu_environment(&mut out.samples, &v);
    }
    if let Some(v) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/TelemetryService/MetricReports/HGX_MemoryMetrics_0",
        )
        .await
    {
        metrics::gpu_memory(&mut out.samples, &v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::{StatusCode, Uri},
        routing::get,
        Json, Router,
    };
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn rejects_ssrf_and_unsafe_references() {
        for address in [
            "127.0.0.1",
            "169.254.169.254",
            "192.0.2.1",
            "example.com",
            "10.0.0.1:443",
        ] {
            assert!(Target::new(TargetConfig {
                address: address.into(),
                username: "a".into(),
                password: "b".into(),
                insecure_tls: true
            })
            .is_err());
        }
        for path in [
            "https://example.com/redfish/v1/X",
            "/redfish/v1/../admin",
            "//evil/redfish/v1/X",
            "/redfish/v1/X%2fY",
        ] {
            assert!(!safe_path(path));
        }
    }

    #[tokio::test]
    async fn a_403_stops_requests_and_enters_quiet_backoff() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().fallback(get(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                StatusCode::FORBIDDEN
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listen");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });
        let target = Target::mock(&format!("http://{addr}/"));
        let inflight = Semaphore::new(4);
        let first = collect("B300-01", &target, &inflight).await;
        let second = collect("B300-01", &target, &inflight).await;
        assert!(first.blocked && second.blocked);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(first.samples.is_empty() && second.samples.is_empty());
    }

    #[tokio::test]
    async fn representative_b300_is_scraped_sequentially_and_only_with_get() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let a = active.clone();
        let m = maximum.clone();
        let app = Router::new().fallback(get(move |uri: Uri| {
            let a = a.clone();
            let m = m.clone();
            async move {
                let n = a.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(1)).await;
                a.fetch_sub(1, Ordering::SeqCst);
                let v = match uri.path() {
                    "/redfish/v1/Systems/System_0" => json!({"PowerState":"On","Status":{"HealthRollup":"OK"}}),
                    "/redfish/v1/Systems/HGX_Baseboard_0/Processors" => json!({"Members":[{"@odata.id":"/redfish/v1/Systems/HGX_Baseboard_0/Processors/GPU_0"}]}),
                    "/redfish/v1/Systems/HGX_Baseboard_0/Processors/GPU_0" => json!({"Status":{"State":"Enabled"},"MemorySummary":{"TotalMemorySizeMiB":288000}}),
                    "/redfish/v1/TelemetryService/MetricReports/HGX_PlatformEnvironmentMetrics_0" => json!({"MetricValues":[{"MetricProperty":"/HGX_GPU_0_TEMP_0","MetricValue":"51"},{"MetricProperty":"/HGX_GPU_0_DRAM_0_TEMP_0","MetricValue":"75"}]}),
                    _ => json!({"Members":[]}),
                };
                Json(v)
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let target = Target::mock(&format!("http://{addr}/"));
        let result = collect("B300-01", &target, &Semaphore::new(4)).await;
        assert!(result.up && !result.blocked && result.errors == 0);
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
        let output = metrics::render("B300-01", result, 0.1);
        assert!(output.contains("redfish_gpu_count{target=\"B300-01\"} 1"));
        assert!(output.contains("redfish_gpu_online_count{target=\"B300-01\"} 1"));
        assert!(
            output.contains("redfish_gpu_temperature_celsius{target=\"B300-01\",gpu=\"GPU_0\"} 51")
        );
        assert!(!output.contains("75"));
    }
}
