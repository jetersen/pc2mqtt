use crate::deployment::Status;
use rumqttc::{Client, Publish, QoS};
use std::{
    collections::HashSet,
    fs, io,
    os::unix::{fs::FileTypeExt, fs::PermissionsExt, net::UnixDatagram},
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, SystemTime},
};

fn socket_path() -> PathBuf {
    std::env::var_os("PC2MQTT_UPDATE_SOCKET")
        .map_or_else(|| PathBuf::from("/run/pc2mqtt/update.sock"), PathBuf::from)
}

pub fn request_local_update() -> io::Result<()> {
    UnixDatagram::unbound()?.send_to(b"UPDATE", socket_path())?;
    Ok(())
}

use hyper_util::rt::TokioIo;
use pc2mqtt::comin::{self, comin_client::CominClient};
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Wait(&'static str),
    RestartFailed,
    Fetch,
}

fn decision(state: &comin::State) -> Decision {
    let (Some(suspended), Some(fetcher), Some(builder), Some(deployer)) = (
        state.is_suspended,
        &state.fetcher,
        &state.builder,
        &state.deployer,
    ) else {
        return Decision::Wait("Comin state is incomplete");
    };
    if suspended || builder.is_suspended == Some(true) || deployer.is_suspended == Some(true) {
        return Decision::Wait("Comin is suspended");
    }
    let flags = [
        fetcher.is_fetching,
        builder.is_evaluating,
        builder.is_building,
        deployer.is_deploying,
    ];
    if flags.iter().any(Option::is_none) {
        return Decision::Wait("Comin state is incomplete");
    }
    if flags.contains(&Some(true)) {
        return Decision::Wait("Comin is busy");
    }
    if builder
        .generation
        .as_ref()
        .is_some_and(|g| g.eval_status == "failed" || g.build_status == "failed")
    {
        return Decision::RestartFailed;
    }
    Decision::Fetch
}

async fn connect() -> Result<(CominClient<Channel>, tonic::Streaming<comin::Event>), String> {
    let path = std::env::var_os("PC2MQTT_COMIN_SOCKET")
        .map_or_else(|| PathBuf::from("/var/lib/comin/grpc.sock"), PathBuf::from);
    let channel = Endpoint::from_static("http://[::]:50051")
        .connect_timeout(Duration::from_secs(5))
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                tokio::net::UnixStream::connect(path)
                    .await
                    .map(TokioIo::new)
            }
        }))
        .await
        .map_err(|e| e.to_string())?;
    let mut client = CominClient::new(channel);
    // A streaming RPC's deadline applies to the whole stream. Bound only setup.
    let events = tokio::time::timeout(Duration::from_secs(5), client.events(()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?
        .into_inner();
    Ok((client, events))
}

async fn state(client: &mut CominClient<Channel>) -> Result<comin::State, tonic::Status> {
    let mut request = tonic::Request::new(());
    request.set_timeout(Duration::from_secs(5));
    Ok(client.get_state(request).await?.into_inner())
}

struct Receipt {
    connection: u64,
    publish: Publish,
}

enum Request {
    Local,
    Mqtt(Receipt),
}

fn drain_requests(
    requests: &Receiver<Request>,
    socket: &tokio::net::UnixDatagram,
    pending: &mut Vec<Request>,
) {
    pending.extend(requests.try_iter());
    let mut buffer = [0; 16];
    while let Ok(length) = socket.try_recv(&mut buffer) {
        if &buffer[..length] == b"UPDATE" {
            pending.push(Request::Local);
        }
    }
}

fn acknowledge(pending: Vec<Request>, client: &Client, connection: &AtomicU64) {
    let mut seen = HashSet::new();
    for request in pending {
        if let Request::Mqtt(receipt) = request
            && seen.insert((receipt.connection, receipt.publish.pkid))
            && connection.load(Ordering::SeqCst) == receipt.connection
            && let Err(error) = client.ack(&receipt.publish)
        {
            eprintln!("Could not acknowledge update command: {error}");
        }
    }
}

async fn submit(
    grpc: &mut Option<(CominClient<Channel>, tonic::Streaming<comin::Event>)>,
    pending: &mut Vec<Request>,
    inflight: &mut Vec<Request>,
    started: &mut prost_types::Timestamp,
) -> String {
    let (client, _) = grpc.as_mut().unwrap();
    match state(client).await {
        Ok(status) => match decision(&status) {
            Decision::Wait(reason) => format!("Update pending: {reason}"),
            Decision::RestartFailed => {
                // No retry-generation RPC exists in Comin 0.14. This is the
                // sole subprocess in update handling; never start a stopped service.
                match Command::new("systemctl")
                    .args(["--no-block", "try-restart", "comin.service"])
                    .status()
                {
                    Ok(status) if status.success() => {
                        "Update pending: restarting Comin after a settled failure".into()
                    }
                    _ => "Update pending: could not restart failed Comin".into(),
                }
            }
            Decision::Fetch => {
                let mut request = tonic::Request::new(());
                request.set_timeout(Duration::from_secs(10));
                *started = SystemTime::now().into();
                match client.fetch(request).await {
                    Ok(_) => {
                        *inflight = std::mem::take(pending);
                        "Comin fetch submitted; waiting for completion".into()
                    }
                    Err(error) => {
                        *grpc = None;
                        format!("Update pending: Comin fetch RPC failed: {error}")
                    }
                }
            }
        },
        Err(error) => {
            *grpc = None;
            format!("Update pending: Comin state RPC failed: {error}")
        }
    }
}

// The worker may wait for outgoing capacity; the main MQTT loop never does so
// on its behalf. Only enqueue while connected, and retain the latest snapshot
// locally for replay on the next connection epoch.
fn publish_status(
    mqtt: &Client,
    topic: &str,
    status: &Status,
    connection: &AtomicU64,
    published: &mut Option<(u64, Status)>,
) {
    let epoch = connection.load(Ordering::SeqCst);
    if epoch & 1 == 0
        || published
            .as_ref()
            .is_some_and(|(previous_epoch, previous)| {
                *previous_epoch == epoch && previous == status
            })
    {
        return;
    }
    if mqtt
        .publish(
            topic,
            QoS::AtLeastOnce,
            true,
            serde_json::to_vec(status).unwrap(),
        )
        .is_ok()
    {
        *published = Some((epoch, status.clone()));
    }
}

async fn telemetry(
    grpc: &mut Option<(CominClient<Channel>, tonic::Streaming<comin::Event>)>,
) -> Status {
    if let Some((client, _)) = grpc {
        match state(client).await {
            Ok(snapshot) => return Status::from_state(&snapshot),
            Err(error) => {
                *grpc = None;
                eprintln!("Comin telemetry unavailable: {error}");
            }
        }
    }
    Status::unavailable()
}

async fn worker(
    requests: Receiver<Request>,
    socket: UnixDatagram,
    mqtt: Client,
    connection: Arc<AtomicU64>,
    status_topic: String,
) {
    socket
        .set_nonblocking(true)
        .expect("nonblocking update socket");
    let socket = tokio::net::UnixDatagram::from_std(socket).expect("async update socket");
    let mut pending = Vec::new();
    let mut inflight = Vec::new();
    let mut started = prost_types::Timestamp::from(SystemTime::UNIX_EPOCH);
    let mut grpc: Option<(CominClient<Channel>, tonic::Streaming<comin::Event>)> = None;
    let mut buffer = [0; 16];
    let mut previous = String::new();
    let mut published = None;
    loop {
        let mut settle = false;
        let mut failed_fetch = false;
        tokio::select! {
            received = socket.recv(&mut buffer) => {
                match received {
                    Ok(length) if &buffer[..length] == b"UPDATE" => pending.push(Request::Local),
                    Ok(_) => {},
                    Err(error) => { eprintln!("Update socket failed: {error}"); return; }
                }
            }
            event = async {
                match &mut grpc {
                    Some((_, events)) => events.message().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Ok(Some(event)) = event {
                    // A previous fetch's event can still be buffered when a new
                    // request is submitted. Only accept completion after submission.
                    if let Some(comin::event::Type::Fetched(fetched)) = event.r#type
                        && event.created_at.is_some_and(|time|
                            (time.seconds, time.nanos) >= (started.seconds, started.nanos))
                    {
                        let success = fetched.repository_status.as_ref().is_some_and(|status|
                            status.error_msg.is_empty() && status.remotes.iter().all(|r| r.fetch_error_msg.is_empty()));
                        if success {
                            if !inflight.is_empty() {
                                acknowledge(std::mem::take(&mut inflight), &mqtt, &connection);
                                eprintln!("Comin fetched remotes; update request handled");
                            }
                        } else {
                            pending.append(&mut inflight);
                            failed_fetch = true;
                            eprintln!("Update pending: Comin fetch failed; waiting for an event");
                        }
                    }
                    settle = true;
                } else {
                    pending.append(&mut inflight);
                    let status = Status::unavailable();
                    publish_status(&mqtt, &status_topic, &status, &connection, &mut published);
                    grpc = connect().await.ok();
                    // If unavailable, the socket path watch wakes us when ready.
                    eprintln!("Comin disconnected; waiting for a lifecycle or MQTT event");
                    continue;
                }
            }
        }
        // Completion events precede clearing some busy flags in Comin 0.14.
        // One settling delay per event, never a repeated readiness poll.
        if settle {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        drain_requests(&requests, &socket, &mut pending);
        if grpc.is_none() {
            match connect().await {
                Ok(connected) => grpc = Some(connected),
                Err(error) => {
                    let status = Status::unavailable();
                    publish_status(&mqtt, &status_topic, &status, &connection, &mut published);
                    eprintln!("Comin unavailable: {error}");
                    continue;
                }
            }
        }
        let mut status = telemetry(&mut grpc).await;
        publish_status(&mqtt, &status_topic, &status, &connection, &mut published);
        if pending.is_empty() || !inflight.is_empty() || failed_fetch || grpc.is_none() {
            continue;
        }
        let message = submit(&mut grpc, &mut pending, &mut inflight, &mut started).await;
        if !inflight.is_empty() {
            // Comin 0.14 emits Fetched but has no FetchStarted event.
            status.state = "updating";
            publish_status(&mqtt, &status_topic, &status, &connection, &mut published);
        }
        if grpc.is_none() {
            status = Status::unavailable();
            publish_status(&mqtt, &status_topic, &status, &connection, &mut published);
            // The socket may already have been replaced before the old channel
            // reports failure. Reconnect once for this event; never run a retry loop.
            grpc = connect().await.ok();
        }
        if message != previous {
            eprintln!("{message}");
            previous = message;
        }
    }
}

pub struct Updates {
    requests: Sender<Request>,
    wake: UnixDatagram,
    // A new connection gets a new odd value; disconnection clears its low bit.
    connection: Arc<AtomicU64>,
}

impl Updates {
    pub fn start(client: Client, device_name: &str) -> io::Result<Self> {
        let status_topic = format!("{device_name}/deployment/state");
        let path = socket_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "update path is not a socket",
                ));
            }
            if UnixDatagram::unbound()?.connect(&path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "update socket is in use",
                ));
            }
            fs::remove_file(&path)?;
        }
        let socket = UnixDatagram::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let wake = UnixDatagram::unbound()?;
        wake.connect(&path)?;
        wake.set_nonblocking(true)?;

        let (requests, receiver) = mpsc::channel();
        let connection = Arc::new(AtomicU64::new(0));
        let worker_connection = Arc::clone(&connection);
        thread::Builder::new()
            .name("pc2mqtt-update".into())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("update runtime")
                    .block_on(worker(
                        receiver,
                        socket,
                        client,
                        worker_connection,
                        status_topic,
                    ));
            })?;
        let updates = Self {
            requests,
            wake,
            connection,
        };
        // Boot checks do not depend on the MQTT broker being reachable.
        updates.queue(Request::Local);
        Ok(updates)
    }

    fn queue(&self, request: Request) {
        self.requests.send(request).expect("update worker stopped");
        // A full socket already has a wakeup queued. The request itself stays
        // in the channel, so MQTT processing never waits on the update worker.
        if let Err(error) = self.wake.send(b"WAKE") {
            assert_eq!(
                error.kind(),
                io::ErrorKind::WouldBlock,
                "update socket failed: {error}"
            );
        }
    }

    pub fn connected(&self) {
        self.connection.fetch_add(2, Ordering::SeqCst);
        self.connection.fetch_or(1, Ordering::SeqCst);
        self.queue(Request::Local);
    }

    pub fn disconnected(&self) {
        self.connection.fetch_and(!1, Ordering::SeqCst);
    }

    pub fn request(&self, publish: Publish) {
        self.queue(Request::Mqtt(Receipt {
            connection: self.connection.load(Ordering::SeqCst),
            publish,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_complete_idle_unsuspended_state_allows_fetch() {
        let mut state = comin::State {
            is_suspended: Some(false),
            fetcher: Some(comin::Fetcher {
                is_fetching: Some(false),
                ..Default::default()
            }),
            builder: Some(comin::Builder {
                is_evaluating: Some(false),
                is_building: Some(false),
                ..Default::default()
            }),
            deployer: Some(comin::Deployer {
                is_deploying: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(decision(&state), Decision::Fetch);
        for flag in 0..7 {
            let mut blocked = state.clone();
            match flag {
                0 => blocked.is_suspended = Some(true),
                1 => blocked.fetcher.as_mut().unwrap().is_fetching = Some(true),
                2 => blocked.builder.as_mut().unwrap().is_evaluating = Some(true),
                3 => blocked.builder.as_mut().unwrap().is_building = Some(true),
                4 => blocked.deployer.as_mut().unwrap().is_deploying = Some(true),
                5 => blocked.builder.as_mut().unwrap().is_suspended = Some(true),
                _ => blocked.deployer.as_mut().unwrap().is_suspended = Some(true),
            }
            assert!(matches!(decision(&blocked), Decision::Wait(_)));
        }
        state.fetcher.as_mut().unwrap().is_fetching = None;
        assert!(matches!(decision(&state), Decision::Wait(_)));
        assert!(matches!(
            decision(&comin::State::default()),
            Decision::Wait(_)
        ));
        state.fetcher.as_mut().unwrap().is_fetching = Some(false);
        state.builder.as_mut().unwrap().generation = Some(comin::Generation {
            eval_status: "failed".into(),
            ..Default::default()
        });
        assert_eq!(decision(&state), Decision::RestartFailed);
    }
}
