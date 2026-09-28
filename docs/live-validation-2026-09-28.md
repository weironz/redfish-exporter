# Three-model Redfish canary — 2026-09-28

Scope: one BMC per hardware family, from a local workstation running the standalone Rust exporter v0.2.0 binary. Credentials came from the existing private inventory workbook and existed only in per-run temporary files; no credential or raw BMC response was logged or committed. All Redfish requests were GET. No IDC or NeoStor production deployment was made.

| Profile | Sample | Duration | Samples | `redfish_up` | `redfish_scrape_complete` | Errors | Main observations |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| `tyan_b8037` | TYAN B8037, `.33` | 46.47 s | 73 | 1 | 1 | 0 | 1 CPU, 4 DIMMs, 1 NVMe, 8 fans; chassis power, PSU and temperature series present |
| `asrockrack_1u10e` | ASRockRack 1U10E, `.37` | 101.75 s | 105 | 1 | 1 | 0 | 2 CPUs, 4 DIMMs, 8 NVMe, 18 fans and 19 temperature series |
| `asrockrack_b300_8u16x` | ASRockRack B300, `.10` | 105.72 s | 410 | 1 | 0 | 3 | 8 GPU slots and 8 online, GPU temperature/power/ECC present; drive inventory partial |

The ASRockRack 1U BMC's `System.Model` response was blank despite the inventory identifying the model. Its `Power` resource did not return `PowerConsumedWatts` or `PowerSupplies`, and sampled `Sensors` lacked usable `ReadingType`/`ReadingUnits`; the exporter correctly omitted numeric power/sensor series. No zero or inferred reading was substituted.

The B300 exporter v0.2.0 reported 10 NVMe *links* but only 7 readable drive detail series; `redfish_scrape_errors=3`. A subsequent, isolated GET diagnostic of the advertised drive resources hit a drive-detail read timeout and was stopped rather than repeatedly retrying the BMC. The exact cause/status of all three original errors is not established. In v0.2.1, `redfish_nvme_drive_count` is omitted unless all advertised drive details are read and their protocols are known. This correction has an offline regression test but has **not** been retested on the live B300; the B300 profile remains partially validated, not fully passed.

The existing Python B300 exporter was actively monitored by Prometheus for all 32 B300s. It was stopped only during the two B300 canaries and restarted in a `finally` block each time. After each restart, Prometheus returned to 32/32 B300 targets `up`; no legacy container or configuration was replaced. The two CPU canaries did not require changing the old collector.

Follow-up before rollout: determine why the B300 Drive detail endpoints time out, and repeat CPU canaries if firmware versions differ from these samples. Monitor scrape duration against the suggested 150-second timeout, especially ASRockRack 1U.

## v0.2.1 B300 retest and latest legacy collector state

At the user's request, the legacy `b300-redfish-exporter` container was manually stopped and must **remain stopped until the user explicitly asks to restart it**. Docker reports `status=exited`, with `restart=unless-stopped`; the new Rust exporter is not deployed to IDC. This intentionally suspends the legacy B300 Redfish monitoring for the 32 targets; Prometheus `down` results caused by the stopped exporter must not be interpreted as 32 BMC hardware failures.

One local v0.2.1 canary against B300-10 took 104.44 seconds and emitted 409 samples: `redfish_up=1`, `redfish_blocked=0`, `redfish_scrape_complete=0`, `redfish_scrape_errors=3`, `redfish_gpu_count=8`, and `redfish_gpu_online_count=8`. GPU temperature, power and ECC series were present for all eight GPUs. As intended, `redfish_nvme_drive_count` was absent because the drive scan was incomplete.

A separate sequential GET check of the 10 advertised NVMe Drive links found that devices 0–6 responded, while `NVMe_Device7_NSID1`, `NVMe_Device8_NSID1` and `NVMe_Device9_NSID1` timed out. Their parent `StorageUnit_A/B/C` objects in the expanded Storage collection each reported `Status.State=Enabled` and `Status.Health=OK`, with one Drive link. Parent status does **not** prove that the individual drives are healthy or present; the supported conclusion is that these three Drive detail endpoints did not respond within 15 seconds. No power, reset, console or configuration action was attempted.

The originally listed follow-up to verify v0.2.1 against one B300 is now done; the remaining blocker to a fully complete B300 scrape is the three BMC Drive detail timeouts. Do not suppress those errors or report a fabricated NVMe count merely to make the scrape appear complete.
