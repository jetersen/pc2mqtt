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

use pc2mqtt::comin::{
    self,
    comin_server::{Comin, CominServer},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};

#[derive(Clone)]
struct MockComin {
    state: Arc<Mutex<comin::State>>,
    fetches: Arc<AtomicUsize>,
    deployments: Arc<AtomicUsize>,
    events: tokio::sync::broadcast::Sender<comin::Event>,
}

#[tonic::async_trait]
impl Comin for MockComin {
    async fn get_state(
        &self,
        _: tonic::Request<()>,
    ) -> Result<tonic::Response<comin::State>, tonic::Status> {
        Ok(tonic::Response::new(self.state.lock().unwrap().clone()))
    }
    async fn fetch(&self, _: tonic::Request<()>) -> Result<tonic::Response<()>, tonic::Status> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(tonic::Response::new(()))
    }
    async fn deployment_latest_submit(
        &self,
        request: tonic::Request<comin::Operation>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        assert_eq!(request.into_inner().operation_submitted, "switch");
        self.state
            .lock()
            .unwrap()
            .deployer
            .as_mut()
            .unwrap()
            .is_deploying = Some(true);
        self.deployments.fetch_add(1, Ordering::SeqCst);
        self.events.send(comin::Event::default()).ok();
        Ok(tonic::Response::new(()))
    }
    type EventsStream = ReceiverStream<Result<comin::Event, tonic::Status>>;
    async fn events(
        &self,
        _: tonic::Request<()>,
    ) -> Result<tonic::Response<Self::EventsStream>, tonic::Status> {
        let mut subscription = self.events.subscribe();
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let initial_state = self.state.lock().unwrap().clone();
        sender
            .send(Ok(comin::Event {
                created_at: Some(SystemTime::now().into()),
                r#type: Some(comin::event::Type::ManagerState(
                    comin::event::ManagerState {
                        state: Some(Box::new(initial_state)),
                    },
                )),
            }))
            .await
            .unwrap();
        tokio::spawn(async move {
            while let Ok(event) = subscription.recv().await {
                if sender.send(Ok(event)).await.is_err() {
                    break;
                }
            }
        });
        Ok(tonic::Response::new(ReceiverStream::new(receiver)))
    }
}

struct App {
    directory: PathBuf,
    child: Child,
    comin: MockComin,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<thread::JoinHandle<()>>,
}

impl App {
    fn start(port: u16) -> Self {
        let directory = std::env::temp_dir().join(format!("pc2mqtt-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let comin = MockComin {
            state: Arc::new(Mutex::new(comin::State {
                is_suspended: Some(false),
                fetcher: Some(comin::Fetcher {
                    is_fetching: Some(false),
                    repository_status: Some(comin::RepositoryStatus {
                        selected_commit_id: "target".into(),
                        ..Default::default()
                    }),
                }),
                builder: Some(comin::Builder {
                    is_evaluating: Some(false),
                    is_building: Some(true),
                    generation: Some(comin::Generation {
                        selected_commit_id: "target".into(),
                        build_status: "built".into(),
                        out_path: "/nix/store/target".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                deployer: Some(comin::Deployer {
                    is_deploying: Some(false),
                    deployment: Some(comin::Deployment {
                        uuid: "target".into(),
                        status: "done".into(),
                        operation: "switch".into(),
                        generation: Some(comin::Generation {
                            selected_commit_id: "target".into(),
                            out_path: "/nix/store/target".into(),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                }),
                store: Some(comin::Store {
                    deployment_switched: "target".into(),
                    deployments: vec![comin::Deployment {
                        uuid: "target".into(),
                        generation: Some(comin::Generation {
                            selected_commit_id: "target".into(),
                            out_path: "/nix/store/target".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            })),
            fetches: Arc::new(AtomicUsize::new(0)),
            deployments: Arc::new(AtomicUsize::new(0)),
            events: tokio::sync::broadcast::channel(64).0,
        };
        let (stop, server) = Self::start_server(directory.join("comin.sock"), comin.clone());
        let config = serde_json::json!({
            "device_id": "fixture", "device_name": "fixture", "updates": true,
            "mqtt": {"host":"127.0.0.1", "port":port, "username":"fixture",
                "password":"fixture", "auto_discovery_prefix":"homeassistant"}
        });
        fs::write(directory.join("config.json"), config.to_string()).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_pc2mqtt"))
            .env("PC2MQTT_CONFIG", directory.join("config.json"))
            .env("PC2MQTT_UPDATE_SOCKET", directory.join("update.sock"))
            .env("PC2MQTT_COMIN_SOCKET", directory.join("comin.sock"))
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        Self {
            directory,
            child,
            comin,
            stop: Some(stop),
            server: Some(server),
        }
    }

    fn start_server(
        socket: PathBuf,
        mock: MockComin,
    ) -> (tokio::sync::oneshot::Sender<()>, thread::JoinHandle<()>) {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (ready, started) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::UnixListener::bind(socket).unwrap();
                    ready.send(()).unwrap();
                    tokio::select! {
                        _ = tonic::transport::Server::builder().add_service(CominServer::new(mock))
                            .serve_with_incoming(UnixListenerStream::new(listener)) => {},
                        _ = stopped => {},
                    }
                });
        });
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        (stop, server)
    }

    fn restart_comin(&mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().join().unwrap();
        let socket = self.directory.join("comin.sock");
        fs::remove_file(&socket).unwrap();
        let (stop, server) = Self::start_server(socket, self.comin.clone());
        self.stop = Some(stop);
        self.server = Some(server);
        // Simulate the systemd socket path watch.
        assert!(
            Command::new(env!("CARGO_BIN_EXE_pc2mqtt"))
                .arg("--request-update")
                .env("PC2MQTT_UPDATE_SOCKET", self.directory.join("update.sock"))
                .status()
                .unwrap()
                .success()
        );
    }

    fn set_busy(&self, busy: bool) {
        self.comin
            .state
            .lock()
            .unwrap()
            .builder
            .as_mut()
            .unwrap()
            .is_building = Some(busy);
        // An event wakes the client; modifying state alone must not do so.
        self.comin.events.send(comin::Event::default()).ok();
    }

    fn finish_fetch(&self) {
        self.comin
            .events
            .send(comin::Event {
                created_at: Some(SystemTime::now().into()),
                r#type: Some(comin::event::Type::Fetched(comin::event::Fetched {
                    repository_status: self
                        .comin
                        .state
                        .lock()
                        .unwrap()
                        .fetcher
                        .as_ref()
                        .unwrap()
                        .repository_status
                        .clone(),
                })),
            })
            .unwrap();
    }

    fn fetches(&self) -> usize {
        self.comin.fetches.load(Ordering::SeqCst)
    }

    fn wait_for_fetch(&self, minimum: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.fetches() < minimum {
            assert!(Instant::now() < deadline, "fetch was not requested");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        if let Some(stop) = self.stop.take() {
            stop.send(()).ok();
        }
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
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
    let mut app = App::start(listener.local_addr().unwrap().port());
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

    // Allow the queued local request to observe the busy state first.
    thread::sleep(Duration::from_millis(300));
    // Changing readiness alone does not trigger polling, even after the old interval.
    app.comin
        .state
        .lock()
        .unwrap()
        .builder
        .as_mut()
        .unwrap()
        .is_building = Some(false);
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(app.fetches(), 0);
    app.set_busy(false);
    app.wait_for_fetch(1);
    assert!(
        !broker.acknowledged(42, Duration::from_millis(200)),
        "submission is not completion"
    );
    // A buffered completion from an older fetch cannot acknowledge this batch.
    app.comin
        .events
        .send(comin::Event {
            created_at: Some(SystemTime::UNIX_EPOCH.into()),
            r#type: Some(comin::event::Type::Fetched(comin::event::Fetched {
                repository_status: Some(comin::RepositoryStatus::default()),
            })),
        })
        .unwrap();
    assert!(!broker.acknowledged(42, Duration::from_millis(200)));
    broker.publish(45, false, false);
    app.finish_fetch();
    assert!(broker.acknowledged(42, Duration::from_secs(5)));
    assert!(!broker.acknowledged(45, Duration::from_millis(200)));
    app.wait_for_fetch(2);
    app.finish_fetch();
    assert!(broker.acknowledged(45, Duration::from_secs(3)));

    app.set_busy(true);
    broker.publish(44, false, false);
    assert!(!broker.acknowledged(44, Duration::from_millis(200)));
    drop(broker);
    let mut broker = Broker::accept(&listener, true);
    broker.wait_for_subscription();
    broker.publish(44, false, true);
    assert!(!broker.acknowledged(44, Duration::from_millis(200)));
    let previous = app.fetches();
    app.set_busy(false);
    app.wait_for_fetch(previous + 1);
    app.comin
        .events
        .send(comin::Event {
            created_at: Some(SystemTime::now().into()),
            r#type: Some(comin::event::Type::Fetched(comin::event::Fetched {
                repository_status: Some(comin::RepositoryStatus {
                    error_msg: "network unavailable".into(),
                    ..Default::default()
                }),
            })),
        })
        .unwrap();
    assert!(!broker.acknowledged(44, Duration::from_millis(1200)));
    assert_eq!(
        app.fetches(),
        previous + 1,
        "failed fetch must wait for an event"
    );
    let requested = Command::new(env!("CARGO_BIN_EXE_pc2mqtt"))
        .arg("--request-update")
        .env("PC2MQTT_UPDATE_SOCKET", app.directory.join("update.sock"))
        .status()
        .unwrap();
    assert!(requested.success());
    app.wait_for_fetch(previous + 2);
    app.finish_fetch();
    assert!(broker.acknowledged(44, Duration::from_secs(5)));
    assert!(app.fetches() >= 2);

    app.set_busy(true);
    broker.publish(46, false, false);
    assert!(!broker.acknowledged(46, Duration::from_millis(200)));
    app.restart_comin();
    assert!(!broker.acknowledged(46, Duration::from_millis(200)));
    let previous = app.fetches();
    app.set_busy(false);
    app.wait_for_fetch(previous + 1);
    app.finish_fetch();
    assert!(broker.acknowledged(46, Duration::from_secs(3)));
}

#[test]
fn deployment_discovery_tracks_idle_events_and_republishes_after_mqtt_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut app = App::start(listener.local_addr().unwrap().port());
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    broker.wait_for_status("updating");
    {
        let mut state = app.comin.state.lock().unwrap();
        state.need_to_reboot = Some(true);
        state.store = Some(comin::Store {
            deployment_switched: "new".into(),
            deployment_booted: "old".into(),
            deployments: [("new", "deployed"), ("old", "booted")]
                .into_iter()
                .map(|(id, revision)| comin::Deployment {
                    uuid: id.into(),
                    generation: Some(comin::Generation {
                        selected_commit_id: revision.into(),
                        out_path: if id == "new" {
                            "/nix/store/target"
                        } else {
                            "/nix/store/old"
                        }
                        .into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
        });
    }
    app.set_busy(false);
    let status = broker.wait_for_status("updating");
    // The fetch request is still pending even though Comin is momentarily idle.
    assert_eq!(status["deployed_revision"], "deployed");
    assert_eq!(status["booted_revision"], "booted");
    assert_eq!(status["reboot_required"], true);
    app.wait_for_fetch(1);
    app.finish_fetch();
    thread::sleep(Duration::from_millis(300));
    app.comin.state.lock().unwrap().is_suspended = Some(true);
    app.comin.events.send(comin::Event::default()).unwrap();
    broker.wait_for_status("suspended");
    let configs: Vec<_> = broker
        .1
        .iter()
        .filter(|(topic, _, _)| topic.ends_with("/config"))
        .collect();
    assert_eq!(configs.len(), 6);
    assert!(configs.iter().all(|(_, _, retained)| *retained));
    drop(broker);
    let mut broker = Broker::accept(&listener, true);
    broker.wait_for_subscription();
    broker.wait_for_status("suspended");
    {
        let mut state = app.comin.state.lock().unwrap();
        state.is_suspended = Some(false);
        state.deployer.as_mut().unwrap().deployment = Some(comin::Deployment {
            status: "failed".into(),
            ..Default::default()
        });
    }
    app.comin.events.send(comin::Event::default()).unwrap();
    broker.wait_for_status("failed");
    app.stop.take().unwrap().send(()).unwrap();
    app.server.take().unwrap().join().unwrap();
    broker.wait_for_status("unavailable");
}

#[test]
fn reconnect_restores_the_fetched_generation_before_acknowledging_updates() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start(listener.local_addr().unwrap().port());
    {
        let mut state = app.comin.state.lock().unwrap();
        let store = state.store.as_mut().unwrap();
        store.deployment_switched = "old-boot".into();
        store.deployments.push(comin::Deployment {
            uuid: "old-boot".into(),
            generation: Some(comin::Generation {
                out_path: "/nix/store/old-boot".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    broker.publish(60, false, false);
    app.set_busy(false);
    app.wait_for_fetch(1);
    app.finish_fetch();
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.comin.deployments.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "older boot was not reconciled");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !broker.acknowledged(60, Duration::from_millis(300)),
        "fetch/submit is not convergence"
    );
    for _ in 0..3 {
        app.comin.events.send(comin::Event::default()).unwrap();
    }
    assert!(!broker.acknowledged(60, Duration::from_millis(300)));
    assert_eq!(app.comin.deployments.load(Ordering::SeqCst), 1);
    // An MQTT reconnect replays the unacknowledged request, never loses it.
    drop(broker);
    let mut broker = Broker::accept(&listener, true);
    broker.wait_for_subscription();
    broker.publish(60, false, true);
    assert!(!broker.acknowledged(60, Duration::from_millis(200)));
    {
        let mut state = app.comin.state.lock().unwrap();
        state.store.as_mut().unwrap().deployment_switched = "target".into();
        state.deployer.as_mut().unwrap().is_deploying = Some(false);
    }
    app.comin.events.send(comin::Event::default()).unwrap();
    // The replay belongs to the next request batch, which fetches again.
    app.wait_for_fetch(2);
    app.finish_fetch();
    assert!(broker.acknowledged(60, Duration::from_secs(3)));
    assert_eq!(app.comin.deployments.load(Ordering::SeqCst), 1);
}

#[test]
fn a_new_commit_waits_for_its_build_instead_of_resubmitting_an_old_deployment() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start(listener.local_addr().unwrap().port());
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    broker.publish(61, false, false);
    app.comin
        .state
        .lock()
        .unwrap()
        .fetcher
        .as_mut()
        .unwrap()
        .repository_status
        .as_mut()
        .unwrap()
        .selected_commit_id = "new-target".into();
    app.set_busy(false);
    app.wait_for_fetch(1);
    app.finish_fetch();
    assert!(!broker.acknowledged(61, Duration::from_millis(300)));
    assert_eq!(app.comin.deployments.load(Ordering::SeqCst), 0);
    // Another successful fetch during deployment supersedes this batch's target.
    app.comin
        .state
        .lock()
        .unwrap()
        .fetcher
        .as_mut()
        .unwrap()
        .repository_status
        .as_mut()
        .unwrap()
        .selected_commit_id = "newer-target".into();
    app.finish_fetch();
    {
        let mut state = app.comin.state.lock().unwrap();
        state
            .builder
            .as_mut()
            .unwrap()
            .generation
            .as_mut()
            .unwrap()
            .selected_commit_id = "new-target".into();
    }
    app.comin.events.send(comin::Event::default()).unwrap();
    assert!(!broker.acknowledged(61, Duration::from_millis(300)));
    {
        let mut state = app.comin.state.lock().unwrap();
        state
            .builder
            .as_mut()
            .unwrap()
            .generation
            .as_mut()
            .unwrap()
            .selected_commit_id = "newer-target".into();
        // An unchanged system derivation needs no redeployment.
    }
    app.comin.events.send(comin::Event::default()).unwrap();
    assert!(broker.acknowledged(61, Duration::from_secs(3)));
}

#[test]
fn rejected_branch_history_keeps_the_update_queued_until_a_successful_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = App::start(listener.local_addr().unwrap().port());
    let mut broker = Broker::accept(&listener, false);
    broker.wait_for_subscription();
    broker.publish(62, false, false);
    app.set_busy(false);
    app.wait_for_fetch(1);
    app.comin
        .state
        .lock()
        .unwrap()
        .fetcher
        .as_mut()
        .unwrap()
        .repository_status
        .as_mut()
        .unwrap()
        .remotes
        .push(comin::Remote {
            main: Some(comin::Branch {
                error_msg: "this branch has been hard reset".into(),
            }),
            ..Default::default()
        });
    app.finish_fetch();
    assert!(!broker.acknowledged(62, Duration::from_millis(400)));
    broker.wait_for_status("failed");
    assert_eq!(
        app.fetches(),
        1,
        "a rejected branch must wait for another event"
    );
    app.comin
        .state
        .lock()
        .unwrap()
        .fetcher
        .as_mut()
        .unwrap()
        .repository_status
        .as_mut()
        .unwrap()
        .remotes
        .clear();
    app.comin.events.send(comin::Event::default()).unwrap();
    app.wait_for_fetch(2);
    app.finish_fetch();
    assert!(broker.acknowledged(62, Duration::from_secs(3)));
}
