# redfish-exporter

Independent, read-only Rust Prometheus exporter for four server families: ASRockRack 8U16X-GNR2 B300, TYAN B8037T73X4-200PE4HR-L, ASRockRack 1U10E-ICX2, and ASRockRack 1U12E-EGS2-EVAC. It runs on an IDC management host, outside the rented server OS. It does not perform Redfish writes, power operations, firmware updates, or console probing.

## Why separate from NeoStor Agent

NeoStor Agent already performs lightweight Redfish health checks and handles the control channel. Deep inventory and telemetry reads can take tens of seconds and AMI BMCs may return HTTP 403 under competing sessions. A separate process isolates those delays and failures. The data path remains:

```text
BMC --HTTPS/Redfish--> redfish-exporter <--HTTP/loopback-- vmagent
vmagent --remote write/loopback--> NeoStor Agent --existing uplink--> center
```

No new IDC inbound firewall port is required. This exporter never requires tenant OS access. NeoStor currently disables node/DCGM in-band monitoring support; this project does not enable it.

## Build and test

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
docker build -t redfish-exporter:v0.2.6 .
```

The container image is built locally; this repository does not automatically publish or deploy it. NeoStor's optional Compose `redfish` profile can reference a version-pinned image from an approved registry or a locally loaded image.

## Configuration

Create a private `redfish-targets.json` on the IDC management host from [the example](examples/redfish-targets.example.json). The `target` URL parameter is an alias in this file, never an arbitrary URL or IP. Use a `0700` parent directory and a `0600` file. Do not commit, log, or send BMC credentials to NeoStor's monitoring configuration. The exporter accepts only preconfigured private IPv4 BMC addresses over HTTPS; set `insecure_tls: true` only for BMCs with self-signed certificates.

NeoStor identity-v2 integrations also require a non-secret `credential_ref` in each target, matching the IDC-private Agent binding. `POST /binding-check` compares `target`, `address`, `profile`, and `credential_ref` against the configuration loaded by this process and returns only JSON `true`/`false`. It never calls a BMC, returns a secret, or proves that a password works. Legacy target files without a reference still scrape normally, but cannot pass this cross-check; an old exporter without the endpoint must be upgraded before publishing a new canonical Redfish configuration. Restart after editing the private target file; checking disk content alone does not prove the running exporter loaded it.

Select a versioned profile per target. `profile` is an exact allowlisted value, not a user-supplied Redfish path. Existing B300 target files without `profile` continue to use `asrockrack_b300_8u16x`; set it explicitly for new targets. The model label in `redfish_target_info` describes this configured profile, not an independently verified asset identity. Inventory remains the source of truth for the exact machine model; verify the mapping before enrollment.

| Profile | Historically surveyed resource family | Collection strategy |
| --- | --- | --- |
| `asrockrack_b300_8u16x` | `/Systems/System_0`, `/Managers/BMC_0`, HGX | Expanded `Storage` + `SimpleStorage` inventories, plus B300-specific telemetry reports |
| `tyan_b8037` | `/Systems/Self`, `/Managers/Self`, `/Chassis/Self` | Follow advertised collection links, fetch members individually; no `$expand` |
| `asrockrack_1u10e` | `/Systems/Self`, `/Managers/Self`, `/Chassis/Self` | Follow advertised collection links plus Thermal, Power and Sensors when present; no `$expand` |
| `asrockrack_1u12e` | Live system discovery: `/Systems/Self` (Redfish 1.15.1) | Distinct CPU profile; bounded Sensors pagination and rolling, source-timestamped CPU/memory/NIC details; v0.2.5 live acceptance pending |

CPU profiles collect system/chassis health, power state, CPU/DIMM inventory, storage and NIC details when the BMC advertises them. Thermal, PSU and sensor metrics appear only for advertised, successfully read resources. Missing links do not become zeros. No CPU model is queried for HGX/GPU endpoints. Profile differences and paths are grounded in read-only capability surveys; real output values still require one authorized canary per model and firmware version. ReadyIDC's 1U12E system reports board model `SP2C741D32TM3`; this is retained as observed BMC inventory, not used to overwrite the operator-declared chassis model `1U12E-EGS2-EVAC`.

Environment variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `REDFISH_EXPORTER_TARGETS_FILE` | `/run/secrets/redfish-targets.json` | IDC-private target and credential file |
| `REDFISH_EXPORTER_BIND` | `127.0.0.1:19626` | Loopback-only HTTP listener |

Endpoints: `/healthz` and `/metrics?target=B300-01`. Unknown targets return 404; overlapping scrapes of the same target return 503. The [vmagent example](examples/vmagent-scrape.example.yaml) uses a 180-second interval and 150-second timeout. It is a canary template, not an automatically loaded NeoStor configuration. Production enrollment requires NeoStor's Redfish monitoring template and dashboard/alert query adaptation.

The new development endpoint `/capabilities?target=B300-01` returns the versioned `redfish.capabilities.v1` metadata contract without reading the BMC. It exposes only the configured profile/model, read-only capability IDs, `conditional`/`unsupported` states, and value semantics; it never returns target addresses, credential references, passwords, or measured values. `conditional` means implemented but dependent on advertised BMC resources, not healthy or successfully scraped. Timeout/error is separate from unsupported, and missing values are never synthesized as zero. Unknown aliases return 404. NeoStor Agent consumes this loopback-only endpoint after applying an identity-bound configuration and reports safe typed metadata under `monitoring.capabilities.v1`; the center validates the exact Agent, asset, profile and latest acknowledged revision. Existing published v0.2.2 images do **not** contain this endpoint until a separately approved image release.

## Safety and metric semantics

- Reads use profile-selected paths and validated same-origin Redfish links, GET only, one request at a time per BMC, at most 96 requests per scrape. CPU member collections are bounded and deliberately do not use `$expand`; unsupported oversized/paginated inventories are reported incomplete rather than counted as zero. The 1U12E Sensors adapter is the documented exception below. A 403 triggers 180–1800 seconds of quiet backoff; timeouts also back off. Never run this exporter and the legacy Python exporter against the same BMC concurrently. First exclude one canary BMC from the old collector, then validate and expand gradually.
- `redfish_gpu_count` is the number of enumerated slots. Use `redfish_gpu_online_count` and per-GPU `redfish_gpu_online` for card presence. For B300, `redfish_nvme_drive_count` and per-drive health, enabled state, and capacity are emitted only when the complete expanded `Storage` and `SimpleStorage` inventories match by storage-unit link. CPU profiles retain their Drive-detail collection. A partial inventory must not claim a complete count. Missing component data is omitted rather than reported as zero. Cached 1U12E sensor values retain explicit original Prometheus sample timestamps; consumers must honor timestamps. `redfish_scrape_complete` and `redfish_scrape_errors` describe partial failures.
- B300's normal scrape does not call the unusually slow Drive detail URLs. An optional `slow_drive_wear: true` in the IDC-private target file now enables a separate wear-life snapshot, triggered after a normal scrape at most once per 1800 seconds. It has a 90-second total budget, 30-second individual GET deadlines, and at most 16 drives. Default remains disabled; other profiles reject this option. `/slow-metrics?target=ALIAS` only reads the cache and never initiates a BMC request. Source timestamps are retained in the Prometheus text output (`honor_timestamps: true`); replaying the cache must not be treated as a new observation. Missing wear-life data is unknown, not 100% remaining life.
- Fast and slow reads of one BMC remain serial. Slow requests have an independent global concurrency limit of one, separate from the four baseline slots; waiting for that budget never holds a BMC lock. A separate fair per-request lock gives a waiting baseline scrape priority before the next slow request; an already-running slow GET can delay it by at most its 30-second deadline, within the baseline's unchanged 140-second total deadline. The control Agent never awaits slow work. A timeout discards that batch instead of claiming a complete result, and failed slow work is not immediately retried. This does not guarantee BMC firmware stops processing a timed-out request.
- `redfish_group_success{group=...}`, `redfish_group_last_attempt_timestamp_seconds` and `redfish_group_last_success_timestamp_seconds` distinguish health, compute, drives, network, power, GPU and optional drive-wear collection evidence. Success means samples were emitted without a known group request/inventory error, not that all hardware is healthy or every possible field is supported. Absent groups report success 0 and no successful timestamp; a drive failure does not mark a successfully collected health group failed. Consumers must check age and success together. Optional slow failures do not replace fast inventory or change its counts.
- `redfish_target_info{profile,model,device_type}` carries bounded configured model metadata for dashboard grouping. New CPU metrics include `redfish_chassis_health`, `redfish_chassis_power_watts`, `redfish_psu_*`, and `redfish_sensor_*`; numeric sensor readings are exported only when their units are known. `redfish_up=0` means the profile's system probe did not succeed, including HTTP 404 from a wrong profile.
- Hardware metrics include system power/health, CPU, DIMM, NVMe, NIC, fan, physical temperature, PCIe, HGX board/power, GPU temperature/power and ECC/row-remap counters. The exporter filters AMI `TEMP_CPU`, `FSC`, `Tlimit`, and `Tcontrol` pseudo-temperature fields.
- Redfish does not reliably expose GPU utilization, processes, Xid, NVLink/NVSwitch bandwidth, or IB fabric congestion. These must remain unavailable unless an authorized additional source supplies them; absence does not mean healthy or zero.

## Bounded 1U12E sensor snapshots and inventory scheduling (v0.2.5)

The ReadyIDC v0.2.3 canary returned 136 Sensors with 50 links on the first page; the former 64-member, non-paginated reader truthfully reported partial. v0.2.4 safely discovers the paginated directory but its live sensor details were starved by baseline resource reads (27,54,78 cached in the first three rounds), so the production center rollout was stopped. The adapter discovers at most 8 pages / 256 unique members, validates total counts and rejects duplicate references, non-numeric pagination, resource/origin changes and `$expand`. v0.2.5 schedules the bounded sensor batch before CPU/member enumeration, then uses only the remaining original 96-request budget for rolling CPU/memory/NIC details. A full offline fixture with 8 processor, 64 memory, 24 NIC and 136 sensor entries converges in three rounds and remains complete across eighteen simulated 600s rounds. This is not a guarantee for every resource size, latency or firmware response; the live source/coverage gate still must pass.

The rolling inventory applies **only to 1U12E**. Every advertised collection is rediscovered each round; directories are limited to their original caps (8 processors / 64 memory / 16 onboard NICs / 8 BMC NICs) and reject truncation, pagination, invalid totals, duplicate or foreign links. Missing detail resources get fair priority across collections, then the oldest observations are refreshed. Caches contain bounded, whitelisted scalar summaries only. Directory changes/failures invalidate the affected inventory; failed details remove previous samples. Aggregated counts/capacity are emitted only for a fully covered, unexpired directory. Per-member metrics retain their own original timestamp and `resource_id`, and aggregate timestamps use the oldest contributing observation. The unchanged 1800s expiry is enforced even if this makes a scrape partial. No background worker or increased per-round request limit is introduced; other profiles keep their existing collection path.

`redfish_sensor_inventory_count` is the observed BMC directory size, **not** an operator-declared expected hardware count. `redfish_sensor_cached_count` and `redfish_sensor_snapshot_complete` show coverage. Warm-up, an invalid directory, failed detail read or expired coverage keeps the scrape partial. Removed/reordered directories invalidate old snapshots as needed; observations older than 1800s are omitted. `sensor_id` distinguishes equal display names. Every cached sensor reading and `redfish_sensor_last_success_timestamp_seconds` keep their original sample time; replay never makes them newly observed. The power-group successful timestamp uses its oldest sensor observation. Smaller inventories refresh each round; larger/slower inventories that cannot stay within the 1800s window remain incomplete, not falsely healthy.

This does not enable OS access, create a background sensor collector, relax control authorization or fix missing NVMe inventory. Keep `honor_timestamps: true` (the Prometheus/vmagent default) and use coverage plus source age for dashboards/alerts. Detailed safety and acceptance evidence is in [the bounded sensor report](docs/1u12e-bounded-sensors-2026-10-12.md).

Offline tests use a mock BMC only. Live canary collection and IDC/production deployment require separate authorization and coordination with the existing collector. The separately authorized ReadyIDC v0.2.5 deployment on 2026-10-12 exposed three empty sensor details (207, 232, 233) in the 136-member directory. Its acceptance gate failed: center deployment was not permitted. Logs expose bounded collection coverage/read budgets and response-shape diagnostics for missing sensor names, never raw bodies or credentials. A missing/invalid sensor name still remains partial; no invented reading, health or count bypass is used to pass the gate.

## Invalid sensor detail diagnostics (v0.2.6 source candidate, not deployed)

The 1U12E adapter keeps the directory size unchanged. `redfish_sensor_cached_count` still counts usable named sensor snapshots, and `redfish_sensor_snapshot_complete` still requires every directory member to be usable. The new `redfish_sensor_detail_count` counts fresh details read successfully, including invalid responses; `redfish_sensor_invalid_count` counts their invalid subset. `redfish_sensor_detail_complete=1` means all details have been read, **not** that readings or hardware are healthy. Invalid responses continue to mark the power group and scrape partial, including cache-replay rounds.

Each invalid response emits `redfish_sensor_invalid_info{sensor_id,reason}`, `redfish_sensor_detail_last_read_timestamp_seconds`, and `redfish_sensor_invalid_field_present{field}`. Reasons are fixed codes (`name_missing`, `name_null`, `name_empty`, `name_non_string`), not raw response text. Field-presence flags describe object/Id/Reading/Health availability, not hardware values. All diagnostic samples retain the actual detail-read timestamp. The same 1800s expiry, directory invalidation, failed-read eviction, 48-detail and 96-total request budgets apply. No new BMC requests or background worker are added.

The mock regression with three empty details yields valid coverage 48/93/133/133 and read coverage 48/96/136/136 across four simulated rounds; usable coverage remains incomplete. These are offline fixture results, not additional live readings. Firmware placeholders, uninstalled sensors or mapping faults remain unconfirmed. An operator-approved diagnostic-aware gate is required before another deployment; these changes do not turn the failed v0.2.5 gate into a pass. NeoStor's next source candidate exposes the diagnostics and keeps asset coverage separate from collector coverage.

The first authorized one-host-per-model canary results are recorded in [the validation report](docs/live-validation-2026-09-28.md). On 2026-10-12 ReadyIDC was authorized to upgrade only the original three canaries to exporter v0.2.4 / NeoStor Agent v0.0.23. CPU-Server-02 remains partial, so center remains v0.0.21. Datazone v0.2.3 was pulled for offline version smoke only. Recheck live versions before another deployment. These observations do not establish full-fleet or rendered Web acceptance.
