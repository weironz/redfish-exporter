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

Follow-up before rollout: diagnose the B300 drive-detail timeouts during a coordinated maintenance/canary window, verify v0.2.1 against one B300 without overlapping the old collector, and repeat CPU canaries if firmware versions differ from these samples. Monitor scrape duration against the suggested 150-second timeout, especially ASRockRack 1U.
