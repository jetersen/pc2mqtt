//! Home Assistant discovery for the last completed system upgrade.
use crate::{Config, Device, Joined};
use serde::Serialize;

#[derive(Serialize)]
struct ResultAvailability<'a> {
    topic: Joined<'a>,
}
#[derive(Serialize)]
struct ResultDiscovery<'a> {
    device: Device<'a>,
    availability: [ResultAvailability<'a>; 1],
    name: &'static str,
    icon: &'static str,
    default_entity_id: Joined<'a>,
    unique_id: Joined<'a>,
    qos: u8,
    state_topic: Joined<'a>,
    value_template: &'static str,
    entity_category: &'static str,
    json_attributes_topic: Joined<'a>,
}
pub fn discovery(cfg: &Config) -> Vec<(String, String)> {
    let n = cfg.device_name.as_str();
    let state_parts = [n, "/deployment/state"];
    let state = Joined(&state_parts);
    let payload = serde_json::to_string(&ResultDiscovery {
        device: Device {
            identifiers: &cfg.device_id,
            model: n,
            name: n,
        },
        availability: [ResultAvailability {
            topic: Joined(&[n, "/state"]),
        }],
        name: "Deployment",
        icon: "mdi:package-variant",
        default_entity_id: Joined(&["sensor.", n, "_deployment"]),
        unique_id: Joined(&[n, "_deployment"]),
        qos: 1,
        state_topic: state,
        value_template: "{{ value_json.state }}",
        entity_category: "diagnostic",
        json_attributes_topic: state,
    })
    .unwrap();
    vec![
        (
            format!(
                "{}/sensor/{}/{n}_deployment/config",
                cfg.mqtt.auto_discovery_prefix, cfg.device_id
            ),
            payload,
        ),
        // Remove discovery left by the former Comin integration.
        (
            format!(
                "{}/sensor/{}/{n}_deployed_revision/config",
                cfg.mqtt.auto_discovery_prefix, cfg.device_id
            ),
            String::new(),
        ),
        (
            format!(
                "{}/binary_sensor/{}/{n}_reboot_required/config",
                cfg.mqtt.auto_discovery_prefix, cfg.device_id
            ),
            String::new(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn discovers_result_and_removes_obsolete_comin_entities() {
        let config = crate::Config::default();
        let discoveries = super::discovery(&config);
        let result: serde_json::Value = serde_json::from_str(&discoveries[0].1).unwrap();
        assert_eq!(
            result["state_topic"],
            format!("{}/deployment/state", config.device_name)
        );
        assert_eq!(result["json_attributes_topic"], result["state_topic"]);
        assert_eq!(discoveries.len(), 3);
        assert!(discoveries[1].0.contains("deployed_revision"));
        assert!(discoveries[2].0.contains("reboot_required"));
        assert!(
            discoveries[1..]
                .iter()
                .all(|(_, payload)| payload.is_empty())
        );
    }
}
