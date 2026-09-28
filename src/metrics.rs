use crate::bmc::Collection;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub struct Metric {
    name: &'static str,
    labels: Vec<(&'static str, String)>,
    value: f64,
}

fn add(out: &mut Vec<Metric>, name: &'static str, labels: Vec<(&'static str, String)>, value: f64) {
    if value.is_finite() {
        out.push(Metric {
            name,
            labels,
            value,
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

pub fn nvme_count(out: &mut Vec<Metric>, drives: &[Value]) {
    if drives
        .iter()
        .all(|drive| drive["Protocol"].as_str().is_some())
    {
        add(
            out,
            "redfish_nvme_drive_count",
            vec![],
            drives
                .iter()
                .filter(|drive| drive["Protocol"] == "NVMe")
                .count() as f64,
        );
    }
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
    let labels = vec![
        ("name", string(&value["Name"])),
        ("serial", string(&value["SerialNumber"])),
    ];
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

pub fn power(out: &mut Vec<Metric>, value: &Value) {
    if let Some(controls) = value["PowerControl"].as_array() {
        if controls.len() == 1 {
            if let Some(watts) = number(&controls[0]["PowerConsumedWatts"]) {
                add(out, "redfish_chassis_power_watts", vec![], watts);
            }
        }
    }
    if let Some(supplies) = value["PowerSupplies"].as_array() {
        add(out, "redfish_psu_count", vec![], supplies.len() as f64);
        for (index, supply) in supplies.iter().enumerate() {
            let name = supply["Name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("PSU-{index}"));
            let labels = vec![("name", name)];
            if !supply["Status"]["Health"].is_null() {
                add(
                    out,
                    "redfish_psu_health",
                    labels.clone(),
                    health(&supply["Status"]["Health"]),
                );
            }
            if let Some(watts) = number(&supply["PowerOutputWatts"]) {
                add(out, "redfish_psu_output_watts", labels, watts);
            }
        }
    }
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
        output.push_str(&format!("}} {}\n", metric.value));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert!(out.is_empty());
        nvme_count(
            &mut out,
            &[json!({"Protocol":"NVMe"}), json!({"Protocol":"SATA"})],
        );
        assert_eq!(out[0].value, 1.0);
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
