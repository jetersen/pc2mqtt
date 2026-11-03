#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

mod audio;
mod deployment;
mod updates;

use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use std::{fmt, io, path::PathBuf, time::Duration};
use uuid::Uuid;

const GIT_SHA: &str = env!("GIT_SHA");
// Leave room for discovery, availability, and subscriptions before polling again.
const MQTT_QUEUE_CAPACITY: usize = 32;

fn config_path() -> PathBuf {
    std::env::var_os("PC2MQTT_CONFIG")
        .map_or_else(|| PathBuf::from("/etc/pc2mqtt/config.json"), PathBuf::from)
}

#[derive(Deserialize, Serialize)]
struct Config {
    device_id: String,
    device_name: String,
    mqtt: MqttConfig,
    #[serde(default)]
    updates: bool,
}

#[derive(Deserialize, Serialize)]
struct MqttConfig {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    username: String,
    password: String,
    auto_discovery_prefix: String,
}

fn default_port() -> u16 {
    1883
}

fn tls_transport(websocket: bool) -> Transport {
    let mut tls = rumqttc::TlsConfiguration::default();
    if let rumqttc::TlsConfiguration::Rustls(config) = &mut tls {
        // This client connects to one broker, not hundreds of different servers.
        std::sync::Arc::get_mut(config)
            .expect("new TLS config")
            .resumption = rumqttc::tokio_rustls::rustls::client::Resumption::in_memory_sessions(1);
    }
    if websocket {
        Transport::wss_with_config(tls)
    } else {
        Transport::tls_with_config(tls)
    }
}

impl MqttConfig {
    fn options(&self, client_id: String) -> MqttOptions {
        let Some(endpoint) = &self.url else {
            return MqttOptions::new(client_id, &self.host, self.port);
        };
        let url: http::Uri = endpoint.parse().expect("invalid MQTT URL");
        let authority = url.authority().expect("MQTT URL requires a host");
        assert!(
            !authority.as_str().contains('@'),
            "use the MQTT credential fields"
        );
        assert!(
            !endpoint.contains('#') && url.query().is_none(),
            "MQTT URL must not have a query or fragment"
        );
        let host = authority.host();
        assert!(!host.is_empty(), "MQTT URL requires a host");
        let scheme = url.scheme_str().expect("MQTT URL requires a scheme");
        let (transport, port, websocket) = match scheme.to_ascii_lowercase().as_str() {
            "mqtt" => (Transport::Tcp, 1883, false),
            "mqtts" => (tls_transport(false), 8883, false),
            "ws" => (Transport::Ws, 80, true),
            "wss" => (tls_transport(true), 443, true),
            _ => panic!("MQTT URL scheme must be mqtt, mqtts, ws, or wss"),
        };
        assert!(
            websocket || url.path().is_empty() || url.path() == "/",
            "TCP MQTT URLs must not have a path"
        );
        // Uri::port_u16() returns None for both absent and invalid ports.
        // Validate an explicit port instead of silently choosing the default.
        let port = match authority.as_str()[host.len()..].strip_prefix(':') {
            None | Some("") => port,
            Some(value) => {
                assert!(
                    value.bytes().all(|b| b.is_ascii_digit()),
                    "invalid MQTT port"
                );
                value.parse::<u16>().expect("invalid MQTT port")
            }
        };
        let mut opts = MqttOptions::new(client_id, if websocket { endpoint } else { host }, port);
        opts.set_transport(transport);
        opts
    }
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
            updates: false,
            mqtt: MqttConfig {
                url: Some("wss://YOUR-MQTT-HOST/socket".into()),
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
    cmd_shutdown: String,
    cmd_reboot: String,
    cmd_update: String,
}

impl Topics {
    fn new(cfg: &Config) -> Self {
        let n = &cfg.device_name;
        Self {
            availability: format!("{n}/state"),
            power_availability: format!("{n}/binary_sensor/availability"),
            cmd_shutdown: format!("{n}/button/shutdown/command"),
            cmd_reboot: format!("{n}/button/reboot/command"),
            cmd_update: format!("{n}/button/update/command"),
        }
    }
}

#[derive(Serialize)]
struct Device<'a> {
    identifiers: &'a str,
    model: &'a str,
    name: &'a str,
}

// Stream composed discovery strings through Serde's escaping writer, without
// allocating intermediate format! strings or hand-writing JSON escapes.
#[derive(Clone, Copy)]
struct Joined<'a>(&'a [&'a str]);
impl fmt::Display for Joined<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        for part in self.0 {
            output.write_str(part)?;
        }
        Ok(())
    }
}
impl Serialize for Joined<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
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
    default_entity_id: Joined<'a>,
    unique_id: Joined<'a>,
    qos: u8,
    // binary_sensor only
    #[serde(skip_serializing_if = "Option::is_none")]
    state_topic: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_on: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload_off: Option<&'static str>,
}

fn discovery_payloads(cfg: &Config, topics: &Topics) -> Vec<(String, String)> {
    let n = &cfg.device_name;
    let id = &cfg.device_id;
    let prefix = &cfg.mqtt.auto_discovery_prefix;
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

    let mut payloads = vec![
        (
            format!("{prefix}/binary_sensor/{id}/{n}_sensor_power/config"),
            serde_json::to_string(&DiscoveryConfig {
                device: device(),
                availability: Some(power_avail),
                command_topic: None,
                name: "Power",
                icon: "mdi:power",
                default_entity_id: Joined(&["binary_sensor.", n, "_sensor_power"]),
                unique_id: Joined(&[n, "_sensor_power"]),
                qos: 1,
                state_topic: Some(&topics.availability),
                payload_on: Some("online"),
                payload_off: Some("offline"),
            })
            .unwrap(),
        ),
        (
            format!("{prefix}/button/{id}/{n}_button_shutdown/config"),
            serde_json::to_string(&DiscoveryConfig {
                device: device(),
                availability: Some(avail),
                command_topic: Some(&topics.cmd_shutdown),
                name: "Shutdown",
                icon: "mdi:power",
                default_entity_id: Joined(&["button.", n, "_button_shutdown"]),
                unique_id: Joined(&[n, "_button_shutdown"]),
                qos: 1,
                state_topic: None,
                payload_on: None,
                payload_off: None,
            })
            .unwrap(),
        ),
        (
            format!("{prefix}/button/{id}/{n}_button_reboot/config"),
            serde_json::to_string(&DiscoveryConfig {
                device: device(),
                availability: Some(avail),
                command_topic: Some(&topics.cmd_reboot),
                name: "Reboot",
                icon: "mdi:restart",
                default_entity_id: Joined(&["button.", n, "_button_reboot"]),
                unique_id: Joined(&[n, "_button_reboot"]),
                qos: 1,
                state_topic: None,
                payload_on: None,
                payload_off: None,
            })
            .unwrap(),
        ),
    ];
    if cfg.updates {
        payloads.extend(deployment::discovery(cfg));
    }
    payloads
}

async fn systemctl(op: &str) {
    eprintln!("Executing systemctl {op}...");
    match tokio::process::Command::new("systemctl")
        .args(["--no-block", "--ignore-inhibitors", op])
        .kill_on_drop(true)
        .status()
        .await
    {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("systemctl {op} failed: {status}"),
        Err(e) => eprintln!("Failed to run systemctl {op}: {e}"),
    }
}

fn accepts_command(retained: bool, payload: &[u8]) -> bool {
    !retained && payload == b"PRESS"
}

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("pc2mqtt {GIT_SHA}");
        return;
    }
    if std::env::args().any(|argument| argument == "--request-update") {
        if let Err(error) = updates::request_local_update() {
            eprintln!("Could not queue update: {error}");
            std::process::exit(1);
        }
        return;
    }

    if std::env::args().any(|argument| argument == "--report-update-result") {
        if let Err(error) = updates::report_result() {
            eprintln!("Could not save update result: {error}");
            std::process::exit(1);
        }
        return;
    }

    println!("pc2mqtt {GIT_SHA} starting");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        // Only MQTT, the update socket, and helper pipes are active.
        .max_io_events_per_tick(64)
        .build()
        .expect("agent runtime")
        .block_on(run_mqtt(Config::load()));
}

fn mqtt_options(cfg: &Config, topics: &Topics) -> MqttOptions {
    let mut opts = cfg.mqtt.options(format!("pc2mqtt-{}", cfg.device_name));
    opts.set_credentials(&cfg.mqtt.username, &cfg.mqtt.password);
    opts.set_keep_alive(Duration::from_mins(1));
    // Bound outstanding publications without changing incoming command handling.
    opts.set_inflight(8);
    opts.set_last_will(LastWill::new(
        &topics.availability,
        "offline",
        QoS::AtLeastOnce,
        true,
    ));

    opts
}

async fn run_mqtt(cfg: Config) {
    let topics = Topics::new(&cfg);
    let opts = mqtt_options(&cfg, &topics);

    let (client, mut connection) = AsyncClient::new(opts, MQTT_QUEUE_CAPACITY);
    // A small state reply must not wait behind the command's TCP acknowledgement.
    connection.network_options.set_tcp_nodelay(true);
    let updates = cfg.updates.then(|| {
        updates::Updates::start(client.clone(), &cfg.device_name)
            .expect("could not start update handler")
    });
    let audio = audio::Audio::start(client.clone(), &cfg);
    let mut discoveries = discovery_payloads(&cfg, &topics);
    if audio.is_some() {
        discoveries.push(audio::discovery(&cfg));
    }
    let mut discoveries = Some(discoveries);
    // MQTT owns its connection settings; release the startup configuration copy.
    drop(cfg);

    loop {
        let event = connection.poll().await;
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                println!("Connected to MQTT broker");
                if let Some(updates) = &updates {
                    client
                        .try_subscribe(&topics.cmd_update, QoS::AtLeastOnce)
                        .ok();
                    updates.connected();
                }

                if let Some(discoveries) = discoveries.take() {
                    for (topic, payload) in discoveries {
                        if !rumqttc::valid_topic(&topic) {
                            continue;
                        }
                        client
                            .publish_bytes(
                                topic,
                                QoS::AtLeastOnce,
                                true,
                                payload.into_bytes().into(),
                            )
                            .await
                            .ok();
                    }
                }

                client
                    .try_publish(&topics.availability, QoS::AtLeastOnce, true, "online")
                    .ok();
                client
                    .try_publish(&topics.power_availability, QoS::AtLeastOnce, true, "online")
                    .ok();
                client
                    .try_subscribe(&topics.cmd_shutdown, QoS::AtLeastOnce)
                    .ok();
                client
                    .try_subscribe(&topics.cmd_reboot, QoS::AtLeastOnce)
                    .ok();

                if let Some(audio) = &audio {
                    audio.connected(&client);
                }
                println!("Ready to receive commands");
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                if let Some(audio) = &audio {
                    audio.handle(&p);
                }
                let accepted = accepts_command(p.retain, &p.payload);
                if let Some(updates) = &updates
                    && accepted
                    && p.topic == topics.cmd_update
                {
                    updates.request();
                } else if accepted {
                    match p.topic.as_str() {
                        t if t == topics.cmd_shutdown => {
                            tokio::spawn(systemctl("poweroff"));
                        }
                        t if t == topics.cmd_reboot => {
                            tokio::spawn(systemctl("reboot"));
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                if let Some(audio) = &audio {
                    audio.disconnected();
                }
                eprintln!("Connection error: {e} — retrying...");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_streams_composed_strings_with_json_escaping() {
        let config = Config {
            device_id: "id-\"🌍".into(),
            device_name: "host\"\\name".into(),
            updates: true,
            ..Config::default()
        };
        let mut payloads = discovery_payloads(&config, &Topics::new(&config));
        payloads.push(audio::discovery(&config));
        for (_, payload) in payloads
            .into_iter()
            .filter(|(_, payload)| !payload.is_empty())
        {
            let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(payload["device"]["identifiers"], config.device_id);
            assert_eq!(payload["device"]["name"], config.device_name);
            assert!(
                payload["unique_id"]
                    .as_str()
                    .unwrap()
                    .starts_with(&config.device_name)
            );
        }
    }

    #[test]
    fn endpoint_uris_preserve_schemes_ports_ipv6_and_websocket_paths() {
        for (endpoint, host, port) in [
            ("mqtt://localhost", "localhost", 1883),
            ("mqtts://broker.example:8884/", "broker.example", 8884),
            ("mqtt://[::1]:1884", "[::1]", 1884),
            (
                "ws://broker.example/socket",
                "ws://broker.example/socket",
                80,
            ),
            (
                "wss://mqtt.jetersen.dev/socket",
                "wss://mqtt.jetersen.dev/socket",
                443,
            ),
            (
                "wss://broker.example:9443/a%20b",
                "wss://broker.example:9443/a%20b",
                9443,
            ),
        ] {
            let mut config = Config::default();
            config.mqtt.url = Some(endpoint.into());
            assert_eq!(
                config.mqtt.options("test".into()).broker_address(),
                (host.into(), port)
            );
        }
    }

    #[test]
    fn endpoint_uris_reject_credentials_queries_fragments_and_invalid_ports() {
        for endpoint in [
            "wss://user:password@broker.example/socket",
            "mqtt://user@broker.example",
            "wss://broker.example/socket?query",
            "wss://broker.example/socket#fragment",
            "mqtt://broker.example?",
            "mqtt://broker.example/path",
            "mqtt://broker.example:65536",
            "mqtt://broker.example:invalid",
            "mqtt://broker.example:+1883",
            "mqtt://:1883",
            "mqtt:///",
            "https://broker.example/socket",
            "broker.example:1883",
        ] {
            let mut config = Config::default();
            config.mqtt.url = Some(endpoint.into());
            assert!(
                std::panic::catch_unwind(|| config.mqtt.options("test".into())).is_err(),
                "accepted {endpoint}"
            );
        }
    }

    #[test]
    fn rejects_retained_and_invalid_commands() {
        assert!(accepts_command(false, b"PRESS"));
        assert!(!accepts_command(true, b"PRESS"));
        for payload in [b"".as_slice(), b"press", b"PRESS\n", b"reboot"] {
            assert!(!accepts_command(false, payload));
        }
    }

    #[test]
    fn secure_websocket_preserves_path_and_port() {
        let mut config = Config::default();
        config.mqtt.url = Some("wss://mqtt.example.org/socket".into());
        let options = config.mqtt.options("test".into());
        assert_eq!(
            options.broker_address(),
            ("wss://mqtt.example.org/socket".into(), 443)
        );
        assert!(matches!(options.transport(), Transport::Wss(_)));
    }

    #[test]
    fn legacy_config_keeps_updates_disabled() {
        let config: Config = serde_json::from_str(
            r#"{
            "device_id":"test", "device_name":"test",
            "mqtt":{"host":"localhost", "port":1883,"username":"u",
            "password":"p","auto_discovery_prefix":"homeassistant"}
        }"#,
        )
        .unwrap();
        assert!(!config.updates);
        assert_eq!(
            config.mqtt.options("test".into()).broker_address(),
            ("localhost".into(), 1883)
        );
    }
}
