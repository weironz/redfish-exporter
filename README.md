# redfish-exporter

Independent, read-only Rust Prometheus exporter for NVIDIA B300/HGX BMC Redfish telemetry. It runs on an IDC management host, outside the rented GPU server OS. It does not perform Redfish writes, power operations, firmware updates, or console probing.

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
docker build -t redfish-exporter:v0.1.0 .
```

The container image is built locally; this repository does not automatically publish or deploy it. NeoStor's optional Compose `redfish` profile can reference a version-pinned image from an approved registry or a locally loaded image.

## Configuration

Create a private `redfish-targets.json` on the IDC management host from [the example](examples/redfish-targets.example.json). The `target` URL parameter is an alias in this file, never an arbitrary URL or IP. Use a `0700` parent directory and a `0600` file. Do not commit, log, or send BMC credentials to NeoStor's monitoring configuration. The exporter accepts only preconfigured private IPv4 BMC addresses over HTTPS; set `insecure_tls: true` only for BMCs with self-signed certificates.

Environment variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `REDFISH_EXPORTER_TARGETS_FILE` | `/run/secrets/redfish-targets.json` | IDC-private target and credential file |
| `REDFISH_EXPORTER_BIND` | `127.0.0.1:19626` | Loopback-only HTTP listener |

Endpoints: `/healthz` and `/metrics?target=B300-01`. Unknown targets return 404; overlapping scrapes of the same target return 503. The [vmagent example](examples/vmagent-scrape.example.yaml) uses a 180-second interval and 150-second timeout. It is a canary template, not an automatically loaded NeoStor configuration. Production enrollment requires NeoStor's Redfish monitoring template and dashboard/alert query adaptation.

## Safety and metric semantics

- Reads use fixed Redfish paths and GET only, one request at a time per BMC. A 403 triggers 180–1800 seconds of quiet backoff; timeouts also back off. Never run this exporter and the legacy Python exporter against the same BMC concurrently. First exclude one canary BMC from the old collector, then validate and expand gradually.
- `redfish_gpu_count` is the number of enumerated slots. Use `redfish_gpu_online_count` and per-GPU `redfish_gpu_online` for card presence. Missing component data is omitted rather than reported as zero or replayed as a fresh sample. `redfish_scrape_complete` and `redfish_scrape_errors` describe partial failures.
- Hardware metrics include system power/health, CPU, DIMM, NVMe, NIC, fan, physical temperature, PCIe, HGX board/power, GPU temperature/power and ECC/row-remap counters. The exporter filters AMI `TEMP_CPU`, `FSC`, `Tlimit`, and `Tcontrol` pseudo-temperature fields.
- Redfish does not reliably expose GPU utilization, processes, Xid, NVLink/NVSwitch bandwidth, or IB fabric congestion. These must remain unavailable unless an authorized additional source supplies them; absence does not mean healthy or zero.

Offline tests use a mock BMC only. Live canary collection and IDC/production deployment require separate authorization and coordination with the existing collector.
