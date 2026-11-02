//! Optional host audio adapter. Commands are argv, never shell input.
mod events;

use std::{
    cell::Cell,
    path::PathBuf,
    process::Stdio,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rumqttc::{AsyncClient, Publish, QoS};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;

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
    request_id: Box<str>,
    expires_at: f64,
    #[serde(default)]
    unmute: bool,
}

#[derive(Debug)]
enum Work {
    Read,
    Connected,
    Publish,
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct Reading {
    volume: f64,
    muted: bool,
}

fn parse_reading(output: &str) -> Result<Reading, &'static str> {
    let mut fields = output.split_whitespace();
    if fields.next() != Some("Volume:") {
        return Err("unexpected audio adapter output");
    }
    let value = fields.next().ok_or("missing audio volume")?;
    let raw: f64 = value.parse().map_err(|_| "invalid audio volume")?;
    if !raw.is_finite() || !(0.0..=10.0).contains(&raw) {
        return Err("audio volume out of range");
    }
    let mute = fields.next();
    let muted = mute == Some("[MUTED]");
    if (mute.is_some() && !muted) || fields.next().is_some() {
        return Err("unexpected audio mute state");
    }
    Ok(Reading {
        volume: raw * 100.0,
        muted,
    })
}

// wpctl emits one short line. Bound misbehaving adapters without growing Vecs.
const MAX_AUDIO_OUTPUT: usize = 256;

async fn execute(
    program: &PathBuf,
    operation: &str,
    value: Option<f64>,
) -> Result<Reading, String> {
    let mut command = tokio::process::Command::new(program);
    command.arg(operation);
    if let Some(value) = value {
        command.arg(value.to_string());
    }
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut child = command
            .spawn()
            .map_err(|e| format!("audio adapter failed: {e}"))?;
        let mut stdout = child.stdout.take().expect("piped audio output");
        let mut output = [0; MAX_AUDIO_OUTPUT + 1];
        let mut length = 0;
        loop {
            let read = stdout
                .read(&mut output[length..])
                .await
                .map_err(|e| format!("audio adapter output failed: {e}"))?;
            if read == 0 {
                break;
            }
            length += read;
            if length > MAX_AUDIO_OUTPUT {
                return Err("audio adapter output is too large".into());
            }
        }
        let status = child
            .wait()
            .await
            .map_err(|e| format!("audio adapter failed: {e}"))?;
        if !status.success() {
            return Err(format!("audio adapter exited with {status}"));
        }
        let text = std::str::from_utf8(&output[..length]).map_err(|_| "non-UTF8 audio output")?;
        parse_reading(text).map_err(str::to_owned)
    })
    .await
    .map_err(|_| "audio adapter timed out".to_owned())?
}

#[derive(Serialize)]
struct State<'a> {
    volume: Option<f64>,
    muted: Option<bool>,
    available: bool,
    request_id: Option<&'a str>,
    limit_ok: bool,
}

// Keep only the values needed to detect a change, not a serialized JSON copy.
struct Published {
    reading: Option<Reading>,
    request_id: Option<Box<str>>,
    limit_ok: bool,
}

impl Published {
    fn matches(&self, reading: Option<Reading>, request_id: Option<&str>, limit_ok: bool) -> bool {
        self.reading == reading
            && self.request_id.as_deref() == request_id
            && self.limit_ok == limit_ok
    }
}

struct Worker {
    acknowledgement: Option<(Limit, bool)>,
    published: Option<Published>,
    reading: Option<Reading>,
    events: events::Events,
    refresh: bool,
    retry: Option<tokio::time::Instant>,
    retry_delay: u64,
}
impl Worker {
    fn new() -> Self {
        let mut retry = None;
        let events = events::Events::open().unwrap_or_else(|error| {
            eprintln!("Audio events: {error}");
            retry = Some(tokio::time::Instant::now() + Duration::from_secs(1));
            events::Events::Disabled
        });
        Self {
            acknowledgement: None,
            published: None,
            reading: None,
            events,
            refresh: false,
            retry,
            retry_delay: 1,
        }
    }
    fn disconnected(&mut self) {
        if self.events.disconnect() {
            self.retry = Some(tokio::time::Instant::now() + Duration::from_secs(self.retry_delay));
            self.retry_delay = (self.retry_delay * 2).min(60);
        }
        self.reading = None;
    }
    async fn next(&mut self, receiver: &mut tokio::sync::mpsc::Receiver<Work>) -> Option<Work> {
        loop {
            if std::mem::take(&mut self.refresh) {
                return Some(Work::Read);
            }
            let expiry = self
                .acknowledgement
                .as_ref()
                .filter(|(_, success)| *success)
                .map(|(request, _)| {
                    tokio::time::Instant::now()
                        + Duration::from_secs_f64((request.expires_at - now()).max(0.0))
                });
            tokio::select! {
                biased;
                flags = self.events.next() => {
                    if flags & events::RESET != 0 { self.acknowledgement = None; }
                    if flags & events::DISCONNECTED != 0 {
                        self.disconnected();
                        return Some(Work::Publish);
                    }
                    if flags & events::CHANGED != 0 {
                        self.retry_delay = 1;
                        return Some(Work::Read);
                    }
                }
                work = receiver.recv() => return work,
                () = deadline(expiry) => return Some(Work::Publish),
                () = deadline(self.retry) => {
                    if let Ok(source) = events::Events::open() {
                        self.events = source; self.retry = None;
                    } else {
                        self.retry_delay = (self.retry_delay * 2).min(60);
                        self.retry = Some(tokio::time::Instant::now() + Duration::from_secs(self.retry_delay));
                    }
                }
            }
        }
    }
    async fn perform(&mut self, work: Work, program: &PathBuf, connection_epoch: &Cell<u64>) {
        if matches!(work, Work::Publish) {
            return;
        }
        if matches!(work, Work::Connected) {
            self.acknowledgement = None;
            self.published = None;
        }
        if matches!(work, Work::Set(_)) {
            self.acknowledgement = None;
        }
        let epoch = connection_epoch.get();
        let mut result = match &work {
            Work::Read | Work::Connected => execute(program, "read", None).await,
            Work::Set(value) => execute(program, "set", Some(*value)).await,
            Work::Limit(request) => {
                execute(
                    program,
                    if request.unmute {
                        "limit-unmute"
                    } else {
                        "limit"
                    },
                    Some(request.maximum),
                )
                .await
            }
            Work::Publish => unreachable!(),
        };
        // A speaker swap or reconnect during a helper invalidates its result.
        // Verify concurrent volume changes before publishing a cap confirmation.
        let mut flags = self.events.drain();
        if flags == events::CHANGED && result.is_ok() {
            result = execute(program, "read", None).await;
            flags = self.events.drain();
            if flags & events::CHANGED != 0 {
                result = Err("audio changed during confirmation".into());
                self.refresh = true;
            }
        }
        if flags & (events::RESET | events::DISCONNECTED) != 0 || epoch != connection_epoch.get() {
            self.acknowledgement = None;
            result = Err("audio session changed during operation".into());
            self.refresh = flags & events::DISCONNECTED == 0;
        }
        if flags & events::DISCONNECTED != 0 {
            self.disconnected();
        }
        if let Err(error) = &result {
            eprintln!("Audio: {error}");
        }
        if let Work::Limit(request) = work {
            self.acknowledgement = Some((request, result.is_ok()));
        }
        self.reading = result.ok();
    }
    async fn publish(&mut self, client: &AsyncClient, topic: &str) {
        // A failed, expired, or contradicted confirmation never revives.
        if let Some((request, success)) = &mut self.acknowledgement {
            *success &= limit_valid(request, now())
                && self
                    .reading
                    .is_some_and(|r| r.volume <= request.maximum && (!request.unmute || !r.muted));
        }
        let (request_id, limit_ok) = self
            .acknowledgement
            .as_ref()
            .map_or((None, false), |(request, success)| {
                (Some(request.request_id.as_ref()), *success)
            });
        if self
            .published
            .as_ref()
            .is_some_and(|previous| previous.matches(self.reading, request_id, limit_ok))
        {
            return;
        }
        let state = State {
            volume: self.reading.map(|r| r.volume),
            muted: self.reading.map(|r| r.muted),
            available: self.reading.is_some(),
            request_id,
            limit_ok,
        };
        if client
            .publish(
                topic,
                QoS::AtLeastOnce,
                true,
                serde_json::to_vec(&state).unwrap(),
            )
            .await
            .is_ok()
        {
            self.published = Some(Published {
                reading: self.reading,
                request_id: request_id.map(Into::into),
                limit_ok,
            });
        }
    }
    async fn run(
        mut self,
        mut receiver: tokio::sync::mpsc::Receiver<Work>,
        program: PathBuf,
        connection_epoch: Rc<Cell<u64>>,
        client: AsyncClient,
        topic: String,
    ) {
        let mut seen_epoch = 0;
        while let Some(work) = self.next(&mut receiver).await {
            if seen_epoch != connection_epoch.get() {
                self.acknowledgement = None;
                self.published = None;
                seen_epoch = connection_epoch.get();
            }
            if let Work::Limit(request) = &work
                && !limit_valid(request, now())
            {
                continue;
            }
            self.perform(work, &program, &connection_epoch).await;
            self.publish(&client, &topic).await;
        }
    }
}
async fn deadline(deadline: Option<tokio::time::Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

pub struct Audio {
    topics: Topics,
    work: tokio::sync::mpsc::Sender<Work>,
    connection_epoch: Rc<Cell<u64>>,
}
impl Audio {
    pub fn start(client: AsyncClient, cfg: &Config) -> Option<Self> {
        let program = PathBuf::from(std::env::var_os("PC2MQTT_AUDIO_COMMAND")?);
        assert!(
            program.is_absolute(),
            "PC2MQTT_AUDIO_COMMAND must be absolute"
        );
        assert!(
            cfg!(feature = "pipewire")
                || std::env::var_os("PC2MQTT_AUDIO_SOCKET").is_none()
                || std::env::var_os("PC2MQTT_AUDIO_EVENT_SOCKET").is_some(),
            "PC2MQTT_AUDIO_SOCKET requires a build with --features pipewire"
        );
        let topics = Topics::new(&cfg.device_name);
        let state_topic = topics.state.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let connection_epoch = Rc::new(Cell::new(0));
        tokio::task::spawn_local(Worker::new().run(
            receiver,
            program,
            connection_epoch.clone(),
            client,
            state_topic,
        ));
        Some(Self {
            topics,
            work: sender,
            connection_epoch,
        })
    }

    pub fn connected(&self, client: &AsyncClient) {
        self.connection_epoch
            .set(self.connection_epoch.get().wrapping_add(1));
        client
            .try_subscribe(&self.topics.set, QoS::AtLeastOnce)
            .ok();
        client
            .try_subscribe(&self.topics.limit, QoS::AtLeastOnce)
            .ok();
        self.work.try_send(Work::Connected).ok();
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
