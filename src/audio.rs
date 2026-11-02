//! Optional host audio adapter. Commands are argv, never shell input.
use std::{
    path::PathBuf,
    process::Command,
    sync::mpsc::{self, SyncSender},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rumqttc::{Client, Publish, QoS};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Config;

#[derive(Clone)]
struct Topics {
    state: String,
    set: String,
    limit: String,
}
impl Topics {
    fn new(name: &str) -> Self {
        Self {
            state: format!("{name}/audio/volume/state"),
            set: format!("{name}/audio/volume/set"),
            limit: format!("{name}/audio/volume/limit"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limit {
    maximum: f64,
    request_id: String,
    expires_at: f64,
    #[serde(default)]
    unmute: bool,
}

#[derive(Debug)]
enum Work {
    Read,
    Set(f64),
    Limit(Limit),
}

fn percentage(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

fn limit_valid(request: &Limit, now: f64) -> bool {
    percentage(request.maximum)
        && !request.request_id.is_empty()
        && request.request_id.len() <= 128
        && request
            .request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && request.expires_at.is_finite()
        && request.expires_at > now
        && request.expires_at <= now + 60.0
}

fn parse_command(
    topic: &str,
    payload: &[u8],
    retained: bool,
    topics: &Topics,
    now: f64,
) -> Option<Work> {
    if retained {
        return None;
    }
    if topic == topics.set {
        let value = std::str::from_utf8(payload).ok()?.parse().ok()?;
        return percentage(value).then_some(Work::Set(value));
    }
    if topic == topics.limit {
        let request: Limit = serde_json::from_slice(payload).ok()?;
        return limit_valid(&request, now).then_some(Work::Limit(request));
    }
    None
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Reading {
    volume: f64,
    muted: bool,
}

fn parse_reading(output: &str) -> Result<Reading, &'static str> {
    let fields: Vec<_> = output.split_whitespace().collect();
    if fields.first() != Some(&"Volume:") || !(2..=3).contains(&fields.len()) {
        return Err("unexpected audio adapter output");
    }
    let raw: f64 = fields[1].parse().map_err(|_| "invalid audio volume")?;
    if !raw.is_finite() || !(0.0..=10.0).contains(&raw) {
        return Err("audio volume out of range");
    }
    let muted = fields.get(2) == Some(&"[MUTED]");
    if fields.len() == 3 && !muted {
        return Err("unexpected audio mute state");
    }
    Ok(Reading {
        volume: raw * 100.0,
        muted,
    })
}

fn execute(program: &PathBuf, operation: &str, value: Option<f64>) -> Result<Reading, String> {
    let mut command = Command::new(program);
    command.arg(operation);
    if let Some(value) = value {
        command.arg(value.to_string());
    }
    let output = command
        .output()
        .map_err(|e| format!("audio adapter failed: {e}"))?;
    if !output.status.success() {
        return Err(format!("audio adapter exited with {}", output.status));
    }
    let output = std::str::from_utf8(&output.stdout).map_err(|_| "non-UTF8 audio output")?;
    parse_reading(output).map_err(str::to_owned)
}

pub struct Audio {
    topics: Topics,
    work: SyncSender<Work>,
}
impl Audio {
    pub fn start(client: Client, cfg: &Config) -> Option<Self> {
        let program = PathBuf::from(std::env::var_os("PC2MQTT_AUDIO_COMMAND")?);
        assert!(
            program.is_absolute(),
            "PC2MQTT_AUDIO_COMMAND must be absolute"
        );
        let topics = Topics::new(&cfg.device_name);
        let worker_topics = topics.clone();
        let (sender, receiver) = mpsc::sync_channel(8);
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                let mut acknowledgement: Option<(String, bool)> = None;
                loop {
                    let work = match receiver.recv_timeout(Duration::from_secs(5)) {
                        Ok(work) => work,
                        Err(mpsc::RecvTimeoutError::Timeout) => Work::Read,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    };
                    if let Work::Limit(request) = &work
                        && !limit_valid(request, now())
                    {
                        continue;
                    }
                    let result = match &work {
                        Work::Read => execute(&program, "read", None),
                        Work::Set(value) => execute(&program, "set", Some(*value)),
                        Work::Limit(request) => execute(
                            &program,
                            if request.unmute {
                                "limit-unmute"
                            } else {
                                "limit"
                            },
                            Some(request.maximum),
                        ),
                    };
                    if let Work::Limit(request) = &work {
                        let success = result.as_ref().is_ok_and(|r| {
                            r.volume <= request.maximum && (!request.unmute || !r.muted)
                        });
                        acknowledgement = Some((request.request_id.clone(), success));
                    }
                    if let Err(error) = &result {
                        eprintln!("Audio: {error}");
                    }
                    let (request_id, limit_ok) = acknowledgement
                        .as_ref()
                        .map_or((None, false), |(id, ok)| (Some(id), *ok));
                    let reading = result.ok();
                    let state = json!({
                        "volume": reading.map(|r| r.volume),
                        "muted": reading.map(|r| r.muted),
                        "available": reading.is_some(),
                        "request_id": request_id,
                        "limit_ok": limit_ok,
                    });
                    client
                        .publish(
                            &worker_topics.state,
                            QoS::AtLeastOnce,
                            true,
                            state.to_string(),
                        )
                        .ok();
                }
            })
            .expect("could not start audio handler");
        Some(Self {
            topics,
            work: sender,
        })
    }

    pub fn connected(&self, client: &Client) {
        client.subscribe(&self.topics.set, QoS::AtLeastOnce).ok();
        client.subscribe(&self.topics.limit, QoS::AtLeastOnce).ok();
        self.work.try_send(Work::Read).ok();
    }

    pub fn handle(&self, publish: &Publish) {
        if let Some(work) = parse_command(
            &publish.topic,
            &publish.payload,
            publish.retain,
            &self.topics,
            now(),
        ) {
            self.work.try_send(work).ok();
        }
    }
}

pub fn discovery(cfg: &Config) -> (String, String) {
    let n = &cfg.device_name;
    let topics = Topics::new(n);
    let payload = json!({
        "device": {"identifiers": cfg.device_id, "model": n, "name": n},
        "name": "Speaker volume", "icon": "mdi:volume-high",
        "unique_id": format!("{n}_speaker_volume"),
        "default_entity_id": format!("number.{n}_speaker_volume"),
        "command_topic": topics.set, "state_topic": topics.state,
        "value_template": "{{ value_json.volume }}",
        "json_attributes_topic": topics.state,
        "json_attributes_template": "{{ dict(request_id=value_json.request_id, limit_ok=value_json.limit_ok, muted=value_json.muted) | tojson }}",
        "availability_mode": "all",
        "availability": [
            {"topic": format!("{n}/state"), "payload_available": "online", "payload_not_available": "offline"},
            {"topic": topics.state, "value_template": "{{ 'online' if value_json.available else 'offline' }}", "payload_available": "online", "payload_not_available": "offline"}
        ],
        "min": 0, "max": 100, "step": 1, "unit_of_measurement": "%", "mode": "box", "qos": 1, "retain": false,
    });
    (
        format!(
            "{}/number/{id}/{n}_speaker_volume/config",
            cfg.mqtt.auto_discovery_prefix,
            id = cfg.device_id
        ),
        payload.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_volume_and_mute_without_hiding_amplification() {
        let reading = parse_reading("Volume: 0.50 [MUTED]\n").unwrap();
        assert!((reading.volume - 50.0).abs() < f64::EPSILON);
        assert!(reading.muted);
        assert!((parse_reading("Volume: 1.25").unwrap().volume - 125.0).abs() < f64::EPSILON);
        for invalid in [
            "",
            "Volume: NaN",
            "Volume: -0.5",
            "Volume: 0.5 garbage",
            "Other: 0.5",
        ] {
            assert!(parse_reading(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn commands_reject_replays_expired_requests_and_invalid_values() {
        let topics = Topics::new("fixture");
        let limit = br#"{"maximum":50,"request_id":"cap-123","expires_at":110}"#;
        assert!(matches!(
            parse_command(&topics.limit, limit, false, &topics, 100.0),
            Some(Work::Limit(_))
        ));
        assert!(parse_command(&topics.limit, limit, true, &topics, 100.0).is_none());
        assert!(parse_command(&topics.limit, limit, false, &topics, 110.0).is_none());
        assert!(parse_command(&topics.limit, limit, false, &topics, 0.0).is_none());
        for value in [b"NaN".as_slice(), b"-1", b"101", b"50; reboot"] {
            assert!(parse_command(&topics.set, value, false, &topics, 100.0).is_none());
        }
        assert!(matches!(
            parse_command(&topics.set, b"25", false, &topics, 100.0),
            Some(Work::Set(25.0))
        ));
        assert!(parse_command(&topics.set, b"25", true, &topics, 100.0).is_none());
    }

    #[test]
    fn discovery_exposes_state_and_correlated_limit_confirmation() {
        let cfg = Config::default();
        let (topic, payload) = discovery(&cfg);
        assert!(topic.contains("/number/"));
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["retain"], false);
        assert_eq!(payload["availability_mode"], "all");
        assert!(
            payload["json_attributes_template"]
                .as_str()
                .unwrap()
                .contains("request_id")
        );
    }
}
