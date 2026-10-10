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
const MAX_REQUESTS_PER_SCRAPE: usize = 96;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
enum Profile {
    #[default]
    #[serde(rename = "asrockrack_b300_8u16x")]
    AsrockrackB3008u16x,
    #[serde(rename = "tyan_b8037")]
    TyanB8037,
    #[serde(rename = "asrockrack_1u10e")]
    Asrockrack1u10e,
}

impl Profile {
    fn id(self) -> &'static str {
        match self {
            Self::AsrockrackB3008u16x => "asrockrack_b300_8u16x",
            Self::TyanB8037 => "tyan_b8037",
            Self::Asrockrack1u10e => "asrockrack_1u10e",
        }
    }

    fn model(self) -> &'static str {
        match self {
            Self::AsrockrackB3008u16x => "8U16X-GNR2",
            Self::TyanB8037 => "B8037T73X4-200PE4HR-L",
            Self::Asrockrack1u10e => "1U10E-ICX2",
        }
    }

    fn device_type(self) -> &'static str {
        match self {
            Self::AsrockrackB3008u16x => "gpu_server",
            Self::TyanB8037 | Self::Asrockrack1u10e => "cpu_server",
        }
    }

    fn system_path(self) -> &'static str {
        match self {
            Self::AsrockrackB3008u16x => "/redfish/v1/Systems/System_0",
            Self::TyanB8037 | Self::Asrockrack1u10e => "/redfish/v1/Systems/Self",
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    address: String,
    /// IDC-local reference, not a password. Required only by identity-v2 binding checks.
    #[serde(default)]
    credential_ref: Option<String>,
    username: String,
    password: String,
    #[serde(default)]
    insecure_tls: bool,
    /// Legacy files without a profile remain B300-only. Other models must opt in.
    #[serde(default)]
    profile: Profile,
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
    address: String,
    credential_ref: Option<String>,
    username: String,
    password: String,
    client: reqwest::Client,
    pub scrape_lock: Mutex<()>,
    circuit: Mutex<Circuit>,
    profile: Profile,
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
    pub fn binding_matches(&self, address: &str, profile: &str, credential_ref: &str) -> bool {
        self.address == address
            && self.profile.id() == profile
            && self.credential_ref.as_deref() == Some(credential_ref)
            && !credential_ref.is_empty()
    }
    pub fn capabilities(&self) -> Value {
        crate::capabilities::profile(
            self.profile.id(),
            self.profile.model(),
            self.profile.device_type(),
        )
    }
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
            address: config.address,
            credential_ref: config.credential_ref,
            username: config.username,
            password: config.password,
            client,
            scrape_lock: Mutex::new(()),
            circuit: Mutex::new(Circuit::default()),
            profile: config.profile,
        })
    }

    #[cfg(test)]
    fn mock(base: &str) -> Self {
        Self::mock_profile(base, Profile::AsrockrackB3008u16x)
    }

    #[cfg(test)]
    fn mock_profile(base: &str, profile: Profile) -> Self {
        Self::with_base(
            TargetConfig {
                address: "10.0.0.1".into(),
                credential_ref: None,
                username: "test".into(),
                password: "secret".into(),
                insecure_tls: false,
                profile,
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
    requests: usize,
    budget_exhausted: bool,
}

impl Collection {
    pub fn failed() -> Self {
        Self {
            samples: Vec::new(),
            up: false,
            blocked: false,
            errors: 1,
            requests: 0,
            budget_exhausted: false,
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
        if self.requests >= MAX_REQUESTS_PER_SCRAPE {
            if !self.budget_exhausted {
                self.errors = self.errors.saturating_add(1);
                self.budget_exhausted = true;
            }
            return None;
        }
        self.requests += 1;
        match target.get(path, inflight).await {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(
                    target = name,
                    path,
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

    async fn fetch_members(
        &mut self,
        name: &str,
        target: &Target,
        inflight: &Semaphore,
        path: &str,
        limit: usize,
    ) -> Option<Vec<Value>> {
        let collection = self.fetch(name, target, inflight, path).await?;
        let Some(members) = collection["Members"].as_array() else {
            self.errors = self.errors.saturating_add(1);
            return None;
        };
        if members.len() > limit
            || !collection["Members@odata.nextLink"].is_null()
            || collection["Members@odata.count"]
                .as_u64()
                .is_some_and(|count| count != members.len() as u64)
        {
            self.errors = self.errors.saturating_add(1);
            return None;
        }
        let paths = refs(&collection, "Members", limit);
        if paths.len() != members.len() {
            self.errors = self.errors.saturating_add(1);
            return None;
        }
        let mut values = Vec::with_capacity(paths.len());
        for path in paths {
            values.push(self.fetch(name, target, inflight, &path).await?);
        }
        Some(values)
    }

    fn advertised_link(&mut self, value: &Value, key: &str) -> Option<String> {
        let advertised = value.get(key).filter(|value| !value.is_null())?;
        match advertised["@odata.id"].as_str() {
            Some(path) if safe_path(path) => Some(path.to_owned()),
            _ => {
                self.errors = self.errors.saturating_add(1);
                None
            }
        }
    }
}

async fn collect_cpu(
    name: &str,
    target: &Target,
    inflight: &Semaphore,
    out: &mut Collection,
    system: &Value,
) {
    if let Some(path) = out.advertised_link(system, "Processors") {
        if let Some(items) = out.fetch_members(name, target, inflight, &path, 8).await {
            metrics::processors(&mut out.samples, &serde_json::json!({"Members": items}));
        }
    }
    if let Some(path) = out.advertised_link(system, "Memory") {
        if let Some(items) = out.fetch_members(name, target, inflight, &path, 64).await {
            metrics::memory(&mut out.samples, &serde_json::json!({"Members": items}));
        }
    }
    if let Some(path) = out.advertised_link(system, "Storage") {
        if let Some(units) = out.fetch_members(name, target, inflight, &path, 8).await {
            let complete_links = units.iter().all(|unit| {
                unit["Drives"]
                    .as_array()
                    .is_some_and(|drives| refs(unit, "Drives", 17).len() == drives.len())
            });
            let drive_paths: Vec<String> = units
                .iter()
                .flat_map(|unit| refs(unit, "Drives", 17))
                .collect();
            if complete_links && drive_paths.len() <= 16 {
                let mut drives = Vec::with_capacity(drive_paths.len());
                for path in &drive_paths {
                    if let Some(drive) = out.fetch(name, target, inflight, path).await {
                        metrics::drive(&mut out.samples, &drive);
                        drives.push(drive);
                    }
                }
                if drives.len() == drive_paths.len() {
                    metrics::nvme_count(&mut out.samples, &drives);
                }
            } else {
                out.errors = out.errors.saturating_add(1);
            }
        }
    }
    if let Some(path) = out.advertised_link(system, "EthernetInterfaces") {
        if let Some(items) = out.fetch_members(name, target, inflight, &path, 16).await {
            metrics::nics(
                &mut out.samples,
                &serde_json::json!({"Members": items}),
                "onboard",
            );
        }
    }
    if let Some(manager) = out
        .fetch(name, target, inflight, "/redfish/v1/Managers/Self")
        .await
    {
        metrics::manager(&mut out.samples, &manager);
        if let Some(path) = out.advertised_link(&manager, "EthernetInterfaces") {
            if let Some(items) = out.fetch_members(name, target, inflight, &path, 8).await {
                metrics::nics(
                    &mut out.samples,
                    &serde_json::json!({"Members": items}),
                    "bmc",
                );
            }
        }
    }
    if let Some(chassis) = out
        .fetch(name, target, inflight, "/redfish/v1/Chassis/Self")
        .await
    {
        metrics::chassis(&mut out.samples, &chassis);
        if let Some(path) = out.advertised_link(&chassis, "Thermal") {
            if let Some(value) = out.fetch(name, target, inflight, &path).await {
                metrics::thermal(&mut out.samples, &value);
            }
        }
        if let Some(path) = out.advertised_link(&chassis, "Power") {
            if let Some(value) = out.fetch(name, target, inflight, &path).await {
                metrics::power(&mut out.samples, &value);
            }
        }
        if let Some(path) = out.advertised_link(&chassis, "Sensors") {
            if let Some(items) = out.fetch_members(name, target, inflight, &path, 64).await {
                for sensor in items {
                    metrics::sensor(&mut out.samples, &sensor);
                }
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
            requests: 0,
            budget_exhausted: false,
        };
    }
    let mut out = Collection {
        samples: Vec::new(),
        up: false,
        blocked: false,
        errors: 0,
        requests: 1,
        budget_exhausted: false,
    };
    let probe = tokio::time::timeout(
        Duration::from_secs(10),
        target.get(target.profile.system_path(), inflight),
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
    metrics::target_info(
        &mut out.samples,
        target.profile.id(),
        target.profile.model(),
        target.profile.device_type(),
    );
    metrics::system(&mut out.samples, &system);

    if target.profile != Profile::AsrockrackB3008u16x {
        collect_cpu(name, target, inflight, &mut out, &system).await;
        return out;
    }

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
    if let Some(storage) = out
        .fetch(
            name,
            target,
            inflight,
            "/redfish/v1/Systems/System_0/Storage?$expand=*($levels=1)",
        )
        .await
    {
        if let Some(simple) = out
            .fetch(
                name,
                target,
                inflight,
                "/redfish/v1/Systems/System_0/SimpleStorage?$expand=*($levels=1)",
            )
            .await
        {
            if !metrics::b300_nvme_inventory(&mut out.samples, &storage, &simple) {
                out.errors = out.errors.saturating_add(1);
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
        response::IntoResponse,
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
                credential_ref: None,
                username: "a".into(),
                password: "b".into(),
                insecure_tls: true,
                profile: Profile::AsrockrackB3008u16x
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

    #[test]
    fn profiles_are_explicit_and_unknown_models_are_rejected() {
        for (id, expected) in [
            ("asrockrack_b300_8u16x", Profile::AsrockrackB3008u16x),
            ("tyan_b8037", Profile::TyanB8037),
            ("asrockrack_1u10e", Profile::Asrockrack1u10e),
        ] {
            let config: TargetConfig = serde_json::from_value(json!({
                "address":"10.0.0.1", "username":"test", "password":"secret", "profile":id
            }))
            .unwrap();
            assert_eq!(config.profile, expected);
        }
        assert!(serde_json::from_value::<TargetConfig>(json!({
            "address":"10.0.0.1", "username":"test", "password":"secret", "profile":"unknown"
        }))
        .is_err());
    }

    #[test]
    fn unsafe_advertised_link_marks_scrape_incomplete() {
        let mut result = Collection::failed();
        assert!(result
            .advertised_link(
                &json!({"Memory":{"@odata.id":"//evil.example/Memory"}}),
                "Memory"
            )
            .is_none());
        assert_eq!(result.errors, 2);
    }

    #[tokio::test]
    async fn wrong_profile_does_not_claim_redfish_up() {
        let app = Router::new().fallback(get(|| async { StatusCode::NOT_FOUND }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let target = Target::mock_profile(&format!("http://{addr}/"), Profile::TyanB8037);
        let result = collect("CPU-01", &target, &Semaphore::new(4)).await;
        assert!(!result.up);
        assert_eq!(result.errors, 1);
    }

    #[tokio::test]
    async fn paginated_cpu_memory_is_incomplete_not_zero_dimms() {
        let app = Router::new().fallback(get(|uri: Uri| async move {
            Json(match uri.path() {
                "/redfish/v1/Systems/Self" => json!({"PowerState":"On","Memory":{"@odata.id":"/redfish/v1/Systems/Self/Memory"}}),
                "/redfish/v1/Systems/Self/Memory" => json!({"Members":[{"@odata.id":"/redfish/v1/Systems/Self/Memory/DIMM0"}],"Members@odata.nextLink":"/redfish/v1/Systems/Self/Memory?page=2"}),
                _ => json!({}),
            })
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let target = Target::mock_profile(&format!("http://{addr}/"), Profile::TyanB8037);
        let output = metrics::render(
            "TYAN-01",
            collect("TYAN-01", &target, &Semaphore::new(4)).await,
            0.1,
        );
        assert!(output.contains("redfish_scrape_complete{target=\"TYAN-01\"} 0"));
        assert!(!output.contains("redfish_dimm_count"));
    }

    #[tokio::test]
    async fn b300_uses_simple_storage_without_slow_drive_detail_gets() {
        let drive_calls = Arc::new(AtomicUsize::new(0));
        let observed = drive_calls.clone();
        let app = Router::new().fallback(get(move |uri: Uri| {
            let observed = observed.clone();
            async move {
                let value = match uri.path() {
                    "/redfish/v1/Systems/System_0" => json!({"PowerState":"On","Status":{"Health":"OK"}}),
                    "/redfish/v1/Systems/System_0/Storage" => json!({
                        "Members@odata.count":1,
                        "Members":[{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A",
                            "StorageControllers":[{"SerialNumber":"NVME-A","SupportedDeviceProtocols":["NVMe"]}],
                            "Drives":[{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A/Drives/NVMe_Device7_NSID1"}]}]
                    }),
                    "/redfish/v1/Systems/System_0/SimpleStorage" => json!({
                        "Members@odata.count":1,
                        "Members":[{"Links":{"Storage":{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A"}},
                            "Devices":[{"Name":"PCIeDevice_01_8F_00","CapacityBytes":3840755982336_u64,
                                        "Status":{"Health":"OK","State":"Enabled"}}]}]
                    }),
                    path if path.contains("/Drives/") => {
                        observed.fetch_add(1, Ordering::SeqCst);
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    _ => json!({"Members":[]}),
                };
                Json(value).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let target = Target::mock(&format!("http://{addr}/"));
        let result = collect("B300-10", &target, &Semaphore::new(4)).await;
        assert_eq!(result.errors, 0);
        assert_eq!(drive_calls.load(Ordering::SeqCst), 0);
        let output = metrics::render("B300-10", result, 0.1);
        assert!(output.contains("redfish_nvme_drive_count{target=\"B300-10\"} 1"));
        assert!(output.contains("redfish_scrape_complete{target=\"B300-10\"} 1"));
        assert!(!output.contains("redfish_drive_life_left_percent"));
    }

    #[tokio::test]
    async fn missing_b300_simple_storage_does_not_claim_a_drive_count() {
        let app = Router::new().fallback(get(|uri: Uri| async move {
            let value = match uri.path() {
                "/redfish/v1/Systems/System_0" => json!({"PowerState":"On","Status":{"Health":"OK"}}),
                "/redfish/v1/Systems/System_0/Storage" => json!({"Members@odata.count":1,"Members":[{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A","StorageControllers":[{"SerialNumber":"NVME-A","SupportedDeviceProtocols":["NVMe"]}]}]}),
                "/redfish/v1/Systems/System_0/SimpleStorage" => return StatusCode::NOT_FOUND.into_response(),
                _ => json!({"Members":[]}),
            };
            Json(value).into_response()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let target = Target::mock(&format!("http://{addr}/"));
        let output = metrics::render(
            "B300-10",
            collect("B300-10", &target, &Semaphore::new(4)).await,
            0.1,
        );
        assert!(output.contains("redfish_scrape_complete{target=\"B300-10\"} 0"));
        assert!(!output.contains("redfish_nvme_drive_count"));
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
                    "/redfish/v1/Systems/System_0/Storage" => json!({"Members@odata.count":1,"Members":[{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A","StorageControllers":[{"SerialNumber":"NVME-A","SupportedDeviceProtocols":["NVMe"]}]}]}),
                    "/redfish/v1/Systems/System_0/SimpleStorage" => json!({"Members@odata.count":1,"Members":[{"Links":{"Storage":{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A"}},"Devices":[{"Name":"NVMe","CapacityBytes":100,"Status":{"Health":"OK","State":"Enabled"}}]}]}),
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

    #[tokio::test]
    async fn cpu_profiles_follow_self_links_without_expand_or_hgx_requests() {
        for (profile, expected_model, has_sensors) in [
            (Profile::TyanB8037, "B8037T73X4-200PE4HR-L", false),
            (Profile::Asrockrack1u10e, "1U10E-ICX2", true),
        ] {
            let paths = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let seen = paths.clone();
            let app = Router::new().fallback(get(move |uri: Uri| {
                let seen = seen.clone();
                async move {
                    seen.lock().expect("paths").push(uri.to_string());
                    let value = match uri.path() {
                        "/redfish/v1/Systems/Self" => json!({"Model":expected_model,"PowerState":"On","Status":{"Health":"OK"},"Processors":{"@odata.id":"/redfish/v1/Systems/Self/Processors"},"Memory":{"@odata.id":"/redfish/v1/Systems/Self/Memory"},"Storage":{"@odata.id":"/redfish/v1/Systems/Self/Storage"}}),
                        "/redfish/v1/Systems/Self/Processors" => json!({"Members":[{"@odata.id":"/redfish/v1/Systems/Self/Processors/CPU0"}]}),
                        "/redfish/v1/Systems/Self/Processors/CPU0" => json!({"ProcessorType":"CPU","Socket":"CPU0","Status":{"Health":"OK"}}),
                        "/redfish/v1/Systems/Self/Memory" => json!({"Members":[{"@odata.id":"/redfish/v1/Systems/Self/Memory/DIMM0"}]}),
                        "/redfish/v1/Systems/Self/Memory/DIMM0" => json!({"Name":"DIMM0","CapacityMiB":32768,"Status":{"Health":"OK"}}),
                        "/redfish/v1/Systems/Self/Storage" => json!({"Members":[]}),
                        "/redfish/v1/Managers/Self" => json!({"FirmwareVersion":"2.05.0","Model":"AMI"}),
                        "/redfish/v1/Chassis/Self" => if has_sensors {json!({"Status":{"Health":"OK"},"Sensors":{"@odata.id":"/redfish/v1/Chassis/Self/Sensors"},"Power":{"@odata.id":"/redfish/v1/Chassis/Self/Power"},"Thermal":{"@odata.id":"/redfish/v1/Chassis/Self/Thermal"}})} else {json!({"Status":{"Health":"OK"}})},
                        "/redfish/v1/Chassis/Self/Power" => json!({"PowerControl":[{"PowerConsumedWatts":420}],"PowerSupplies":[{"Name":"PSU1","Status":{"Health":"OK"},"PowerOutputWatts":225}]}),
                        "/redfish/v1/Chassis/Self/Thermal" => json!({"Fans":[{"Name":"FAN1","Reading":4400,"ReadingUnits":"RPM","Status":{"Health":"OK"}}],"Temperatures":[{"Name":"Inlet","ReadingCelsius":28,"PhysicalContext":"Intake"}]}),
                        "/redfish/v1/Chassis/Self/Sensors" => json!({"Members":[{"@odata.id":"/redfish/v1/Chassis/Self/Sensors/1"}]}),
                        "/redfish/v1/Chassis/Self/Sensors/1" => json!({"Name":"CPU Temp","ReadingType":"Temperature","ReadingUnits":"Cel","Reading":46,"Status":{"Health":"OK"}}),
                        _ => json!({"Members":[]}),
                    };
                    Json(value)
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let target = Target::mock_profile(&format!("http://{addr}/"), profile);
            let result = collect("CPU-01", &target, &Semaphore::new(4)).await;
            let output = metrics::render("CPU-01", result, 0.1);
            assert!(output.contains("redfish_cpu_count{target=\"CPU-01\"} 1"));
            assert!(output.contains("redfish_memory_total_bytes{target=\"CPU-01\"} 34359738368"));
            assert!(output.contains(&format!("model=\"{expected_model}\"")));
            assert_eq!(
                output.contains("redfish_sensor_temperature_celsius"),
                has_sensors
            );
            assert_eq!(output.contains("redfish_chassis_power_watts"), has_sensors);
            assert_eq!(output.contains("redfish_fan_rpm"), has_sensors);
            let paths = paths.lock().expect("paths");
            assert!(paths
                .iter()
                .all(|p| !p.contains("$expand") && !p.contains("HGX")));
        }
    }
}
