//! Versioned provider contract, without target addresses or credential metadata.
use serde_json::{json, Value};
pub const SCHEMA: &str = "redfish.capabilities.v1";
pub fn profile(id: &str, model: &str, kind: &str) -> Value {
    let gpu = kind == "gpu_server";
    json!({"schema":SCHEMA,"profile":id,"model":model,"device_type":kind,"read_only":true,
        "capabilities":[
            {"id":"health","state":"conditional","reason":"BMC system status required"},
            {"id":"compute_inventory","state":"conditional","reason":"advertised processors and memory"},
            {"id":"drives","state":"conditional","reason":"complete storage inventory required"},
            {"id":"drive_wear","state":if gpu{"unsupported"}else{"conditional"},"reason":if gpu{"slow Drive details excluded from baseline"}else{"only if BMC exposes life remaining"}},
            {"id":"environment","state":"conditional","reason":"advertised resources and known units required"},
            {"id":"gpu","state":if gpu{"conditional"}else{"unsupported"},"reason":if gpu{"HGX inventory/telemetry must be readable"}else{"CPU profile has no GPU collector"}},
            {"id":"nvlink","state":"unsupported","reason":"not provided by current Redfish collector"},
            {"id":"os_utilization","state":"unsupported","reason":"out of band only; no tenant OS access"}
        ],"value_semantics":{"missing":"unknown, not zero","zero":"valid only when emitted","timeout":"scrape error, not unsupported","freshness":"per scrape timestamp"}})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn three_profiles_distinguish_conditional_and_unsupported_without_fake_values() {
        for (id, model, kind) in [
            ("asrockrack_b300_8u16x", "8U16X-GNR2", "gpu_server"),
            ("tyan_b8037", "B8037T73X4-200PE4HR-L", "cpu_server"),
            ("asrockrack_1u10e", "1U10E-ICX2", "cpu_server"),
        ] {
            let doc = profile(id, model, kind);
            assert_eq!(doc["schema"], SCHEMA);
            assert!(doc.get("configured_address").is_none() && doc.get("credential_ref").is_none());
            assert!(doc["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c.get("value").is_none()));
            let gpu = doc["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == "gpu")
                .unwrap();
            assert_eq!(
                gpu["state"],
                if kind == "gpu_server" {
                    "conditional"
                } else {
                    "unsupported"
                }
            );
        }
    }
}
