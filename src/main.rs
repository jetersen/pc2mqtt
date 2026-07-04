#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use rumqttc::{Client, Event, LastWill, MqttOptions, Packet, QoS};
use serde::{Deserialize, Serialize};
use std::{io, path::PathBuf, process::Command, time::Duration};
use uuid::Uuid;

const GIT_SHA: &str = env!("GIT_SHA");
// Max connect burst: 3 discovery publishes + 2 availability publishes + 2 subscribes.
const MQTT_QUEUE_CAPACITY: usize = 8;

fn config_path() -> PathBuf {
    std::env::var_os("PC2MQTT_CONFIG")
        .map_or_else(|| PathBuf::from("/etc/pc2mqtt/config.json"), PathBuf::from)
}

#[derive(Deserialize, Serialize)]
struct Config {
    device_id: String,
    device_name: String,
    mqtt: MqttConfig,
}

#[derive(Deserialize, Serialize)]
struct MqttConfig {
    host: String,
    port: u16,
    username: String,
    password: String,
    auto_discovery_prefix: String,
}

impl Config {
    fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).expect("failed to parse config.json"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let config = Self::default();
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).expect("failed to create config directory");
                }
                std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap())
                    .expect("failed to write config.json");
                eprintln!(
                    "Created default {} — please fill in your MQTT credentials.",
                    path.display()
                );
                std::process::exit(1);
            }
            Err(e) => panic!("failed to read {}: {e}", path.display()),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let hostname = hostname();
        Self {
            device_id: Uuid::new_v4().to_string(),
            device_name: hostname.to_lowercase(),
            mqtt: MqttConfig {
                host: "YOUR MQTT HOST".into(),
                port: 1883,
                username: "MQTT USER".into(),
                password: "MQTT PASSWORD".into(),
                auto_discovery_prefix: "homeassistant".into(),
            },
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "DEFAULT_SYSTEM_NAME".into())
        .trim()
        .to_string()
}

struct Topics {
    availability: String,
    power_availability: String,
    discovery_power: String,
    discovery_shutdown: String,
    discovery_reboot: String,
    cmd_shutdown: String,
    cmd_reboot: String,
}

impl Topics {
    fn new(cfg: &Config) -> Self {
        let n = &cfg.device_name;
        let id = &cfg.device_id;
        let prefix = &cfg.mqtt.auto_discovery_prefix;
        Self {
            availability: format!("{n}/state"),
            power_availability: format!("{n}/binary_sensor/availability"),
            discovery_power: format!("{prefix}/binary_sensor/{id}/{n}_sensor_power/config"),
            discovery_shutdown: format!("{prefix}/button/{id}/{n}_button_shutdown/config"),
            discovery_reboot: format!("{prefix}/button/{id}/{n}_button_reboot/config"),
            cmd_shutdown: format!("{n}/button/shutdown/command"),
            cmd_reboot: format!("{n}/button/reboot/command"),
        }
    }
}

#[derive(Serialize)]
struct Device<'a> {
    identifiers: &'a str,
    model: &'a str,
    name: &'a str,
}

#[derive(Serialize, Clone, Copy)]
struct Availability<'a> {
    topic: &'a str,
    payload_available: &'static str,
    payload_not_available: &'static str,
}

#[derive(Serialize)]
struct DiscoveryConfig<'a> {
    device: Device<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    availability: Option<Availability<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_topic: Option<&'a str>,
    name: &'a str,
    icon: &'static str,
    default_entity_id: String,
    unique_id: String,
    qos: u8,
    // binary_sensor only
    #[serde(skip_serializing_if = "Option::is_none")]
    state_topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_on: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_off: Option<&'static str>,
}

fn discovery_payloads(cfg: &Config, topics: &Topics) -> Vec<(String, String)> {
    let n = &cfg.device_name;
    let id = &cfg.device_id;
    let avail = Availability {
        topic: &topics.availability,
        payload_available: "online",
        payload_not_available: "offline",
    };
    let power_avail = Availability {
        topic: &topics.power_availability,
        payload_available: "online",
        payload_not_available: "offline",
    };
    let device = || Device {
        identifiers: id,
        model: n,
        name: n,
    };

    vec![
        (
            topics.discovery_power.clone(),
            serde_json::to_string(&DiscoveryConfig {
                device: device(),
                availability: Some(power_avail),
                command_topic: None,
                name: "Power",
                icon: "mdi:power",
                default_entity_id: format!("binary_sensor.{n}_sensor_power"),
                unique_id: format!("{n}_sensor_power"),
                qos: 1,
                state_topic: Some(topics.availability.clone()),
                payload_on: Some("online"),
                payload_off: Some("offline"),
            })
            .unwrap(),
        ),
        (
            topics.discovery_shutdown.clone(),
            serde_json::to_string(&DiscoveryConfig {
                device: device(),
                availability: Some(avail),
                command_topic: Some(&topics.cmd_shutdown),
                name: "Shutdown",
                icon: "mdi:power",
                default_entity_id: format!("button.{n}_button_shutdown"),
                unique_id: format!("{n}_button_shutdown"),
                qos: 1,
                state_topic: None,
                payload_on: None,
                payload_off: None,
            })
            .unwrap(),
        ),
        (
            topics.discovery_reboot.clone(),
            serde_json::to_string(&DiscoveryConfig {
                device: device(),
                availability: Some(avail),
                command_topic: Some(&topics.cmd_reboot),
                name: "Reboot",
                icon: "mdi:restart",
                default_entity_id: format!("button.{n}_button_reboot"),
                unique_id: format!("{n}_button_reboot"),
                qos: 1,
                state_topic: None,
                payload_on: None,
                payload_off: None,
            })
            .unwrap(),
        ),
    ]
}

fn systemctl(op: &str) {
    eprintln!("Executing systemctl {op}...");
    match Command::new("systemctl")
        .args(["--no-block", "--ignore-inhibitors", op])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("systemctl {op} failed: {status}"),
        Err(e) => eprintln!("Failed to run systemctl {op}: {e}"),
    }
}

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("pc2mqtt {GIT_SHA}");
        return;
    }

    println!("pc2mqtt {GIT_SHA} starting");

    let cfg = Config::load();
    let topics = Topics::new(&cfg);

    let mut opts = MqttOptions::new(
        format!("pc2mqtt-{}", cfg.device_name),
        &cfg.mqtt.host,
        cfg.mqtt.port,
    );
    opts.set_credentials(&cfg.mqtt.username, &cfg.mqtt.password);
    opts.set_keep_alive(Duration::from_mins(1));
    opts.set_last_will(LastWill::new(
        &topics.availability,
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    opts.set_clean_session(true);

    let (client, mut connection) = Client::new(opts, MQTT_QUEUE_CAPACITY);
    let discoveries = discovery_payloads(&cfg, &topics);
    let mut initial_connect_done = false;

    for event in connection.iter() {
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                println!("Connected to {}:{}", cfg.mqtt.host, cfg.mqtt.port);

                if !initial_connect_done {
                    for (topic, payload) in &discoveries {
                        client
                            .publish(topic, QoS::AtLeastOnce, true, payload.as_bytes())
                            .ok();
                    }
                    initial_connect_done = true;
                }

                client
                    .publish(&topics.availability, QoS::AtLeastOnce, true, "online")
                    .ok();
                client
                    .publish(&topics.power_availability, QoS::AtLeastOnce, true, "online")
                    .ok();
                client
                    .subscribe(&topics.cmd_shutdown, QoS::AtLeastOnce)
                    .ok();
                client.subscribe(&topics.cmd_reboot, QoS::AtLeastOnce).ok();

                println!("Ready to receive commands");
            }
            Ok(Event::Incoming(Packet::Publish(p))) => match p.topic.as_str() {
                t if t == topics.cmd_shutdown => systemctl("poweroff"),
                t if t == topics.cmd_reboot => systemctl("reboot"),
                _ => {}
            },
            Err(e) => eprintln!("Connection error: {e} — retrying..."),
            _ => {}
        }
    }
}
