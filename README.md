# redfish-exporter

Independent, read-only Rust Prometheus exporter for three server families: ASRockRack 8U16X-GNR2 B300, TYAN B8037T73X4-200PE4HR-L, and ASRockRack 1U10E-ICX2. It runs on an IDC management host, outside the rented server OS. It does not perform Redfish writes, power operations, firmware updates, or console probing.

## Why separate from NeoStor Agent

NeoStor Agent already performs lightweight Redfish health checks and handles the control channel. Deep inventory and telemetry reads can take tens of seconds and AMI BMCs may return HTTP 403 under competing sessions. A separate process isolates those delays and failures. The data path remains:

```text
BMC --HTTPS/Redfish--> redfish-exporter <--HTTP/loopback-- vmagent
vmagent --remote write/loopback--> NeoStor Agent --existing uplink--> center
```

No new IDC inbound firewall port is required. `node_exporter` and DCGM are optional enhancements where the server renter grants OS access; they are not prerequisites for this exporter.

## Build and test

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
docker build -t redfish-exporter:v0.2.2 .
```

The container image is built locally; this repository does not automatically publish or deploy it. NeoStor's optional Compose `redfish` profile can reference a version-pinned image from an approved registry or a locally loaded image.

## Configuration

Create a private `redfish-targets.json` on the IDC management host from [the example](examples/redfish-targets.example.json). The `target` URL parameter is an alias in this file, never an arbitrary URL or IP. Use a `0700` parent directory and a `0600` file. Do not commit, log, or send BMC credentials to NeoStor's monitoring configuration. The exporter accepts only preconfigured private IPv4 BMC addresses over HTTPS; set `insecure_tls: true` only for BMCs with self-signed certificates.

Select a versioned profile per target. `profile` is an exact allowlisted value, not a user-supplied Redfish path. Existing B300 target files without `profile` continue to use `asrockrack_b300_8u16x`; set it explicitly for new targets. The model label in `redfish_target_info` describes this configured profile, not an independently verified asset identity. Inventory remains the source of truth for the exact machine model; verify the mapping before enrollment.

| Profile | Historically surveyed resource family | Collection strategy |
| --- | --- | --- |
| `asrockrack_b300_8u16x` | `/Systems/System_0`, `/Managers/BMC_0`, HGX | Expanded `Storage` + `SimpleStorage` inventories, plus B300-specific telemetry reports |
| `tyan_b8037` | `/Systems/Self`, `/Managers/Self`, `/Chassis/Self` | Follow advertised collection links, fetch members individually; no `$expand` |
| `asrockrack_1u10e` | `/Systems/Self`, `/Managers/Self`, `/Chassis/Self` | Follow advertised collection links plus Thermal, Power and Sensors when present; no `$expand` |

Both CPU profiles collect system/chassis health, power state, CPU/DIMM inventory, storage and NIC details when the BMC advertises them. Thermal, PSU and sensor metrics appear only for advertised, successfully read resources. Missing links do not become zeros. No CPU model is queried for HGX/GPU endpoints. Profile differences and paths are grounded in the existing read-only capability surveys; real output values still require one authorized canary per model and firmware version.

Environment variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `REDFISH_EXPORTER_TARGETS_FILE` | `/run/secrets/redfish-targets.json` | IDC-private target and credential file |
| `REDFISH_EXPORTER_BIND` | `127.0.0.1:19626` | Loopback-only HTTP listener |

Endpoints: `/healthz` and `/metrics?target=B300-01`. Unknown targets return 404; overlapping scrapes of the same target return 503. The [vmagent example](examples/vmagent-scrape.example.yaml) uses a 180-second interval and 150-second timeout. It is a canary template, not an automatically loaded NeoStor configuration. Production enrollment requires NeoStor's Redfish monitoring template and dashboard/alert query adaptation.

## Safety and metric semantics

- Reads use profile-selected paths and validated same-origin Redfish links, GET only, one request at a time per BMC, at most 96 requests per scrape. CPU member collections are bounded and deliberately do not use `$expand`; an oversized/paginated collection is reported incomplete rather than counted as zero. A 403 triggers 180–1800 seconds of quiet backoff; timeouts also back off. Never run this exporter and the legacy Python exporter against the same BMC concurrently. First exclude one canary BMC from the old collector, then validate and expand gradually.
- `redfish_gpu_count` is the number of enumerated slots. Use `redfish_gpu_online_count` and per-GPU `redfish_gpu_online` for card presence. For B300, `redfish_nvme_drive_count` and per-drive health, enabled state, and capacity are emitted only when the complete expanded `Storage` and `SimpleStorage` inventories match by storage-unit link. CPU profiles retain their Drive-detail collection. A partial inventory must not claim a complete count. Missing component data is omitted rather than reported as zero or replayed as a fresh sample. `redfish_scrape_complete` and `redfish_scrape_errors` describe partial failures.
- B300's normal scrape does not call the unusually slow Drive detail URLs. Therefore `redfish_drive_life_left_percent`, which is available only from those URLs, is intentionally unavailable for B300 in v0.2.2. A future optional, separately budgeted wear-life collection must not block or alter the baseline scrape. Do not interpret an absent wear-life series as 100% remaining life.
- `redfish_target_info{profile,model,device_type}` carries bounded configured model metadata for dashboard grouping. New CPU metrics include `redfish_chassis_health`, `redfish_chassis_power_watts`, `redfish_psu_*`, and `redfish_sensor_*`; numeric sensor readings are exported only when their units are known. `redfish_up=0` means the profile's system probe did not succeed, including HTTP 404 from a wrong profile.
- Hardware metrics include system power/health, CPU, DIMM, NVMe, NIC, fan, physical temperature, PCIe, HGX board/power, GPU temperature/power and ECC/row-remap counters. The exporter filters AMI `TEMP_CPU`, `FSC`, `Tlimit`, and `Tcontrol` pseudo-temperature fields.
- Redfish does not reliably expose GPU utilization, processes, Xid, NVLink/NVSwitch bandwidth, or IB fabric congestion. These must remain unavailable unless an authorized additional source supplies them; absence does not mean healthy or zero.

Offline tests use a mock BMC only. Live canary collection and IDC/production deployment require separate authorization and coordination with the existing collector.

The first authorized one-host-per-model canary results are recorded in [the validation report](docs/live-validation-2026-09-28.md). This repository has not deployed the new exporter to IDC or NeoStor production.
