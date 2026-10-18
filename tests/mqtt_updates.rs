use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const TOPIC: &str = "fixture/button/update/command";

struct App {
    directory: PathBuf,
    child: Child,
}

impl App {
    fn start(port: u16) -> Self {
        let directory = std::env::temp_dir().join(format!("pc2mqtt-test-{}", uuid::Uuid::new_v4()));
        let bin = directory.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let path = std::env::var_os("PATH").unwrap();
        let shell = std::env::split_paths(&path)
            .map(|directory| directory.join("sh"))
            .find(|candidate| candidate.is_file())
            .expect("POSIX shell is required");
        for (name, body) in [
            (
                "systemctl",
                "test \"$*\" = '--quiet is-active comin.service'",
            ),
            (
                "comin",
                "case \"$*\" in\n  'status --json') cat \"$MOCK_STATUS\" ;;\n  fetch) printf 'fetch\\n' >> \"$MOCK_FETCHES\" ;;\n  *) exit 1 ;;\nesac",
            ),
        ] {
            let script = bin.join(name);
            fs::write(&script, format!("#!{}\nset -eu\n{body}\n", shell.display())).unwrap();
            fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(directory.join("status.json"), Self::status(true)).unwrap();
        let config = serde_json::json!({
            "device_id": "fixture", "device_name": "fixture", "updates": true,
            "mqtt": {"host":"127.0.0.1", "port":port, "username":"fixture",
                "password":"fixture", "auto_discovery_prefix":"homeassistant"}
        });
        fs::write(directory.join("config.json"), config.to_string()).unwrap();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path))).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_pc2mqtt"))
            .env("PC2MQTT_CONFIG", directory.join("config.json"))
            .env("PC2MQTT_UPDATE_SOCKET", directory.join("update.sock"))
            .env("MOCK_STATUS", directory.join("status.json"))
            .env("MOCK_FETCHES", directory.join("fetches"))
            .env("PATH", path)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        Self { directory, child }
    }

    fn status(busy: bool) -> String {
        serde_json::json!({
            "is_suspended":false, "fetcher":{"is_fetching":false},
            "builder":{"is_evaluating":false,"is_building":busy,"generation":null},
            "deployer":{"is_deploying":false}
        })
        .to_string()
    }

    fn set_busy(&self, busy: bool) {
        let temporary = self.directory.join("next-status.json");
        fs::write(&temporary, Self::status(busy)).unwrap();
        fs::rename(temporary, self.directory.join("status.json")).unwrap();
    }

    fn fetches(&self) -> usize {
        fs::read_to_string(self.directory.join("fetches"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        fs::remove_dir_all(&self.directory).ok();
    }
}

struct Broker(TcpStream);

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
        let mut broker = Self(stream);
        let (header, connect) = broker.read(Duration::from_secs(3)).unwrap();
        assert_eq!(header >> 4, 1);
        assert_eq!(
            connect[7] & 2,
            0,
            "update subscriptions need a persistent session"
        );
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
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let (header, body) = self.read(Duration::from_millis(100)).unwrap_or_default();
            self.respond(header, &body);
            if header >> 4 == 8 && body[4..body.len() - 1] == *TOPIC.as_bytes() {
                return;
            }
        }
        panic!("update topic was not subscribed");
    }

    fn publish(&mut self, id: u16, retained: bool, duplicate: bool) {
        let topic_length = u16::try_from(TOPIC.len()).unwrap();
        let mut body = Vec::from(topic_length.to_be_bytes());
        body.extend_from_slice(TOPIC.as_bytes());
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(b"PRESS");
        let header = 0x32 | u8::from(retained) | (u8::from(duplicate) << 3);
        self.0
            .write_all(&[header, u8::try_from(body.len()).unwrap()])
            .unwrap();
        self.0.write_all(&body).unwrap();
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
fn update_acknowledgements_wait_for_comin_and_survive_reconnection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start(listener.local_addr().unwrap().port());
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    broker.publish(42, false, false);
    assert!(!broker.acknowledged(42, Duration::from_millis(200)));
    assert_eq!(app.fetches(), 0);

    // A burst shares the same worker, even when its wakeup socket fills.
    for id in 100..132 {
        broker.publish(id, false, false);
    }

    // A rejected retained command is acknowledged without starting a fetch.
    broker.publish(43, true, false);
    assert!(broker.acknowledged(43, Duration::from_secs(2)));
    assert_eq!(app.fetches(), 0);
    let requested = Command::new(env!("CARGO_BIN_EXE_pc2mqtt"))
        .arg("--request-update")
        .env("PC2MQTT_UPDATE_SOCKET", app.directory.join("update.sock"))
        .status()
        .unwrap();
    assert!(requested.success());

    app.set_busy(false);
    assert!(broker.acknowledged(42, Duration::from_secs(5)));
    assert_eq!(app.fetches(), 1);

    app.set_busy(true);
    broker.publish(44, false, false);
    assert!(!broker.acknowledged(44, Duration::from_millis(200)));
    drop(broker);
    let mut broker = Broker::accept(&listener, true);
    broker.wait_for_subscription();
    broker.publish(44, false, true);
    assert!(!broker.acknowledged(44, Duration::from_millis(200)));
    app.set_busy(false);
    assert!(broker.acknowledged(44, Duration::from_secs(5)));
    assert!(app.fetches() >= 2);
}
