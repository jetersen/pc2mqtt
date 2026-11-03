//! Optional host audio adapter. Commands are argv, never shell input.
use std::{
    borrow::Cow,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rumqttc::{AsyncClient, Publish, QoS};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::Config;

struct Topics {
    prefix: Box<str>,
}
impl Topics {
    fn new(name: &str) -> Self {
        Self {
            prefix: format!("{name}/audio/").into_boxed_str(),
        }
    }
    fn topic(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.prefix)
    }
}

#[derive(Debug)]
struct Limit {
    maximum: f64,
    request_id: Box<str>,
    expires_at: f64,
    unmute: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitCommand<'a> {
    maximum: f64,
    #[serde(borrow)]
    request_id: Cow<'a, str>,
    expires_at: f64,
    #[serde(default)]
    unmute: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MusicCommand<'a> {
    #[serde(borrow)]
    request_id: Cow<'a, str>,
    expires_at: f64,
}

const MUSIC_VOLUME_LIMIT: f64 = 50.0;

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
    valid_limit(
        request.maximum,
        &request.request_id,
        request.expires_at,
        now,
    )
}
fn valid_limit(maximum: f64, request_id: &str, expires_at: f64, now: f64) -> bool {
    percentage(maximum)
        && !request_id.is_empty()
        && request_id.len() <= 128
        && request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && expires_at.is_finite()
        && expires_at > now
        && expires_at <= now + 60.0
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
    let topic = topic.strip_prefix(topics.prefix.as_ref())?;
    if topic == "volume/get" && payload == b"GET" {
        return Some(Work::Read);
    }
    if topic == "music/set" {
        let request: MusicCommand<'_> = serde_json::from_slice(payload).ok()?;
        if !valid_limit(
            MUSIC_VOLUME_LIMIT,
            &request.request_id,
            request.expires_at,
            now,
        ) {
            return None;
        }
        return Some(Work::Limit(Limit {
            maximum: MUSIC_VOLUME_LIMIT,
            request_id: request.request_id.into_owned().into_boxed_str(),
            expires_at: request.expires_at,
            unmute: true,
        }));
    }
    if topic == "volume/set" {
        let value = std::str::from_utf8(payload).ok()?.parse().ok()?;
        return percentage(value).then_some(Work::Set(value));
    }
    if topic == "volume/limit" {
        // Borrow ordinary JSON strings while validating. Only valid work gets
        // an owned ID; retaining the whole MQTT buffer would use more memory.
        let request: LimitCommand<'_> = serde_json::from_slice(payload).ok()?;
        if !valid_limit(
            request.maximum,
            &request.request_id,
            request.expires_at,
            now,
        ) {
            return None;
        }
        return Some(Work::Limit(Limit {
            maximum: request.maximum,
            request_id: request.request_id.into_owned().into_boxed_str(),
            expires_at: request.expires_at,
            unmute: request.unmute,
        }));
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
        // Even the smallest positive finite f64 fits in 384 decimal bytes.
        // Command copies this slice into argv; avoid a second temporary String.
        let mut buffer = [0; 384];
        let mut output = &mut buffer[..];
        std::io::Write::write_fmt(&mut output, format_args!("{value}"))
            .expect("bounded percentage argument");
        let length = 384 - output.len();
        command.arg(std::str::from_utf8(&buffer[..length]).unwrap());
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

// The ID itself lives only in the active request. A generation detects ID
// changes without copying it into the publication cache.
struct Published {
    reading: Option<Reading>,
    generation: u64,
    limit_ok: bool,
}
impl Published {
    fn matches(&self, reading: Option<Reading>, generation: u64, limit_ok: bool) -> bool {
        self.reading == reading && self.generation == generation && self.limit_ok == limit_ok
    }
}

// Validated IDs are <=128 ASCII bytes and readings are finite. Serialization
// uses stack storage; MQTT then owns one exactly sized payload allocation.
fn state_payload(state: &State<'_>) -> Vec<u8> {
    let mut buffer = [0; 384];
    let mut output = &mut buffer[..];
    serde_json::to_writer(&mut output, state).expect("bounded audio state");
    let length = 384 - output.len();
    buffer[..length].to_vec()
}

struct Worker {
    acknowledgement: Option<(Limit, bool)>,
    published: Option<Published>,
    reading: Option<Reading>,
    generation: u64,
}
impl Worker {
    fn new() -> Self {
        Self {
            acknowledgement: None,
            published: None,
            reading: None,
            generation: 0,
        }
    }
    fn clear_confirmation(&mut self) {
        if self.acknowledgement.take().is_some() {
            self.generation = self.generation.wrapping_add(1);
        }
    }
    async fn next(&self, receiver: &mut tokio::sync::mpsc::Receiver<Work>) -> Option<Work> {
        let expiry = self
            .acknowledgement
            .as_ref()
            .filter(|(_, success)| *success)
            .map(|(request, _)| {
                tokio::time::Instant::now()
                    + Duration::from_secs_f64((request.expires_at - now()).max(0.0))
            });
        tokio::select! {
            work = receiver.recv() => work,
            () = deadline(expiry) => Some(Work::Publish),
        }
    }
    async fn perform(&mut self, work: Work, program: &PathBuf, connection_epoch: &AtomicU64) {
        if matches!(work, Work::Publish) {
            return;
        }
        if matches!(work, Work::Read | Work::Connected) {
            // Explicit state requests always receive a fresh reply.
            self.published = None;
        }
        if matches!(work, Work::Connected | Work::Set(_)) {
            self.clear_confirmation();
        }
        let epoch = connection_epoch.load(Ordering::Relaxed);
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
        if epoch != connection_epoch.load(Ordering::Relaxed) {
            self.clear_confirmation();
            result = Err("MQTT reconnected during audio operation".into());
        }
        if let Err(error) = &result {
            eprintln!("Audio: {error}");
        }
        if let Work::Limit(request) = work {
            self.generation = self.generation.wrapping_add(1);
            self.acknowledgement = Some((request, result.is_ok()));
        }
        self.reading = result.ok();
    }
    async fn publish(&mut self, client: &AsyncClient, topic: &str) {
        if !rumqttc::valid_topic(topic) {
            return;
        }
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
            .is_some_and(|previous| previous.matches(self.reading, self.generation, limit_ok))
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
            .publish_bytes(topic, QoS::AtLeastOnce, true, state_payload(&state).into())
            .await
            .is_ok()
        {
            self.published = Some(Published {
                reading: self.reading,
                generation: self.generation,
                limit_ok,
            });
        }
    }
    async fn run(
        mut self,
        mut receiver: tokio::sync::mpsc::Receiver<Work>,
        program: PathBuf,
        connection_epoch: Arc<AtomicU64>,
        client: AsyncClient,
        topic: String,
    ) {
        let mut seen_epoch = 0;
        while let Some(work) = self.next(&mut receiver).await {
            if seen_epoch != connection_epoch.load(Ordering::Relaxed) {
                self.clear_confirmation();
                self.published = None;
                seen_epoch = connection_epoch.load(Ordering::Relaxed);
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
    connection_epoch: Arc<AtomicU64>,
}
impl Audio {
    pub fn start(client: AsyncClient, cfg: &Config) -> Option<Self> {
        let program = PathBuf::from(std::env::var_os("PC2MQTT_AUDIO_COMMAND")?);
        assert!(
            program.is_absolute(),
            "PC2MQTT_AUDIO_COMMAND must be absolute"
        );
        let topics = Topics::new(&cfg.device_name);
        let state_topic = topics.topic("volume/state");
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let connection_epoch = Arc::new(AtomicU64::new(0));
        tokio::spawn(Worker::new().run(
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
        self.connection_epoch.fetch_add(1, Ordering::Relaxed);
        for suffix in ["volume/set", "volume/limit", "volume/get", "music/set"] {
            client
                .try_subscribe(self.topics.topic(suffix), QoS::AtLeastOnce)
                .ok();
        }
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

    pub fn disconnected(&self) {
        self.connection_epoch.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Serialize)]
struct AudioAvailability<'a> {
    topic: crate::Joined<'a>,
    payload_available: &'static str,
    payload_not_available: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value_template: Option<&'static str>,
}
#[derive(Serialize)]
struct AudioDiscovery<'a> {
    device: crate::Device<'a>,
    name: &'static str,
    icon: &'static str,
    unique_id: crate::Joined<'a>,
    default_entity_id: crate::Joined<'a>,
    command_topic: crate::Joined<'a>,
    state_topic: crate::Joined<'a>,
    value_template: &'static str,
    json_attributes_topic: crate::Joined<'a>,
    json_attributes_template: &'static str,
    availability_mode: &'static str,
    availability: [AudioAvailability<'a>; 2],
    min: u8,
    max: u8,
    step: u8,
    unit_of_measurement: &'static str,
    mode: &'static str,
    qos: u8,
    retain: bool,
}
pub fn discovery(cfg: &Config) -> (String, String) {
    use crate::Joined;
    let n = cfg.device_name.as_str();
    let state_parts = [n, "/audio/volume/state"];
    let state = Joined(&state_parts);
    let payload = serde_json::to_string(&AudioDiscovery {
        device: crate::Device { identifiers: &cfg.device_id, model: n, name: n },
        name: "Speaker volume", icon: "mdi:volume-high",
        unique_id: Joined(&[n, "_speaker_volume"]),
        default_entity_id: Joined(&["number.", n, "_speaker_volume"]),
        command_topic: Joined(&[n, "/audio/volume/set"]), state_topic: state,
        value_template: "{{ value_json.volume }}",
        json_attributes_topic: state,
        json_attributes_template: "{{ dict(request_id=value_json.request_id, limit_ok=value_json.limit_ok, muted=value_json.muted) | tojson }}",
        availability_mode: "all",
        availability: [
            AudioAvailability { topic: Joined(&[n, "/state"]), payload_available: "online", payload_not_available: "offline", value_template: None },
            AudioAvailability { topic: state, payload_available: "online", payload_not_available: "offline", value_template: Some("{{ 'online' if value_json.available else 'offline' }}") },
        ],
        min: 0, max: 100, step: 1, unit_of_measurement: "%", mode: "box", qos: 1, retain: false,
    }).unwrap();
    (
        format!(
            "{}/number/{}/{n}_speaker_volume/config",
            cfg.mqtt.auto_discovery_prefix, cfg.device_id
        ),
        payload,
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
            parse_command(&topics.topic("volume/limit"), limit, false, &topics, 100.0),
            Some(Work::Limit(_))
        ));
        assert!(
            parse_command(&topics.topic("volume/limit"), limit, true, &topics, 100.0).is_none()
        );
        assert!(
            parse_command(&topics.topic("volume/limit"), limit, false, &topics, 110.0).is_none()
        );
        assert!(parse_command(&topics.topic("volume/limit"), limit, false, &topics, 0.0).is_none());
        for value in [b"NaN".as_slice(), b"-1", b"101", b"50; reboot"] {
            assert!(
                parse_command(&topics.topic("volume/set"), value, false, &topics, 100.0).is_none()
            );
        }
        assert!(matches!(
            parse_command(&topics.topic("volume/set"), b"25", false, &topics, 100.0),
            Some(Work::Set(25.0))
        ));
        assert!(parse_command(&topics.topic("volume/set"), b"25", true, &topics, 100.0).is_none());
    }

    #[test]
    fn music_profile_is_fixed_and_commands_reject_replays_or_overrides() {
        let topics = Topics::new("fixture");
        let payload = br#"{"request_id":"music-123","expires_at":110}"#;
        let Some(Work::Limit(request)) =
            parse_command(&topics.topic("music/set"), payload, false, &topics, 100.0)
        else {
            panic!("missing music work");
        };
        assert!((request.maximum - 50.0).abs() < f64::EPSILON);
        assert!(request.unmute);
        assert!(parse_command(&topics.topic("music/set"), payload, true, &topics, 100.0).is_none());
        assert!(
            parse_command(&topics.topic("music/set"), payload, false, &topics, 110.0).is_none()
        );
        for payload in [
            br#"{"request_id":"music-123","expires_at":110,"maximum":100}"#.as_slice(),
            br#"{"request_id":"music-123","expires_at":110,"unmute":false}"#,
            br#"{"request_id":"music-123","request_id":"other","expires_at":110}"#,
        ] {
            assert!(
                parse_command(&topics.topic("music/set"), payload, false, &topics, 100.0).is_none()
            );
        }
        assert!(matches!(
            parse_command(&topics.topic("volume/get"), b"GET", false, &topics, 100.0),
            Some(Work::Read)
        ));
        assert!(parse_command(&topics.topic("volume/get"), b"GET", true, &topics, 100.0).is_none());
        assert!(
            parse_command(&topics.topic("volume/get"), b"other", false, &topics, 100.0).is_none()
        );
        // JSON escapes still work; borrowing does not narrow the accepted syntax.
        let escaped = br#"{"request_id":"music-\u0031","expires_at":110}"#;
        let Some(Work::Limit(request)) =
            parse_command(&topics.topic("music/set"), escaped, false, &topics, 100.0)
        else {
            panic!("escaped ID rejected");
        };
        assert_eq!(&*request.request_id, "music-1");
    }

    #[test]
    fn bounded_state_encoding_fits_the_largest_valid_request_id() {
        let id = "a".repeat(128);
        let state = State {
            volume: Some(1000.0),
            muted: Some(false),
            available: true,
            request_id: Some(&id),
            limit_ok: true,
        };
        let payload = state_payload(&state);
        assert!(payload.len() <= 384);
        let state: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(state["request_id"], id);
        assert_eq!(state["volume"], 1000.0);
        assert_eq!(state["limit_ok"], true);
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
