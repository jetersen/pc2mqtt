//! Home Assistant discovery for the last completed system upgrade.
pub fn discovery(cfg: &crate::Config) -> Vec<(String, String)> {
    let n = &cfg.device_name;
    let topic = format!("{n}/deployment/state");
    let mut discoveries: Vec<_> = [(
        "sensor",
        "deployment",
        "Deployment",
        "mdi:package-variant",
        "state",
    )]
    .into_iter()
    .map(|(component, key, name, icon, field)| {
        let mut payload = serde_json::json!({
            "device": {"identifiers": cfg.device_id, "model": n, "name": n},
            "availability": [{"topic": format!("{n}/state")}],
            "name": name, "icon": icon,
            "default_entity_id": format!("{component}.{n}_{key}"),
            "unique_id": format!("{n}_{key}"), "qos": 1,
            "state_topic": topic, "value_template": format!("{{{{ value_json.{field} }}}}"),
            "entity_category": "diagnostic",
        });
        payload["json_attributes_topic"] = topic.clone().into();
        (
            format!(
                "{}/{component}/{}/{n}_{key}/config",
                cfg.mqtt.auto_discovery_prefix, cfg.device_id
            ),
            payload.to_string(),
        )
    })
    .collect();
    // Remove discovery left by the former Comin integration.
    for (component, key) in [
        ("sensor", "deployed_revision"),
        ("binary_sensor", "reboot_required"),
    ] {
        discoveries.push((
            format!(
                "{}/{component}/{}/{n}_{key}/config",
                cfg.mqtt.auto_discovery_prefix, cfg.device_id
            ),
            String::new(),
        ));
    }
    discoveries
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
