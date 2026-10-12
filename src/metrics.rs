use crate::bmc::Collection;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct Metric {
    name: &'static str,
    labels: Vec<(&'static str, String)>,
    value: f64,
    source_ms: Option<u64>,
}

fn add(out: &mut Vec<Metric>, name: &'static str, labels: Vec<(&'static str, String)>, value: f64) {
    if value.is_finite() {
        out.push(Metric {
            name,
            labels,
            value,
            source_ms: None,
        });
    }
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn string(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(128)
        .collect()
}

fn health(value: &Value) -> f64 {
    match value.as_str() {
        Some("OK") => 0.0,
        Some("Warning") => 1.0,
        Some("Critical") => 2.0,
        _ => 3.0,
    }
}

fn members(value: &Value) -> Option<&Vec<Value>> {
    value["Members"].as_array()
}

pub fn system(out: &mut Vec<Metric>, value: &Value) {
    add(
        out,
        "redfish_system_health",
        vec![],
        health(
            value["Status"]["HealthRollup"]
                .as_str()
                .map_or(&value["Status"]["Health"], |_| {
                    &value["Status"]["HealthRollup"]
                }),
        ),
    );
    if let Some(on) = value["PowerState"].as_str() {
        if on == "On" || on == "Off" {
            add(out, "redfish_power_on", vec![], f64::from(on == "On"));
        }
    }
}

pub fn target_info(out: &mut Vec<Metric>, profile: &str, model: &str, device_type: &str) {
    add(
        out,
        "redfish_target_info",
        vec![
            ("profile", profile.into()),
            ("model", model.into()),
            ("device_type", device_type.into()),
        ],
        1.0,
    );
}

pub fn chassis(out: &mut Vec<Metric>, value: &Value) {
    if !value["Status"]["Health"].is_null() {
        add(
            out,
            "redfish_chassis_health",
            vec![],
            health(&value["Status"]["Health"]),
        );
    }
}

pub fn processors(out: &mut Vec<Metric>, value: &Value) {
    let Some(members) = members(value) else {
        return;
    };
    // A collection containing links but no expanded data cannot establish a count.
    if members
        .iter()
        .any(|m| m["ProcessorType"].as_str().is_none())
    {
        return;
    }
    let cpus: Vec<_> = members
        .iter()
        .filter(|m| m["ProcessorType"] == "CPU")
        .collect();
    add(out, "redfish_cpu_count", vec![], cpus.len() as f64);
    for cpu in cpus {
        let socket = string(&cpu["Socket"]);
        add(
            out,
            "redfish_cpu_health",
            vec![("socket", socket.clone())],
            health(&cpu["Status"]["Health"]),
        );
        if let Some(v) = number(&cpu["MaxSpeedMHz"]) {
            add(
                out,
                "redfish_cpu_max_speed_mhz",
                vec![("socket", socket)],
                v,
            );
        }
    }
}

pub fn memory(out: &mut Vec<Metric>, value: &Value) {
    let Some(members) = members(value) else {
        return;
    };
    if members.iter().any(|m| m["CapacityMiB"].is_null()) {
        return;
    }
    let dimms: Vec<_> = members
        .iter()
        .filter(|m| {
            m["MemoryDeviceType"] != "HBM" && number(&m["CapacityMiB"]).is_some_and(|n| n > 0.0)
        })
        .collect();
    add(out, "redfish_dimm_count", vec![], dimms.len() as f64);
    let capacity_mib: f64 = dimms.iter().filter_map(|d| number(&d["CapacityMiB"])).sum();
    add(
        out,
        "redfish_memory_total_bytes",
        vec![],
        capacity_mib * 1024.0 * 1024.0,
    );
    for dimm in dimms {
        add(
            out,
            "redfish_dimm_health",
            vec![("slot", string(&dimm["Name"]))],
            health(&dimm["Status"]["Health"]),
        );
    }
}

// Drive.Protocol is nullable. PCIe/OEM/MultiProtocol do not prove whether the
// device speaks NVMe. Never classify using a Name, serial or controller capability.
fn drive_protocol(value: &Value) -> (&'static str, Option<&str>) {
    match value.get("Protocol") {
        None | Some(Value::Null) => ("missing", None),
        Some(Value::String(p)) if p.trim().is_empty() => ("empty", None),
        Some(Value::String(p)) => match p.as_str() {
            "NVMe" | "SATA" | "SAS" | "AHCI" | "USB" | "FC" | "iSCSI" | "FCoE" | "FCP"
            | "FICON" | "NVMeOverFabrics" | "eMMC" => ("recognized", Some(p)),
            "PCIe" | "OEM" | "MultiProtocol" => ("ambiguous", Some(p)),
            _ => ("unsupported", None),
        },
        Some(_) => ("invalid_type", None),
    }
}

/// Counts only fully fetched, bounded resource inventories. Observed resources
/// and confirmed NVMe entries are not a claim about complete physical inventory.
pub fn nvme_count(out: &mut Vec<Metric>, drives: &[Value]) -> bool {
    if drives.len() > 16 || drives.iter().any(|d| !d.is_object()) {
        return false;
    }
    add(
        out,
        "redfish_drive_inventory_observed_count",
        vec![],
        drives.len() as f64,
    );
    let nvme = drives
        .iter()
        .filter(|d| drive_protocol(d).1 == Some("NVMe"))
        .count();
    add(
        out,
        "redfish_drive_nvme_observed_count",
        vec![],
        nvme as f64,
    );
    for state in [
        "recognized",
        "missing",
        "empty",
        "invalid_type",
        "ambiguous",
        "unsupported",
    ] {
        add(
            out,
            "redfish_drive_protocol_observed_count",
            vec![("protocol_state", state.into())],
            drives
                .iter()
                .filter(|d| drive_protocol(d).0 == state)
                .count() as f64,
        );
    }
    let classified = drives.iter().all(|d| drive_protocol(d).0 == "recognized");
    if classified {
        add(out, "redfish_nvme_drive_count", vec![], nvme as f64);
    }
    classified
}

/// B300 inventory is complete only when both expanded collections agree on every NVMe unit.
pub fn b300_nvme_inventory(out: &mut Vec<Metric>, storage: &Value, simple: &Value) -> bool {
    fn complete_members(value: &Value) -> Option<&Vec<Value>> {
        let members = value["Members"].as_array()?;
        let advertised = value["Members@odata.count"].as_u64()?;
        (value["Members@odata.nextLink"].is_null()
            && members.len() <= 64
            && advertised == members.len() as u64)
            .then_some(members)
    }

    let (Some(units), Some(simple_units)) = (complete_members(storage), complete_members(simple))
    else {
        return false;
    };
    let mut simple_by_storage = BTreeMap::new();
    for unit in simple_units {
        let Some(path) = unit["Links"]["Storage"]["@odata.id"].as_str() else {
            return false;
        };
        if simple_by_storage.insert(path, unit).is_some() {
            return false;
        }
    }

    let mut staged = Vec::new();
    let mut identities = BTreeSet::new();
    let mut seen_units = BTreeSet::new();
    let mut count = 0;
    for unit in units {
        let Some(path) = unit["@odata.id"].as_str() else {
            return false;
        };
        if !seen_units.insert(path) {
            return false;
        }
        let Some(controllers) = unit["StorageControllers"].as_array() else {
            return false;
        };
        let nvme: Vec<_> = controllers
            .iter()
            .filter(|controller| {
                controller["SupportedDeviceProtocols"]
                    .as_array()
                    .is_some_and(|protocols| protocols.iter().any(|p| p == "NVMe"))
            })
            .collect();
        if nvme.is_empty() {
            continue;
        }
        if nvme.len() != 1 {
            return false;
        }
        let Some(simple_unit) = simple_by_storage.get(path) else {
            return false;
        };
        let Some(devices) = simple_unit["Devices"].as_array() else {
            return false;
        };
        if devices.len() != 1 {
            return false;
        }
        let device = &devices[0];
        let Some(name) = device["Name"].as_str().filter(|s| !s.is_empty()) else {
            return false;
        };
        let Some(serial) = nvme[0]["SerialNumber"].as_str().filter(|s| !s.is_empty()) else {
            return false;
        };
        let Some(capacity) = number(&device["CapacityBytes"]).filter(|n| n.is_finite() && *n > 0.0)
        else {
            return false;
        };
        if device["Status"]["Health"].as_str().is_none() {
            return false;
        }
        let Some(state) = device["Status"]["State"].as_str() else {
            return false;
        };
        if !identities.insert((name, serial)) {
            return false;
        }
        let labels = vec![
            ("name", string(&device["Name"])),
            ("serial", string(&nvme[0]["SerialNumber"])),
        ];
        add(
            &mut staged,
            "redfish_drive_health",
            labels.clone(),
            health(&device["Status"]["Health"]),
        );
        add(
            &mut staged,
            "redfish_drive_enabled",
            labels.clone(),
            f64::from(state == "Enabled"),
        );
        add(
            &mut staged,
            "redfish_drive_capacity_bytes",
            labels,
            capacity,
        );
        count += 1;
    }
    if count == 0 {
        return false;
    }
    add(
        &mut staged,
        "redfish_nvme_drive_count",
        vec![],
        count as f64,
    );
    out.extend(staged);
    true
}

pub fn drive(out: &mut Vec<Metric>, value: &Value) {
    if !value.is_object() {
        return;
    }
    let labels = vec![
        ("name", string(&value["Name"])),
        ("serial", string(&value["SerialNumber"])),
    ];
    let (state, protocol) = drive_protocol(value);
    let mut info_labels = labels.clone();
    info_labels.push(("protocol_state", state.into()));
    if let Some(protocol) = protocol {
        info_labels.push(("protocol", protocol.into()));
    }
    if value["Id"].as_str().is_some_and(|id| !id.trim().is_empty()) {
        info_labels.push(("resource_id", string(&value["Id"])));
    }
    add(out, "redfish_drive_protocol_info", info_labels, 1.);
    add(
        out,
        "redfish_drive_health",
        labels.clone(),
        health(&value["Status"]["Health"]),
    );
    if let Some(v) = number(&value["PredictedMediaLifeLeftPercent"]) {
        add(out, "redfish_drive_life_left_percent", labels.clone(), v);
    }
    if let Some(v) = number(&value["CapacityBytes"]) {
        add(out, "redfish_drive_capacity_bytes", labels, v);
    }
}

pub fn drive_wear(out: &mut Vec<Metric>, value: &Value) {
    if let Some(v) =
        number(&value["PredictedMediaLifeLeftPercent"]).filter(|v| (0.0..=100.0).contains(v))
    {
        add(
            out,
            "redfish_drive_life_left_percent",
            vec![
                ("name", string(&value["Name"])),
                ("serial", string(&value["SerialNumber"])),
            ],
            v,
        );
    }
}

fn group(name: &str) -> Option<&'static str> {
    if name.starts_with("redfish_drive_") || name == "redfish_nvme_drive_count" {
        Some("drives")
    } else if name.starts_with("redfish_cpu_")
        || name.starts_with("redfish_dimm_")
        || name == "redfish_memory_total_bytes"
    {
        Some("compute")
    } else if name.starts_with("redfish_gpu_") {
        Some("gpu")
    } else if name.contains("nic_") {
        Some("network")
    } else if name.starts_with("redfish_sensor_")
        || name.contains("power_")
        || name.contains("psu_")
        || name.contains("fan_")
        || name.contains("temperature_")
    {
        Some("power")
    } else if name.ends_with("_health") || name.ends_with("_info") {
        Some("health")
    } else {
        None
    }
}

pub(crate) fn epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

/// Cached slow values retain the observation timestamp in Prometheus text format;
/// repeated reads do not turn a cached sample into newly observed hardware data.
pub fn render_slow(target: &str, mut result: Collection) -> String {
    let observed = epoch();
    let present = !result.samples.is_empty();
    for (metric, value) in [
        (
            "redfish_group_success",
            f64::from(present && result.errors == 0 && !result.blocked),
        ),
        ("redfish_group_last_attempt_timestamp_seconds", observed),
    ] {
        add(
            &mut result.samples,
            metric,
            vec![("group", "drive_wear".into())],
            value,
        );
    }
    if present && result.errors == 0 && !result.blocked {
        add(
            &mut result.samples,
            "redfish_group_last_success_timestamp_seconds",
            vec![("group", "drive_wear".into())],
            observed,
        );
    }
    let text = render(target, result, 0.);
    text.lines()
        .map(|line| {
            if line.starts_with('#') {
                format!("{line}\n")
            } else {
                format!("{line} {}\n", (observed * 1000.) as u64)
            }
        })
        .collect()
}

pub fn nics(out: &mut Vec<Metric>, value: &Value, kind: &'static str) {
    let Some(members) = members(value) else {
        return;
    };
    if members.iter().any(|m| m["LinkStatus"].as_str().is_none()) {
        return;
    }
    let mut onboard = 0;
    let mut onboard_up = 0;
    for nic in members {
        let mac = string(&nic["MACAddress"]);
        if kind == "onboard" && mac.is_empty() {
            continue;
        }
        let id = string(&nic["Id"]);
        let link = f64::from(nic["LinkStatus"] == "LinkUp");
        add(
            out,
            "redfish_nic_link_up",
            vec![("kind", kind.into()), ("name", id.clone()), ("mac", mac)],
            link,
        );
        if let Some(speed) = number(&nic["SpeedMbps"]) {
            add(
                out,
                "redfish_nic_speed_mbps",
                vec![("kind", kind.into()), ("name", id)],
                speed,
            );
        }
        if kind == "onboard" {
            onboard += 1;
            onboard_up += link as usize;
        }
    }
    if kind == "onboard" {
        add(out, "redfish_onboard_nic_count", vec![], onboard as f64);
        add(
            out,
            "redfish_onboard_nic_up_count",
            vec![],
            onboard_up as f64,
        );
    }
}

/// Rolling 1U12E members keep their own observation time. Aggregates require a
/// complete directory and use the oldest contributing observation, not replay time.
pub(crate) fn cpu_inventory_observations(
    out: &mut Vec<Metric>,
    kind: &'static str,
    values: &[(&str, &Value, u64)],
    complete: bool,
) {
    let encode = |samples: &mut Vec<Metric>, value: &Value| match kind {
        "processors" => processors(samples, value),
        "memory" => memory(samples, value),
        "onboard" | "bmc" => nics(samples, value, kind),
        _ => {}
    };
    let aggregate = |sample: &Metric| {
        matches!(
            sample.name,
            "redfish_cpu_count"
                | "redfish_dimm_count"
                | "redfish_memory_total_bytes"
                | "redfish_onboard_nic_count"
                | "redfish_onboard_nic_up_count"
        )
    };
    for (path, value, source) in values {
        let mut samples = Vec::new();
        encode(&mut samples, &serde_json::json!({"Members":[value]}));
        for mut sample in samples.into_iter().filter(|sample| !aggregate(sample)) {
            sample.source_ms = Some(*source);
            sample.labels.push(("resource_id", (*path).into()));
            out.push(sample);
        }
    }
    if complete {
        let mut samples = Vec::new();
        encode(
            &mut samples,
            &serde_json::json!({"Members":values.iter().map(|(_,v,_)| *v).collect::<Vec<_>>()}),
        );
        let oldest = values
            .iter()
            .map(|(_, _, source)| *source)
            .min()
            .unwrap_or_else(|| (epoch() * 1000.) as u64);
        for mut sample in samples.into_iter().filter(aggregate) {
            sample.source_ms = Some(oldest);
            out.push(sample);
        }
    }
}

pub fn manager(out: &mut Vec<Metric>, value: &Value) {
    add(
        out,
        "redfish_bmc_info",
        vec![
            ("firmware", string(&value["FirmwareVersion"])),
            ("model", string(&value["Model"])),
        ],
        1.0,
    );
}

pub fn thermal(out: &mut Vec<Metric>, value: &Value) {
    if let Some(fans) = value["Fans"].as_array() {
        add(out, "redfish_fan_count", vec![], fans.len() as f64);
        for fan in fans {
            let labels = vec![("name", string(&fan["Name"]))];
            add(
                out,
                "redfish_fan_health",
                labels.clone(),
                health(&fan["Status"]["Health"]),
            );
            if fan["ReadingUnits"]
                .as_str()
                .is_none_or(|unit| unit == "RPM")
            {
                if let Some(rpm) = number(&fan["Reading"]) {
                    add(out, "redfish_fan_rpm", labels, rpm);
                }
            }
        }
    }
    if let Some(temperatures) = value["Temperatures"].as_array() {
        for sensor in temperatures {
            let name = string(&sensor["Name"]);
            let upper = name.to_ascii_uppercase();
            if ["TEMP_CPU", "FSC", "TLIMIT", "TCONTROL"]
                .iter()
                .any(|exclude| upper.contains(exclude))
            {
                continue;
            }
            if let Some(celsius) = number(&sensor["ReadingCelsius"]) {
                add(
                    out,
                    "redfish_temperature_celsius",
                    vec![
                        ("name", name),
                        ("context", string(&sensor["PhysicalContext"])),
                    ],
                    celsius,
                );
            }
        }
    }
}

/// PowerSupplies on some AMI models contains CPU/DIMM power domains rather
/// than identifiable physical PSUs. Export collection entries, never infer
/// physical inventory or sum overlapping domains into chassis consumption.
/// False means an advertised array is malformed/oversized, not unsupported.
pub fn power(out: &mut Vec<Metric>, value: &Value) -> bool {
    if let Some(controls) = value["PowerControl"].as_array() {
        if controls.len() == 1 {
            if let Some(watts) = number(&controls[0]["PowerConsumedWatts"]) {
                add(out, "redfish_chassis_power_watts", vec![], watts);
            }
        }
    }
    let supplies = match value.get("PowerSupplies") {
        None | Some(Value::Null) => return true,
        Some(Value::Array(entries))
            if entries.len() <= 64 && entries.iter().all(Value::is_object) =>
        {
            entries
        }
        _ => return false,
    };
    add(
        out,
        "redfish_sensor_power_supply_entry_count",
        vec![],
        supplies.len() as f64,
    );
    for (index, supply) in supplies.iter().enumerate() {
        // Array index is only an observation-local discriminator, not a slot
        // or persistent physical identity. Preserve the BMC MemberId when given.
        let mut labels = vec![("entry_index", index.to_string())];
        for (key, label) in [("Name", "name"), ("MemberId", "member_id")] {
            if supply[key].as_str().is_some_and(|s| !s.trim().is_empty()) {
                labels.push((label, string(&supply[key])));
            }
        }
        add(
            out,
            "redfish_sensor_power_supply_entry_info",
            labels.clone(),
            1.,
        );
        if supply["Status"]["Health"].is_string() {
            add(
                out,
                "redfish_sensor_power_supply_entry_health",
                labels.clone(),
                health(&supply["Status"]["Health"]),
            );
        }
        if let Some(watts) = number(&supply["PowerOutputWatts"]) {
            add(
                out,
                "redfish_sensor_power_supply_entry_output_watts",
                labels,
                watts,
            );
        }
    }
    true
}

pub fn sensor(out: &mut Vec<Metric>, value: &Value) {
    let name = string(&value["Name"]);
    if name.is_empty() {
        return;
    }
    let labels = vec![("name", name)];
    if !value["Status"]["Health"].is_null() {
        add(
            out,
            "redfish_sensor_health",
            labels.clone(),
            health(&value["Status"]["Health"]),
        );
    }
    let Some(reading) = number(&value["Reading"]) else {
        return;
    };
    match (
        value["ReadingType"].as_str(),
        value["ReadingUnits"].as_str(),
    ) {
        (Some("Temperature"), Some("Cel")) => {
            add(out, "redfish_sensor_temperature_celsius", labels, reading);
        }
        (Some("Power"), Some("W")) => {
            add(out, "redfish_sensor_power_watts", labels, reading);
        }
        (Some("Rotational"), Some("RPM" | "{rev}/min")) => {
            add(out, "redfish_sensor_fan_rpm", labels, reading);
        }
        _ => {}
    }
}

/// A rolling sensor sample keeps its own observation time, never the replay time.
pub fn sensor_observation(value: &Value, id: &str, observed: f64) -> Vec<Metric> {
    let mut samples = Vec::new();
    sensor(&mut samples, value);
    add(
        &mut samples,
        "redfish_sensor_last_success_timestamp_seconds",
        vec![("name", string(&value["Name"]))],
        observed,
    );
    for sample in &mut samples {
        sample.labels.push(("sensor_id", id.to_owned()));
        sample.source_ms = Some((observed * 1000.) as u64);
    }
    samples
}

pub fn sensor_coverage(out: &mut Vec<Metric>, inventory: usize, cached: usize) {
    for (name, value) in [
        ("redfish_sensor_inventory_count", inventory as f64),
        ("redfish_sensor_cached_count", cached as f64),
        (
            "redfish_sensor_snapshot_complete",
            f64::from(cached == inventory),
        ),
    ] {
        add(out, name, vec![], value);
    }
}

/// Successfully read details can contain unusable data. Keep this separate
/// from valid snapshots and operator-declared expected hardware counts.
pub fn sensor_detail_coverage(
    out: &mut Vec<Metric>,
    inventory: usize,
    valid: usize,
    invalid: usize,
) {
    for (name, value) in [
        ("redfish_sensor_detail_count", (valid + invalid) as f64),
        ("redfish_sensor_invalid_count", invalid as f64),
        (
            "redfish_sensor_detail_complete",
            f64::from(valid + invalid == inventory),
        ),
    ] {
        add(out, name, vec![], value);
    }
}

/// No raw body, Name or invented sensor value/health. Replays preserve the
/// timestamp of the actual HTTP detail response, not this scrape's time.
pub fn sensor_invalid_observation(
    value: &Value,
    id: &str,
    reason: &'static str,
    observed: f64,
) -> Vec<Metric> {
    let mut out = Vec::new();
    let labels = vec![("sensor_id", id.to_owned()), ("reason", reason.to_owned())];
    add(&mut out, "redfish_sensor_invalid_info", labels.clone(), 1.);
    add(
        &mut out,
        "redfish_sensor_detail_last_read_timestamp_seconds",
        labels.clone(),
        observed,
    );
    for (field, present) in [
        ("object", value.is_object()),
        ("id", value["Id"].is_string()),
        ("reading", !value["Reading"].is_null()),
        ("health", value["Status"]["Health"].is_string()),
    ] {
        let mut field_labels = labels.clone();
        field_labels.push(("field", field.to_owned()));
        add(
            &mut out,
            "redfish_sensor_invalid_field_present",
            field_labels,
            f64::from(present),
        );
    }
    for sample in &mut out {
        sample.source_ms = Some((observed * 1000.) as u64);
    }
    out
}

pub fn pcie(out: &mut Vec<Metric>, value: &Value) {
    if let Some(n) = number(&value["Members@odata.count"]) {
        add(out, "redfish_pcie_device_count", vec![], n);
    }
}

pub fn gpu_board(out: &mut Vec<Metric>, value: &Value) {
    add(
        out,
        "redfish_gpu_board_health",
        vec![],
        health(&value["Status"]["HealthRollup"]),
    );
}

pub fn board_power(out: &mut Vec<Metric>, value: &Value) {
    if let Some(watts) = number(&value["PowerWatts"]["Reading"]) {
        add(out, "redfish_board_power_watts", vec![], watts);
    }
}

pub fn gpu_slots(out: &mut Vec<Metric>, count: usize) {
    add(out, "redfish_gpu_count", vec![], count as f64);
}

pub fn gpu_details(out: &mut Vec<Metric>, details: &[(&str, Value)], expected: usize) {
    if details.len() != expected
        || details
            .iter()
            .any(|(_, gpu)| gpu["Status"]["State"].as_str().is_none())
    {
        return;
    }
    let mut online = 0;
    let mut online_capacity_mib = Some(0.0);
    for (reference, gpu) in details {
        let id = reference.rsplit('/').next().unwrap_or_default().to_owned();
        let enabled = gpu["Status"]["State"] == "Enabled";
        online += usize::from(enabled);
        add(
            out,
            "redfish_gpu_online",
            vec![("gpu", id)],
            f64::from(enabled),
        );
        if enabled {
            online_capacity_mib = online_capacity_mib
                .zip(number(&gpu["MemorySummary"]["TotalMemorySizeMiB"]))
                .map(|(total, mib)| total + mib);
        }
    }
    add(out, "redfish_gpu_online_count", vec![], online as f64);
    if let Some(mib) = online_capacity_mib {
        add(
            out,
            "redfish_gpu_memory_total_bytes",
            vec![],
            mib * 1024.0 * 1024.0,
        );
    }
}

fn gpu_index(property: &str, marker: &str, suffix: &str, between: &str) -> Option<String> {
    let after = &property[property.rfind(marker)? + marker.len()..];
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || after[digits.len()..] != format!("{between}{suffix}") {
        None
    } else {
        Some(format!("GPU_{digits}"))
    }
}

fn report(
    out: &mut Vec<Metric>,
    value: &Value,
    marker: &str,
    between: &str,
    mappings: &[(&str, &'static str)],
) {
    for item in value["MetricValues"].as_array().into_iter().flatten() {
        let Some(property) = item["MetricProperty"].as_str() else {
            continue;
        };
        let Some(number) = number(&item["MetricValue"]) else {
            continue;
        };
        for (suffix, metric) in mappings {
            if let Some(gpu) = gpu_index(property, marker, suffix, between) {
                add(out, metric, vec![("gpu", gpu)], number);
                break;
            }
        }
    }
}

pub fn gpu_environment(out: &mut Vec<Metric>, value: &Value) {
    report(
        out,
        value,
        "HGX_GPU_",
        "",
        &[
            ("_TEMP_0", "redfish_gpu_temperature_celsius"),
            ("_Power_0", "redfish_gpu_power_watts"),
        ],
    );
}

pub fn gpu_memory(out: &mut Vec<Metric>, value: &Value) {
    report(
        out,
        value,
        "GPU_",
        "_DRAM_0/MemoryMetrics#",
        &[
            (
                "/LifeTime/CorrectableECCErrorCount",
                "redfish_gpu_ecc_correctable_errors",
            ),
            (
                "/LifeTime/UncorrectableECCErrorCount",
                "redfish_gpu_ecc_uncorrectable_errors",
            ),
            (
                "/Oem/Nvidia/RowRemapping/CorrectableRowRemappingCount",
                "redfish_gpu_row_remap_correctable",
            ),
            (
                "/Oem/Nvidia/RowRemapping/UncorrectableRowRemappingCount",
                "redfish_gpu_row_remap_uncorrectable",
            ),
        ],
    );
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

pub fn render(target: &str, mut result: Collection, duration: f64) -> String {
    let observed = epoch();
    let present: BTreeSet<_> = result
        .samples
        .iter()
        .filter_map(|m| group(m.name))
        .collect();
    // Request success is not component health. Missing groups remain unknown.
    let baseline = !result.samples.iter().any(|m| {
        m.labels
            .iter()
            .any(|(key, value)| *key == "group" && value == "drive_wear")
    });
    for id in ["health", "compute", "drives", "network", "power", "gpu"]
        .into_iter()
        .filter(|_| baseline)
    {
        let successful = result.up
            && !result.blocked
            && present.contains(id)
            && !result.failed_groups.contains(id);
        add(
            &mut result.samples,
            "redfish_group_success",
            vec![("group", id.into())],
            f64::from(successful),
        );
        add(
            &mut result.samples,
            "redfish_group_last_attempt_timestamp_seconds",
            vec![("group", id.into())],
            observed,
        );
        if successful {
            let source = result
                .samples
                .iter()
                .filter(|sample| group(sample.name) == Some(id))
                .filter_map(|sample| sample.source_ms)
                .map(|source| source as f64 / 1000.)
                .fold(observed, f64::min);
            add(
                &mut result.samples,
                "redfish_group_last_success_timestamp_seconds",
                vec![("group", id.into())],
                source,
            );
        }
    }
    add(
        &mut result.samples,
        "redfish_up",
        vec![],
        f64::from(result.up),
    );
    add(
        &mut result.samples,
        "redfish_blocked",
        vec![],
        f64::from(result.blocked),
    );
    add(
        &mut result.samples,
        "redfish_scrape_complete",
        vec![],
        f64::from(result.up && !result.blocked && result.errors == 0),
    );
    add(
        &mut result.samples,
        "redfish_scrape_errors",
        vec![],
        result.errors as f64,
    );
    add(
        &mut result.samples,
        "redfish_scrape_duration_seconds",
        vec![],
        duration,
    );
    let mut declared = BTreeSet::new();
    let mut output = String::new();
    for metric in result.samples {
        if declared.insert(metric.name) {
            output.push_str(&format!("# TYPE {} gauge\n", metric.name));
        }
        output.push_str(metric.name);
        output.push_str("{target=\"");
        output.push_str(&escape_label(target));
        output.push('"');
        for (key, value) in metric.labels {
            output.push_str(&format!(",{key}=\"{}\"", escape_label(&value)));
        }
        output.push_str(&format!("}} {}", metric.value));
        if let Some(source) = metric.source_ms {
            output.push_str(&format!(" {source}"));
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rolling_cpu_members_keep_individual_time_and_complete_aggregates_use_oldest_source() {
        let a = json!({"Name":"same-name","CapacityMiB":32768,"Status":{"Health":"OK"}});
        let b = json!({"Name":"same-name","CapacityMiB":32768,"Status":{"Health":"Warning"}});
        let values = [
            ("/redfish/v1/Systems/Self/Memory/0", &a, 1700000000123),
            ("/redfish/v1/Systems/Self/Memory/1", &b, 1700000100123),
        ];
        let render_members = |complete| {
            let mut out = Collection::failed();
            out.up = true;
            out.errors = 0;
            cpu_inventory_observations(&mut out.samples, "memory", &values, complete);
            render("CPU", out, 0.1)
        };
        let text = render_members(true);
        let health = text
            .lines()
            .filter(|line| line.starts_with("redfish_dimm_health{"))
            .collect::<Vec<_>>();
        assert_eq!(health.len(), 2);
        assert!(
            health[0].contains("resource_id=\"/redfish/v1/Systems/Self/Memory/0\"")
                && health[0].ends_with(" 1700000000123")
        );
        assert!(
            health[1].contains("resource_id=\"/redfish/v1/Systems/Self/Memory/1\"")
                && health[1].ends_with(" 1700000100123")
        );
        assert!(text.contains("redfish_dimm_count{target=\"CPU\"} 2 1700000000123"));
        assert!(text.contains("redfish_group_last_success_timestamp_seconds{target=\"CPU\",group=\"compute\"} 1700000000.123"));
        let partial = render_members(false);
        assert!(
            partial.contains("redfish_dimm_health{") && !partial.contains("redfish_dimm_count{")
        );
    }
    #[test]
    fn rolling_sensor_values_and_group_freshness_use_the_original_observation() {
        let mut result = Collection::failed();
        result.up = true;
        result.errors = 0;
        result.samples = sensor_observation(
            &json!({"Name":"CPU Temp","Reading":42,"ReadingType":"Temperature","ReadingUnits":"Cel","Status":{"Health":"OK"}}),
            "12",
            1000.25,
        );
        sensor_coverage(&mut result.samples, 1, 1);
        let text = render("fixture", result, 0.1);
        assert!(text.contains("redfish_sensor_temperature_celsius{target=\"fixture\",name=\"CPU Temp\",sensor_id=\"12\"} 42 1000250"));
        assert!(text.contains("redfish_sensor_last_success_timestamp_seconds{target=\"fixture\",name=\"CPU Temp\",sensor_id=\"12\"} 1000.25 1000250"));
        assert!(text.contains("redfish_group_last_success_timestamp_seconds{target=\"fixture\",group=\"power\"} 1000.25\n"));
        assert!(text.contains("redfish_sensor_snapshot_complete{target=\"fixture\"} 1\n"));
    }
    #[test]
    fn group_failure_is_independent_and_missing_groups_are_not_successful() {
        let mut result = Collection::failed();
        result.up = true;
        result.failed_groups.insert("drives");
        system(&mut result.samples, &json!({"Status":{"Health":"OK"}}));
        drive(&mut result.samples, &json!({"Status":{"Health":"OK"}}));
        let text = render("fixture", result, 1.);
        assert!(text.contains("redfish_group_success{target=\"fixture\",group=\"health\"} 1"));
        assert!(text.contains("redfish_group_success{target=\"fixture\",group=\"drives\"} 0"));
        assert!(text.contains("redfish_group_success{target=\"fixture\",group=\"gpu\"} 0"));
        assert!(!text.contains(
            "redfish_group_last_success_timestamp_seconds{target=\"fixture\",group=\"drives\"}"
        ));
    }
    #[test]
    fn slow_snapshot_has_original_timestamps_and_never_replays_baseline_inventory() {
        let mut result = Collection::failed();
        result.up = true;
        result.errors = 0;
        drive_wear(
            &mut result.samples,
            &json!({"Name":"Disk", "SerialNumber":"fixture", "PredictedMediaLifeLeftPercent":89}),
        );
        let snapshot = render_slow("fixture", result);
        assert!(snapshot.contains("redfish_drive_life_left_percent"));
        assert!(!snapshot.contains("redfish_drive_capacity_bytes"));
        assert!(!snapshot.contains("group=\"drives\""));
        assert!(snapshot
            .lines()
            .filter(|line| !line.starts_with('#'))
            .all(|line| line.split_whitespace().count() == 3));
        let replay = snapshot.clone();
        assert_eq!(replay, snapshot); // cached text, not regenerated on subsequent scrape.
    }

    #[test]
    fn reports_gpu_telemetry_without_dram_temperature_confusion() {
        let mut out = vec![];
        gpu_environment(
            &mut out,
            &json!({"MetricValues": [
                {"MetricProperty":"/redfish/v1/Chassis/HGX_GPU_0_TEMP_0","MetricValue":"42"},
                {"MetricProperty":"/redfish/v1/Chassis/HGX_GPU_0_DRAM_0_TEMP_0","MetricValue":"80"},
                {"MetricProperty":"/redfish/v1/Chassis/HGX_GPU_1_Power_0","MetricValue":"1300"}
            ]}),
        );
        assert_eq!(
            out.iter()
                .filter(|m| m.name == "redfish_gpu_temperature_celsius")
                .count(),
            1
        );
        assert_eq!(
            out.iter()
                .find(|m| m.name == "redfish_gpu_power_watts")
                .map(|m| m.value),
            Some(1300.0)
        );
    }

    #[test]
    fn incomplete_gpu_details_never_report_false_online_zero() {
        let mut out = vec![];
        gpu_slots(&mut out, 8);
        gpu_details(
            &mut out,
            &[("/redfish/v1/GPU_0", json!({"Status":{"State":"Enabled"}}))],
            8,
        );
        assert!(out.iter().all(|m| m.name != "redfish_gpu_online_count"));
    }

    #[test]
    fn gpu_memory_sums_online_cards_and_omits_incomplete_capacity() {
        let mut out = vec![];
        gpu_details(
            &mut out,
            &[
                (
                    "/redfish/v1/GPU_0",
                    json!({"Status":{"State":"Enabled"},"MemorySummary":{"TotalMemorySizeMiB":100}}),
                ),
                (
                    "/redfish/v1/GPU_1",
                    json!({"Status":{"State":"Enabled"},"MemorySummary":{"TotalMemorySizeMiB":200}}),
                ),
                ("/redfish/v1/GPU_2", json!({"Status":{"State":"Disabled"}})),
            ],
            3,
        );
        assert_eq!(
            out.iter()
                .find(|m| m.name == "redfish_gpu_memory_total_bytes")
                .map(|m| m.value),
            Some(300.0 * 1024.0 * 1024.0)
        );
        let mut incomplete = vec![];
        gpu_details(
            &mut incomplete,
            &[("/redfish/v1/GPU_0", json!({"Status":{"State":"Enabled"}}))],
            1,
        );
        assert!(incomplete
            .iter()
            .all(|m| m.name != "redfish_gpu_memory_total_bytes"));
    }

    #[test]
    fn missing_expanded_memory_never_reports_false_dimm_zero() {
        let mut out = vec![];
        memory(
            &mut out,
            &json!({"Members":[{"@odata.id":"/redfish/v1/Memory/0"}]}),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn power_supply_entries_are_observations_not_physical_psu_inventory() {
        let entries: Vec<_> = (0..22).map(|i| json!({"MemberId":i.to_string(),"Name":"CPU/DDR5 Power","PowerOutputWatts":if i == 0 {0}else{10},"Status":{"Health":"OK"}})).collect();
        let mut samples = vec![];
        power(&mut samples, &json!({"PowerSupplies":entries}));
        assert!(!samples.iter().any(|m| m.name.starts_with("redfish_psu_")));
        assert_eq!(
            samples
                .iter()
                .find(|m| m.name == "redfish_sensor_power_supply_entry_count")
                .map(|m| m.value),
            Some(22.)
        );
        let watts: Vec<_> = samples
            .iter()
            .filter(|m| m.name == "redfish_sensor_power_supply_entry_output_watts")
            .collect();
        assert_eq!(watts.len(), 22);
        assert_eq!(watts[0].value, 0.);
        assert!(watts
            .iter()
            .enumerate()
            .all(|(i, m)| m.labels.contains(&("entry_index", i.to_string()))
                && m.labels.contains(&("member_id", i.to_string()))));
        assert!(!samples
            .iter()
            .any(|m| m.name == "redfish_chassis_power_watts")); // No sum of possibly overlapping domains.
        let mut anonymous = vec![];
        power(
            &mut anonymous,
            &json!({"PowerSupplies":[{}, {"Name":"same"}, {"Name":"same","Status":{"Health":"Unrecognized"}}]}),
        );
        assert!(!anonymous.iter().any(|m| m
            .labels
            .iter()
            .any(|(k, v)| *k == "name" && v.starts_with("PSU-"))));
        assert!(!anonymous.iter().any(|m| m.name.ends_with("output_watts")));
        assert_eq!(
            anonymous
                .iter()
                .filter(|m| m.name == "redfish_sensor_power_supply_entry_health")
                .map(|m| m.value)
                .collect::<Vec<_>>(),
            vec![3.]
        );
    }

    #[test]
    fn malformed_or_oversized_power_arrays_do_not_claim_complete_entry_counts() {
        for value in [
            json!({"PowerSupplies":"invalid"}),
            json!({"PowerSupplies":[{},null]}),
            json!({"PowerSupplies":vec![json!({});65]}),
        ] {
            let mut out = vec![];
            assert!(!power(&mut out, &value));
            assert!(out.is_empty());
        }
        for value in [json!({}), json!({"PowerSupplies":null})] {
            let mut out = vec![];
            assert!(power(&mut out, &value));
            assert!(out.is_empty());
        }
        let mut out = vec![];
        assert!(power(&mut out, &json!({"PowerSupplies":[]})));
        assert_eq!(out[0].name, "redfish_sensor_power_supply_entry_count");
        assert_eq!(out[0].value, 0.); // Observed empty array, not zero physical PSUs.
    }

    #[test]
    fn cpu_power_and_sensor_units_are_normalized_without_guessing() {
        let mut out = vec![];
        power(
            &mut out,
            &json!({"PowerControl":[{"PowerConsumedWatts":375}],"PowerSupplies":[{"Name":"PSU1","Status":{"Health":"OK"},"PowerOutputWatts":190}]}),
        );
        sensor(
            &mut out,
            &json!({"Name":"CPU0","ReadingType":"Temperature","ReadingUnits":"Cel","Reading":51,"Status":{"Health":"OK"}}),
        );
        sensor(
            &mut out,
            &json!({"Name":"Unknown","ReadingType":"Temperature","ReadingUnits":"F","Reading":150}),
        );
        assert_eq!(
            out.iter()
                .find(|m| m.name == "redfish_chassis_power_watts")
                .map(|m| m.value),
            Some(375.0)
        );
        assert_eq!(
            out.iter()
                .find(|m| m.name == "redfish_sensor_temperature_celsius")
                .map(|m| m.value),
            Some(51.0)
        );
        assert_eq!(
            out.iter()
                .filter(|m| m.name == "redfish_sensor_temperature_celsius")
                .count(),
            1
        );
    }

    #[test]
    fn nvme_count_requires_known_protocol_for_every_drive() {
        let mut out = vec![];
        nvme_count(&mut out, &[json!({"Protocol":"NVMe"}), json!({})]);
        assert!(!out.iter().any(|m| m.name == "redfish_nvme_drive_count"));
        out.clear();
        nvme_count(
            &mut out,
            &[json!({"Protocol":"NVMe"}), json!({"Protocol":"SATA"})],
        );
        assert_eq!(
            out.iter()
                .find(|m| m.name == "redfish_nvme_drive_count")
                .map(|m| m.value),
            Some(1.0)
        );
    }

    #[test]
    fn incomplete_drive_protocols_expose_observations_without_guessing_a_total() {
        let mut drives = (0..8)
            .map(|i| json!({"Name":format!("disk-{i}"),"Protocol":"NVMe"}))
            .collect::<Vec<_>>();
        drives.extend([
            json!({"Name":"NVMe-by-name-only"}),
            json!({"Protocol":null}),
            json!({"Protocol":""}),
            json!({"Protocol":"PCIe"}),
        ]);
        let mut out = vec![];
        for value in &drives {
            drive(&mut out, value);
        }
        nvme_count(&mut out, &drives);
        let value = |name| out.iter().find(|m| m.name == name).map(|m| m.value);
        assert_eq!(value("redfish_drive_inventory_observed_count"), Some(12.));
        assert_eq!(value("redfish_drive_nvme_observed_count"), Some(8.));
        assert_eq!(value("redfish_nvme_drive_count"), None);
        assert_eq!(
            out.iter()
                .filter(|m| m.name == "redfish_drive_protocol_info")
                .count(),
            12
        );
        assert!(out.iter().any(|m| m.name == "redfish_drive_protocol_info"
            && m.labels.contains(&("name", "NVMe-by-name-only".into()))
            && m.labels.contains(&("protocol_state", "missing".into()))));
        assert!(out.iter().any(|m| m.name == "redfish_drive_protocol_info"
            && m.labels.contains(&("protocol", "PCIe".into()))
            && m.labels.contains(&("protocol_state", "ambiguous".into()))));
    }

    #[test]
    fn invalid_drive_protocols_never_become_a_zero_nvme_inventory() {
        for protocol in [
            json!(""),
            json!(" "),
            json!("nvme"),
            json!("Unknown"),
            json!("OEM"),
            json!("MultiProtocol"),
            json!(42),
            json!(null),
        ] {
            let mut out = vec![];
            nvme_count(&mut out, &[json!({"Name":"NVMe0","Protocol":protocol})]);
            assert!(!out.iter().any(|m| m.name == "redfish_nvme_drive_count"));
        }
        let mut empty = vec![];
        nvme_count(&mut empty, &[]);
        assert_eq!(
            empty
                .iter()
                .find(|m| m.name == "redfish_nvme_drive_count")
                .map(|m| m.value),
            Some(0.)
        );
        for drives in [vec![json!(null)], vec![json!({"Protocol":"NVMe"}); 17]] {
            let mut invalid = vec![];
            nvme_count(&mut invalid, &drives);
            assert!(invalid.is_empty());
        }
    }

    #[test]
    fn b300_nvme_inventory_uses_complete_linked_simple_storage() {
        let storage = json!({
            "Members@odata.count": 2,
            "Members": [
                {"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_0", "StorageControllers":[]},
                {"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A",
                 "StorageControllers":[{"SerialNumber":"NVME-A", "SupportedDeviceProtocols":["NVMe"]}]}
            ]
        });
        let simple = json!({
            "Members@odata.count": 2,
            "Members": [
                {"Id":"0", "Links":{"Storage":{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_0"}}, "Devices":[]},
                {"Id":"A", "Links":{"Storage":{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A"}},
                 "Devices":[{"Name":"PCIeDevice_01_8F_00", "Model":"Samsung NVMe",
                             "CapacityBytes":3840755982336_u64, "Status":{"Health":"OK", "State":"Enabled"}}]}
            ]
        });
        let mut out = vec![];
        assert!(b300_nvme_inventory(&mut out, &storage, &simple));
        assert_eq!(
            out.iter()
                .find(|m| m.name == "redfish_nvme_drive_count")
                .map(|m| m.value),
            Some(1.0)
        );
        let health = out
            .iter()
            .find(|m| m.name == "redfish_drive_health")
            .unwrap();
        assert_eq!(health.value, 0.0);
        assert!(health.labels.contains(&("serial", "NVME-A".into())));
        assert_eq!(
            out.iter()
                .find(|m| m.name == "redfish_drive_capacity_bytes")
                .map(|m| m.value),
            Some(3840755982336.0)
        );
        assert!(!out
            .iter()
            .any(|m| m.name == "redfish_drive_life_left_percent"));
    }

    #[test]
    fn b300_nvme_inventory_rejects_missing_or_paginated_members() {
        let storage = json!({
            "Members@odata.count": 1,
            "Members": [{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A",
                         "StorageControllers":[{"SerialNumber":"NVME-A", "SupportedDeviceProtocols":["NVMe"]}]}]
        });
        let simple = json!({
            "Members@odata.count": 1,
            "Members": [{"Links":{"Storage":{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_B"}},
                         "Devices":[{"Name":"NVMe", "CapacityBytes":100, "Status":{"Health":"OK"}}]}]
        });
        let mut out = vec![];
        assert!(!b300_nvme_inventory(&mut out, &storage, &simple));
        assert!(out.is_empty());
        let mut paginated = simple;
        paginated["Members@odata.nextLink"] = json!("/next");
        assert!(!b300_nvme_inventory(&mut out, &storage, &paginated));
        assert!(out.is_empty());

        let missing_capacity = json!({
            "Members@odata.count": 1,
            "Members": [{"Links":{"Storage":{"@odata.id":"/redfish/v1/Systems/System_0/Storage/StorageUnit_A"}},
                         "Devices":[{"Name":"NVMe", "Status":{"Health":"OK", "State":"Enabled"}}]}]
        });
        assert!(!b300_nvme_inventory(&mut out, &storage, &missing_capacity));
        assert!(out.is_empty());
    }

    #[test]
    fn labels_are_escaped_and_no_stale_component_is_fabricated() {
        let output = render("B300-01", Collection::failed(), 0.1);
        assert!(output.contains("redfish_up{target=\"B300-01\"} 0"));
        assert!(!output.contains("redfish_gpu_online_count"));
        assert_eq!(escape_label("a\"b\\c\n"), "a\\\"b\\\\c\\n");
    }
}
