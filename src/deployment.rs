//! Home Assistant deployment telemetry derived from Comin snapshots.
use pc2mqtt::comin;
use serde::Serialize;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub state: &'static str,
    pub deployed_revision: Option<String>,
    pub booted_revision: Option<String>,
    pub reboot_required: Option<bool>,
}

impl Status {
    pub fn unavailable() -> Self {
        Self {
            state: "unavailable",
            ..Self::default()
        }
    }

    pub fn from_state(state: &comin::State) -> Self {
        let mut result = Self::unavailable();
        let (Some(suspended), Some(fetcher), Some(builder), Some(deployer)) = (
            state.is_suspended,
            &state.fetcher,
            &state.builder,
            &state.deployer,
        ) else {
            return result;
        };
        let flags = [
            fetcher.is_fetching,
            builder.is_evaluating,
            builder.is_building,
            deployer.is_deploying,
        ];
        if flags.iter().any(Option::is_none) {
            return result;
        }
        result.state = if suspended
            || builder.is_suspended == Some(true)
            || deployer.is_suspended == Some(true)
        {
            "suspended"
        } else if flags.contains(&Some(true)) {
            "updating"
        } else if builder
            .generation
            .as_ref()
            .is_some_and(|g| g.eval_status == "failed" || g.build_status == "failed")
            || deployer
                .deployment
                .as_ref()
                .is_some_and(|d| d.status == "failed")
            || fetcher.repository_status.as_ref().is_some_and(|r| {
                !r.error_msg.is_empty()
                    || r.remotes
                        .iter()
                        .any(|remote| !remote.fetch_error_msg.is_empty())
            })
        {
            "failed"
        } else {
            "idle"
        };
        result.reboot_required = state.need_to_reboot;
        if let Some(store) = &state.store {
            let revision = |id: &str| {
                store
                    .deployments
                    .iter()
                    .find(|deployment| !id.is_empty() && deployment.uuid == id)
                    .and_then(|deployment| deployment.generation.as_ref())
                    .map(|generation| generation.selected_commit_id.clone())
                    .filter(|revision| !revision.is_empty())
            };
            // The builder's selected commit may not have deployed successfully.
            // Comin's store resolves the actual switched and booted systems.
            result.deployed_revision = revision(&store.deployment_switched);
            result.booted_revision = revision(&store.deployment_booted);
        }
        result
    }
}

pub fn discovery(cfg: &crate::Config) -> Vec<(String, String)> {
    let n = &cfg.device_name;
    let topic = format!("{n}/deployment/state");
    [
        ("sensor", "deployment", "Deployment", "mdi:package-variant", "state"),
        ("sensor", "deployed_revision", "Deployed revision", "mdi:source-commit", "deployed_revision"),
        ("binary_sensor", "reboot_required", "Reboot required", "mdi:restart-alert", "reboot_required"),
    ].into_iter().map(|(component, key, name, icon, field)| {
        let mut payload = serde_json::json!({
            "device": {"identifiers": cfg.device_id, "model": n, "name": n},
            "availability": [{"topic": format!("{n}/state")}],
            "name": name, "icon": icon,
            "default_entity_id": format!("{component}.{n}_{key}"),
            "unique_id": format!("{n}_{key}"), "qos": 1,
            "state_topic": topic, "value_template": format!("{{{{ value_json.{field} }}}}"),
            "entity_category": "diagnostic",
        });
        if key == "deployment" {
            payload["json_attributes_topic"] = topic.clone().into();
        } else {
            payload["availability"].as_array_mut().unwrap().push(serde_json::json!({
                "topic": topic,
                "value_template": format!("{{{{ 'online' if value_json.state != 'unavailable' and value_json.{field} is not none else 'offline' }}}}"),
            }));
            payload["availability_mode"] = "all".into();
        }
        if component == "binary_sensor" {
            payload["payload_on"] = "True".into();
            payload["payload_off"] = "False".into();
            payload["device_class"] = "problem".into();
        }
        (format!("{}/{component}/{}/{n}_{key}/config", cfg.mqtt.auto_discovery_prefix, cfg.device_id), payload.to_string())
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_actual_switched_and_booted_revisions_and_state() {
        let mut state = comin::State {
            is_suspended: Some(false),
            need_to_reboot: Some(true),
            fetcher: Some(comin::Fetcher {
                is_fetching: Some(false),
                ..Default::default()
            }),
            builder: Some(comin::Builder {
                is_evaluating: Some(false),
                is_building: Some(false),
                generation: Some(comin::Generation {
                    selected_commit_id: "unbuilt".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            deployer: Some(comin::Deployer {
                is_deploying: Some(false),
                ..Default::default()
            }),
            store: Some(comin::Store {
                deployment_switched: "new".into(),
                deployment_booted: "old".into(),
                deployments: [("new", "deployed"), ("old", "booted")]
                    .into_iter()
                    .map(|(id, revision)| comin::Deployment {
                        uuid: id.into(),
                        generation: Some(comin::Generation {
                            selected_commit_id: revision.into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                    .collect(),
            }),
        };
        let status = Status::from_state(&state);
        assert_eq!(status.state, "idle");
        assert_eq!(status.deployed_revision.as_deref(), Some("deployed"));
        assert_eq!(status.booted_revision.as_deref(), Some("booted"));
        assert_eq!(status.reboot_required, Some(true));
        state
            .builder
            .as_mut()
            .unwrap()
            .generation
            .as_mut()
            .unwrap()
            .build_status = "failed".into();
        assert_eq!(Status::from_state(&state).state, "failed");
        state.fetcher.as_mut().unwrap().is_fetching = Some(true);
        assert_eq!(Status::from_state(&state).state, "updating");
        state.is_suspended = Some(true);
        assert_eq!(Status::from_state(&state).state, "suspended");
        assert_eq!(
            Status::from_state(&comin::State::default()).state,
            "unavailable"
        );
    }

    #[test]
    fn discovery_uses_retained_json_state_and_host_availability() {
        let config = crate::Config {
            updates: true,
            ..Default::default()
        };
        let discoveries = discovery(&config);
        assert_eq!(discoveries.len(), 3);
        for (topic, payload) in discoveries {
            let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert!(topic.ends_with("/config"));
            assert_eq!(
                payload["state_topic"],
                format!("{}/deployment/state", config.device_name)
            );
            assert_eq!(
                payload["availability"][0]["topic"],
                format!("{}/state", config.device_name)
            );
        }
    }
}
