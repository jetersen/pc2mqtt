use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

const TOPIC: &str = "fixture/button/update/command";

fn binary() -> std::ffi::OsString {
    std::env::var_os("PC2MQTT_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_pc2mqtt").into())
}
fn shell() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("sh"))
        .find(|p| p.is_file())
        .unwrap()
}
fn executable(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, format!("#!{}\n{body}\n", shell().display())).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
struct App {
    directory: PathBuf,
    child: Child,
    audio: Option<PathBuf>,
    quiet: bool,
}
impl App {
    fn start(port: u16) -> Self {
        Self::start_with_audio(port, None)
    }
    fn start_with_audio(port: u16, audio: Option<&std::path::Path>) -> Self {
        Self::start_with_output(port, audio, false)
    }
    fn start_with_output(port: u16, audio: Option<&std::path::Path>, quiet: bool) -> Self {
        let directory = std::env::temp_dir().join(format!("pc2mqtt-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("config.json"),
            serde_json::json!({
                "device_id":"fixture", "device_name":"fixture", "updates":true,
                "mqtt":{"host":"127.0.0.1","port":port,"username":"fixture",
                    "password":"fixture","auto_discovery_prefix":"homeassistant"}
            })
            .to_string(),
        )
        .unwrap();
        executable(
            &directory.join("updater"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'/requests",
                directory.display()
            ),
        );
        let child = Self::spawn(&directory, audio, quiet);
        Self {
            directory,
            child,
            audio: audio.map(std::path::Path::to_path_buf),
            quiet,
        }
    }
    fn spawn(directory: &std::path::Path, audio: Option<&std::path::Path>, quiet: bool) -> Child {
        let mut command = Command::new(binary());
        command.env_remove("PC2MQTT_AUDIO_COMMAND");
        command.env_remove("PC2MQTT_AUDIO_SOCKET");
        command.env_remove("PC2MQTT_AUDIO_EVENT_SOCKET");
        if let Some(audio) = audio {
            command.env("PC2MQTT_AUDIO_COMMAND", audio);
            command.env(
                "PC2MQTT_AUDIO_EVENT_SOCKET",
                directory.join("audio-events.sock"),
            );
            if quiet && let Some(socket) = std::env::var_os("PC2MQTT_BENCH_PIPEWIRE_SOCKET") {
                command.env_remove("PC2MQTT_AUDIO_EVENT_SOCKET");
                command.env("PC2MQTT_AUDIO_SOCKET", socket);
            }
        }
        if quiet {
            command.stderr(Stdio::null());
        }
        command
            .env("PC2MQTT_CONFIG", directory.join("config.json"))
            .env("PC2MQTT_UPDATE_SOCKET", directory.join("update.sock"))
            .env("PC2MQTT_UPDATE_RESULT", directory.join("result.json"))
            .env("PC2MQTT_UPDATE_COMMAND", directory.join("updater"))
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    }
    fn stop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        fs::remove_file(self.directory.join("audio-events.sock")).ok();
    }
    fn restart(&mut self) {
        self.stop();
        self.child = Self::spawn(&self.directory, self.audio.as_deref(), self.quiet);
    }
    fn audio_changed(&self, event: u8) {
        std::os::unix::net::UnixDatagram::unbound()
            .unwrap()
            .send_to(&[event], self.directory.join("audio-events.sock"))
            .unwrap();
    }
    fn report(&self, result: &str, status: &str) {
        assert!(
            Command::new(binary())
                .arg("--report-update-result")
                .env("PC2MQTT_UPDATE_SOCKET", self.directory.join("update.sock"))
                .env("PC2MQTT_UPDATE_RESULT", self.directory.join("result.json"))
                .env("SERVICE_RESULT", result)
                .env("EXIT_STATUS", status)
                .status()
                .unwrap()
                .success()
        );
    }
    fn requests(&self) -> usize {
        fs::read_to_string(self.directory.join("requests"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn wait_for_requests(&self, minimum: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.requests() < minimum {
            assert!(Instant::now() < deadline, "upgrade was not requested");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for App {
    fn drop(&mut self) {
        self.stop();
        fs::remove_dir_all(&self.directory).ok();
    }
}
struct Broker(TcpStream, Vec<(String, Vec<u8>, bool)>);

impl Broker {
    fn accept(listener: &TcpListener, session_present: bool) -> Self {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client did not connect");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        let mut broker = Self(stream, Vec::new());
        let (header, connect) = broker.read(Duration::from_secs(3)).unwrap();
        assert_eq!(header >> 4, 1);
        assert_eq!(connect[7] & 2, 2, "commands must use a clean session");
        broker
            .0
            .write_all(&[0x20, 2, u8::from(session_present), 0])
            .unwrap();
        broker
    }

    fn read(&mut self, timeout: Duration) -> Option<(u8, Vec<u8>)> {
        self.0.set_read_timeout(Some(timeout)).unwrap();
        let mut header = [0];
        if let Err(error) = self.0.read_exact(&mut header) {
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) {
                return None;
            }
            panic!("read failed: {error}");
        }
        let mut length = 0;
        let mut multiplier = 1;
        loop {
            let mut byte = [0];
            self.0.read_exact(&mut byte).unwrap();
            length += usize::from(byte[0] & 127) * multiplier;
            if byte[0] & 128 == 0 {
                break;
            }
            multiplier *= 128;
        }
        let mut body = vec![0; length];
        self.0.read_exact(&mut body).unwrap();
        Some((header[0], body))
    }

    fn respond(&mut self, header: u8, body: &[u8]) {
        if header >> 4 == 3 {
            let length = usize::from(u16::from_be_bytes([body[0], body[1]]));
            let offset = 2 + length + if header & 6 == 0 { 0 } else { 2 };
            self.1.push((
                String::from_utf8(body[2..2 + length].to_vec()).unwrap(),
                body[offset..].to_vec(),
                header & 1 != 0,
            ));
        }
        match header >> 4 {
            3 if header & 6 == 2 => {
                let topic_length = usize::from(u16::from_be_bytes([body[0], body[1]]));
                let id = &body[2 + topic_length..4 + topic_length];
                self.0.write_all(&[0x40, 2, id[0], id[1]]).unwrap();
            }
            8 => self.0.write_all(&[0x90, 3, body[0], body[1], 1]).unwrap(),
            12 => self.0.write_all(&[0xd0, 0]).unwrap(),
            _ => {}
        }
    }

    fn wait_for_subscription(&mut self) {
        self.wait_for_topic(TOPIC);
    }

    fn wait_for_topic(&mut self, topic: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let (header, body) = self.read(Duration::from_millis(100)).unwrap_or_default();
            self.respond(header, &body);
            if header >> 4 == 8 && body[4..body.len() - 1] == *topic.as_bytes() {
                return;
            }
        }
        panic!("update topic was not subscribed");
    }

    fn wait_for_status(&mut self, expected: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(index) = self.1.iter().position(|(topic, payload, _)| {
                topic == "fixture/deployment/state"
                    && serde_json::from_slice::<serde_json::Value>(payload).unwrap()["state"]
                        == expected
            }) {
                let (_, payload, retained) = self.1.remove(index);
                assert!(retained);
                return serde_json::from_slice(&payload).unwrap();
            }
            assert!(Instant::now() < deadline, "no {expected} deployment status");
            if let Some((header, body)) = self.read(Duration::from_millis(100)) {
                self.respond(header, &body);
            }
        }
    }

    fn publish(&mut self, id: u16, retained: bool, duplicate: bool) {
        self.publish_topic(id, retained, duplicate, TOPIC, b"PRESS");
    }

    fn publish_topic(
        &mut self,
        id: u16,
        retained: bool,
        duplicate: bool,
        topic: &str,
        payload: &[u8],
    ) {
        let topic_length = u16::try_from(topic.len()).unwrap();
        let mut body = Vec::from(topic_length.to_be_bytes());
        body.extend_from_slice(topic.as_bytes());
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(payload);
        let header = 0x32 | u8::from(retained) | (u8::from(duplicate) << 3);
        let mut length = body.len();
        let mut packet = vec![header];
        loop {
            let mut byte = u8::try_from(length % 128).unwrap();
            length /= 128;
            if length > 0 {
                byte |= 128;
            }
            packet.push(byte);
            if length == 0 {
                break;
            }
        }
        packet.extend_from_slice(&body);
        self.0.write_all(&packet).unwrap();
    }

    fn wait_for_audio(&mut self, request_id: &str) -> serde_json::Value {
        self.wait_for_audio_id(Some(request_id))
    }

    fn wait_for_audio_id(&mut self, request_id: Option<&str>) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(index) = self.1.iter().position(|(topic, payload, _)| {
                topic == "fixture/audio/volume/state"
                    && serde_json::from_slice::<serde_json::Value>(payload).unwrap()["request_id"]
                        .as_str()
                        == request_id
            }) {
                let (_, payload, _) = self.1.remove(index);
                return serde_json::from_slice(&payload).unwrap();
            }
            assert!(Instant::now() < deadline, "missing audio acknowledgement");
            if let Some((header, body)) = self.read(Duration::from_millis(100)) {
                self.respond(header, &body);
            }
        }
    }

    fn acknowledged(&mut self, id: u16, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some((header, body)) =
                self.read(deadline.saturating_duration_since(Instant::now()))
            {
                self.respond(header, &body);
                if header >> 4 == 4 && body == id.to_be_bytes() {
                    return true;
                }
            }
        }
        false
    }
}

#[test]
fn audio_caps_are_correlated_and_unmute_is_confirmed() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!("pc2mqtt-audio-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&directory).unwrap();
    let program = directory.join("adapter");
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("sh"))
        .find(|path| path.is_file())
        .expect("POSIX shell on PATH");
    fs::write(&program, format!("#!{}\ncase \"$1\" in\nread) echo 'Volume: 0.75 [MUTED]' ;;\nlimit-unmute) test \"$2\" = 50 && echo 'Volume: 0.30' ;;\nlimit) echo 'Volume: 0.40 [MUTED]' ;;\nesac\n", shell.display())).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start_with_audio(listener.local_addr().unwrap().port(), Some(&program));
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_topic("fixture/audio/volume/limit");
    let request = serde_json::json!({
        "maximum":50, "request_id":"cap-unmute", "unmute":true,
        "expires_at": SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()+20.0,
    });
    broker.publish_topic(
        42,
        false,
        false,
        "fixture/audio/volume/limit",
        request.to_string().as_bytes(),
    );
    let state = broker.wait_for_audio("cap-unmute");
    assert_eq!(state["limit_ok"], true);
    assert_eq!(state["muted"], false);
    assert_eq!(state["volume"], 30.0);
    let failed = serde_json::json!({
        "maximum": 40, "request_id": "cap-failed", "unmute": true,
        "expires_at": SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() + 20.0,
    });
    broker.publish_topic(
        43,
        false,
        false,
        "fixture/audio/volume/limit",
        failed.to_string().as_bytes(),
    );
    let state = broker.wait_for_audio("cap-failed");
    assert_eq!(state["limit_ok"], false);
    assert_eq!(state["available"], false);
    drop(app);
    fs::remove_dir_all(directory).unwrap();
}

// Linux-only, opt-in release benchmark. No live services or audio are used.
// Run with: cargo test --release --target x86_64-unknown-linux-gnu --test mqtt_updates benchmark_idle -- --ignored --nocapture
#[test]
#[ignore = "60+ seconds of isolated process measurements"]
#[cfg(target_os = "linux")]
fn benchmark_idle() {
    use std::os::unix::fs::PermissionsExt;
    fn sample(pid: u32) -> serde_json::Value {
        let root = format!("/proc/{pid}");
        let stat = fs::read_to_string(format!("{root}/stat")).unwrap();
        let fields: Vec<_> = stat[stat.rfind(')').unwrap() + 2..]
            .split_whitespace()
            .collect();
        let mut result = serde_json::Map::new();
        for (name, index) in [
            ("cpu_ticks", 11),
            ("child_cpu_ticks", 13),
            ("minor_faults", 7),
        ] {
            let value = fields[index].parse::<u64>().unwrap()
                + if name.ends_with("cpu_ticks") {
                    fields[index + 1].parse::<u64>().unwrap()
                } else {
                    0
                };
            result.insert(name.into(), value.into());
        }
        for file in ["status", "smaps_rollup"] {
            for line in fs::read_to_string(format!("{root}/{file}"))
                .unwrap()
                .lines()
            {
                let mut words = line.split_whitespace();
                let key = words.next().unwrap_or_default().trim_end_matches(':');
                if matches!(
                    key,
                    "Rss" | "Pss" | "Private_Clean" | "Private_Dirty" | "VmSize" | "Threads"
                ) {
                    result.insert(
                        key.into(),
                        words.next().unwrap().parse::<u64>().unwrap().into(),
                    );
                }
            }
        }
        let mut switches = 0u64;
        for task in fs::read_dir(format!("{root}/task")).unwrap() {
            for line in fs::read_to_string(task.unwrap().path().join("status"))
                .unwrap()
                .lines()
            {
                if line.starts_with("voluntary_ctxt_switches:")
                    || line.starts_with("nonvoluntary_ctxt_switches:")
                {
                    switches += line
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                }
            }
        }
        result.insert("context_switches".into(), switches.into());
        result.into()
    }
    fn measure(app: &App, broker: &mut Broker, label: &str) {
        let before = sample(app.child.id());
        let started = Instant::now();
        let mut packets = 0;
        while started.elapsed() < Duration::from_secs(15) {
            if let Some((header, body)) = broker.read(Duration::from_secs(1)) {
                broker.respond(header, &body);
                packets += 1;
            }
        }
        println!(
            "BENCH {label} seconds={:.3} packets={packets} before={before} after={}",
            started.elapsed().as_secs_f64(),
            sample(app.child.id())
        );
    }
    for audio in [false, true] {
        if !audio && std::env::var_os("PC2MQTT_BENCH_ALL").is_none() {
            continue;
        }
        let dir = std::env::temp_dir().join(format!("pc2mqtt-bench-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let program = dir.join("adapter");
        let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("sh"))
            .find(|p| p.is_file())
            .unwrap();
        fs::write(
            &program,
            format!(
                "#!{}\necho read >> '{}'/calls\necho 'Volume: 0.30 [MUTED]'\n",
                shell.display(),
                dir.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = unavailable.local_addr().unwrap().port();
        drop(unavailable);
        let app = App::start_with_output(port, audio.then_some(program.as_path()), true);
        thread::sleep(Duration::from_secs(1));
        let before = sample(app.child.id());
        let started = Instant::now();
        thread::sleep(Duration::from_secs(5));
        println!(
            "BENCH audio={audio} broker-refused seconds={:.3} before={before} after={}",
            started.elapsed().as_secs_f64(),
            sample(app.child.id())
        );
        drop(app);
        fs::remove_file(dir.join("calls")).ok();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let app = App::start_with_output(
            listener.local_addr().unwrap().port(),
            audio.then_some(program.as_path()),
            true,
        );
        let mut broker = Broker::accept(&listener, false);
        broker.wait_for_subscription();
        // Drain startup before sampling.
        while let Some((header, body)) = broker.read(Duration::from_millis(300)) {
            broker.respond(header, &body);
        }
        measure(&app, &mut broker, &format!("audio={audio} connected-idle"));
        broker.0.shutdown(std::net::Shutdown::Both).unwrap();
        drop(broker);
        let before = sample(app.child.id());
        let started = Instant::now();
        thread::sleep(Duration::from_secs(5));
        println!(
            "BENCH audio={audio} broker-wait seconds={:.3} before={before} after={}",
            started.elapsed().as_secs_f64(),
            sample(app.child.id())
        );
        let mut broker = Broker::accept(&listener, false);
        broker.wait_for_subscription();
        while let Some((header, body)) = broker.read(Duration::from_millis(300)) {
            broker.respond(header, &body);
        }
        measure(&app, &mut broker, &format!("audio={audio} reconnected"));
        println!(
            "BENCH audio={audio} helper_calls={}",
            fs::read_to_string(dir.join("calls"))
                .unwrap_or_default()
                .lines()
                .count()
        );
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn blocked_audio_does_not_stall_mqtt_or_updates_and_times_out() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("pc2mqtt-blocked-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let program = dir.join("adapter");
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("sh"))
        .find(|p| p.is_file())
        .unwrap();
    fs::write(&program, format!("#!{}\ncase \"$1\" in\nread) echo 'Volume: 0.30 [MUTED]' ;;\n*) exec sleep 30 ;;\nesac\n", shell.display())).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start_with_audio(listener.local_addr().unwrap().port(), Some(&program));
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_topic("fixture/audio/volume/limit");
    let request = serde_json::json!({"maximum":50,"request_id":"blocked","expires_at":SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()+20.0});
    broker.publish_topic(
        40,
        false,
        false,
        "fixture/audio/volume/limit",
        request.to_string().as_bytes(),
    );
    // Retained power commands are safe fixtures and must be promptly rejected/acked.
    broker.publish_topic(41, true, false, "fixture/button/shutdown/command", b"PRESS");
    assert!(broker.acknowledged(41, Duration::from_millis(500)));
    broker.publish(42, false, false);
    assert!(broker.acknowledged(42, Duration::from_millis(500)));
    app.wait_for_requests(2);
    let state = broker.wait_for_audio("blocked");
    assert_eq!(state["available"], false);
    assert_eq!(state["limit_ok"], false);
    drop(app);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn expired_and_contradicted_audio_confirmations_cannot_authorize_playback() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("pc2mqtt-expiry-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let program = dir.join("adapter");
    let reading = dir.join("reading");
    fs::write(&reading, "Volume: 0.30\n").unwrap();
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("sh"))
        .find(|p| p.is_file())
        .unwrap();
    fs::write(
        &program,
        format!(
            "#!{}\nif test \"$1\" = limit-unmute; then sleep 1; fi\ncat '{}'\n",
            shell.display(),
            reading.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start_with_audio(listener.local_addr().unwrap().port(), Some(&program));
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_topic("fixture/audio/volume/limit");
    let request = |id: &str, lifetime, unmute| serde_json::json!({"maximum":50,"request_id":id,"unmute":unmute,"expires_at":SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()+lifetime});
    broker.publish_topic(
        41,
        false,
        false,
        "fixture/audio/volume/limit",
        request("expired", 0.5, true).to_string().as_bytes(),
    );
    let state = broker.wait_for_audio("expired");
    assert_eq!(state["available"], true);
    assert_eq!(state["limit_ok"], false);
    broker.publish_topic(
        42,
        false,
        false,
        "fixture/audio/volume/limit",
        request("contradicted", 40.0, false).to_string().as_bytes(),
    );
    assert_eq!(broker.wait_for_audio("contradicted")["limit_ok"], true);
    fs::write(&reading, "Volume: 0.90\n").unwrap();
    app.audio_changed(b'C');
    assert_eq!(broker.wait_for_audio("contradicted")["limit_ok"], false);
    fs::write(&reading, "Volume: 0.30\n").unwrap();
    app.audio_changed(b'C');
    assert_eq!(broker.wait_for_audio("contradicted")["limit_ok"], false);
    drop(app);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn audio_waits_for_events_expires_without_polling_and_invalidates_speaker_swaps() {
    let dir = std::env::temp_dir().join(format!("pc2mqtt-events-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let program = dir.join("adapter");
    executable(
        &program,
        &format!(
            "echo \"$1\" >> '{}'/calls\necho 'Volume: 0.30'",
            dir.display()
        ),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start_with_audio(listener.local_addr().unwrap().port(), Some(&program));
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_topic("fixture/audio/volume/limit");
    assert_eq!(broker.wait_for_audio_id(None)["volume"], 30.0);
    let calls = || {
        fs::read_to_string(dir.join("calls"))
            .unwrap()
            .lines()
            .count()
    };
    let before = calls();
    assert!(broker.read(Duration::from_millis(5200)).is_none());
    assert_eq!(calls(), before, "idle must not launch audio reads");
    let request = |id, seconds| {
        serde_json::json!({"maximum":50,"request_id":id,
        "expires_at":SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()+seconds})
    };
    broker.publish_topic(
        41,
        false,
        false,
        "fixture/audio/volume/limit",
        request("expiry", 0.5).to_string().as_bytes(),
    );
    assert_eq!(broker.wait_for_audio("expiry")["limit_ok"], true);
    assert_eq!(broker.wait_for_audio("expiry")["limit_ok"], false);
    let before = calls();
    broker.publish_topic(
        42,
        false,
        false,
        "fixture/audio/volume/limit",
        request("speaker-swap", 20.0).to_string().as_bytes(),
    );
    assert_eq!(broker.wait_for_audio("speaker-swap")["limit_ok"], true);
    app.audio_changed(b'R');
    let state = broker.wait_for_audio_id(None);
    assert_eq!(state["limit_ok"], false);
    assert_eq!(state["volume"], 30.0);
    assert_eq!(
        calls(),
        before + 2,
        "only the command and reset should read"
    );
    app.audio_changed(b'D');
    assert_eq!(broker.wait_for_audio_id(None)["available"], false);
    drop(app);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn speaker_swap_during_cap_cannot_publish_a_successful_confirmation() {
    let dir = std::env::temp_dir().join(format!("pc2mqtt-race-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let program = dir.join("adapter");
    executable(
        &program,
        &format!(
            "if test \"$1\" = limit; then touch '{}'/started; sleep 0.3; fi\necho 'Volume: 0.30'",
            dir.display()
        ),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start_with_audio(listener.local_addr().unwrap().port(), Some(&program));
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_topic("fixture/audio/volume/limit");
    let request = serde_json::json!({"maximum":50,"request_id":"racing-speaker",
        "expires_at":SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()+10.0});
    broker.publish_topic(
        41,
        false,
        false,
        "fixture/audio/volume/limit",
        request.to_string().as_bytes(),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !dir.join("started").exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    app.audio_changed(b'R');
    let state = broker.wait_for_audio("racing-speaker");
    assert_eq!(state["limit_ok"], false);
    assert_eq!(state["available"], false);
    drop(app);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn updates_launch_systemd_and_reject_retained_commands() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start(listener.local_addr().unwrap().port());
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    app.wait_for_requests(1);
    broker.publish(41, true, false);
    assert!(broker.acknowledged(41, Duration::from_millis(500)));
    assert_eq!(app.requests(), 1);
    broker.publish(42, false, false);
    assert!(broker.acknowledged(42, Duration::from_millis(500)));
    app.wait_for_requests(2);
    assert!(
        fs::read_to_string(app.directory.join("requests"))
            .unwrap()
            .lines()
            .all(|line| line == "--no-block start nixos-upgrade.service")
    );
    assert!(
        Command::new(binary())
            .arg("--request-update")
            .env("PC2MQTT_UPDATE_SOCKET", app.directory.join("update.sock"))
            .status()
            .unwrap()
            .success()
    );
    app.wait_for_requests(3);
}

#[test]
fn results_survive_agent_downtime_and_broker_reconnection() {
    use std::os::unix::fs::PermissionsExt;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut app = App::start(listener.local_addr().unwrap().port());
    app.report("exit-code", "1");
    assert_eq!(
        fs::metadata(app.directory.join("result.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    let result = broker.wait_for_status("failed");
    assert_eq!(result["result"], "exit-code");
    assert_eq!(result["exit_status"], "1");
    broker.0.shutdown(std::net::Shutdown::Both).unwrap();
    drop(broker);
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    assert_eq!(broker.wait_for_status("failed"), result);
    app.stop();
    // ExecStopPost reports completion even while the MQTT process is stopped.
    app.report("success", "0");
    app.restart();
    drop(broker);
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    assert_eq!(broker.wait_for_status("idle")["result"], "success");
}

#[test]
fn failed_or_blocked_launchers_report_failure_without_stalling_mqtt() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start(listener.local_addr().unwrap().port());
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    app.wait_for_requests(1);
    executable(&app.directory.join("updater"), "exit 1");
    broker.publish(41, false, false);
    assert!(broker.acknowledged(41, Duration::from_millis(500)));
    assert_eq!(broker.wait_for_status("failed")["result"], "launch-failed");
    executable(&app.directory.join("updater"), "exec sleep 30");
    broker.publish(42, false, false);
    broker.publish_topic(43, true, false, "fixture/button/shutdown/command", b"PRESS");
    assert!(broker.acknowledged(43, Duration::from_millis(500)));
    assert!(
        broker.wait_for_status("failed")["exit_status"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
}

#[test]
fn audio_output_is_bounded_and_a_valid_boundary_reading_still_confirms_caps() {
    let dir = std::env::temp_dir().join(format!("pc2mqtt-output-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let program = dir.join("adapter");
    let line = "Volume: 0.30";
    let output = format!("{line}{}", " ".repeat(257 - line.len()));
    executable(&program, &format!("printf '%s' '{output}'"));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start_with_audio(listener.local_addr().unwrap().port(), Some(&program));
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_topic("fixture/audio/volume/limit");
    let request = |id: &str| serde_json::json!({"maximum":50,"request_id":id,"expires_at":SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()+20.0});
    broker.publish_topic(
        41,
        false,
        false,
        "fixture/audio/volume/limit",
        request("oversized").to_string().as_bytes(),
    );
    let result = broker.wait_for_audio("oversized");
    assert_eq!(result["available"], false);
    assert_eq!(result["limit_ok"], false);
    let output = format!("{line}{}", " ".repeat(256 - line.len()));
    executable(&program, &format!("printf '%s' '{output}'"));
    broker.publish_topic(
        42,
        false,
        false,
        "fixture/audio/volume/limit",
        request("bounded").to_string().as_bytes(),
    );
    let result = broker.wait_for_audio("bounded");
    assert_eq!(result["available"], true);
    assert_eq!(result["limit_ok"], true);
    assert_eq!(result["volume"], 30.0);
    drop(app);
    fs::remove_dir_all(dir).unwrap();
}
