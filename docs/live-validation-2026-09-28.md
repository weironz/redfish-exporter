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

## Second B300 comparison: B300-15

With the legacy Python exporter still stopped, a separate local v0.2.1 read-only canary against B300-15 took 98.92 seconds and emitted 409 samples. It reported `redfish_up=1`, `redfish_blocked=0`, `redfish_scrape_complete=0`, `redfish_scrape_errors=3`, `redfish_gpu_count=8`, and `redfish_gpu_online_count=8`. All eight GPUs had temperature, power, and ECC series. Seven Drive detail resources supplied capacity, health, and life-left series; `redfish_nvme_drive_count` was correctly absent because the inventory was incomplete.

The same three detail GETs as on B300-10 timed out: `StorageUnit_A/Drives/NVMe_Device7_NSID1`, `StorageUnit_B/Drives/NVMe_Device8_NSID1`, and `StorageUnit_C/Drives/NVMe_Device9_NSID1` under `/redfish/v1/Systems/System_0/Storage/`. An additional read-only GET of `/redfish/v1/Managers/BMC_0` showed both B300-10 and B300-15 reporting BMC model `825649622` and firmware `1.40.00`.

This reproduces the exact endpoint-specific failure on two machines running the same BMC firmware. At this stage, it did not establish whether the disks were physically present. The exporter was surfacing 15-second BMC response timeouts, not generating them. The later inventory discovery below resolves the disk-presence question and supersedes that provisional uncertainty. No BMC write, reset, console, or IDC deployment was attempted, and the old exporter remains stopped pending explicit user instruction.

## Corrected B300 NVMe inventory diagnosis

The operator supplied a B300-10 BMC inventory screenshot showing ten enabled Samsung NVMe devices. A subsequent read-only Redfish discovery confirmed that the ten devices are available through **other endpoints** on both B300-10 and B300-15:

| Endpoint | Result on each host | Useful fields |
| --- | --- | --- |
| `/redfish/v1/Systems/System_0/SimpleStorage?$expand=*($levels=1)` | 13 storage members, including ten NVMe `Devices`; complete, no pagination or timeout | Device PCIe name, model, `CapacityBytes`, `Status.Health` and `Status.State` |
| `/redfish/v1/Systems/System_0/Storage?$expand=*($levels=1)` | 13 storage members, including ten NVMe `StorageControllers`; all ten have a serial and `Health=OK` | Controller model, serial, firmware, status, protocol and a link to each unit's Drive resource |
| `/redfish/v1/Chassis/BMC_0/PCIeDevices` (all three pages, expanded) | 139 PCIe devices, including the same ten Samsung NVMe PCIe device IDs shown in the screenshot | PCIe identity, link width/generation and status; first page alone is incomplete |

The `SimpleStorage` unit IDs `3`–`C` align with `StorageUnit_3`–`StorageUnit_C`. For example, unit `A` reports `PCIeDevice_01_8F_00`, a Samsung NVMe device with capacity and healthy/enabled status; its `StorageUnit_A` controller has model, serial, firmware and healthy/enabled status. The separately advertised `StorageUnit_A/Drives/NVMe_Device7_NSID1` detail URL still times out. The same pattern holds for units `B` and `C` and on B300-15.

**Correction:** the previous suggestion that the three drives might be absent is inconsistent with the BMC inventory and alternate Redfish resources. The exporter chose a fragile `Storage/.../Drives/...` detail path for its authoritative NVMe count, health and capacity, which explains the missing metrics and roughly 45 seconds of timeout overhead. The underlying reason those three Drive detail URLs hang is still unknown; it is not evidence that the physical disks are faulty. A code change should take count/health/capacity from the complete `SimpleStorage` inventory, correlate controller serial/firmware from the expanded `Storage` collection by unit ID, and treat Drive-only wear-life data as unavailable where the detail endpoint does not respond. Do not fabricate wear-life values or equate a controller count with a verified Drive detail count without checking the independent inventory. No exporter code was changed in this diagnostic pass.
