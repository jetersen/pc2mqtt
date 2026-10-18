use rumqttc::{Client, Publish};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs, io,
    os::unix::{fs::FileTypeExt, fs::PermissionsExt, net::UnixDatagram},
    path::PathBuf,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

const CHECK_INTERVAL: Duration = Duration::from_secs(1);

fn socket_path() -> PathBuf {
    std::env::var_os("PC2MQTT_UPDATE_SOCKET")
        .map_or_else(|| PathBuf::from("/run/pc2mqtt/update.sock"), PathBuf::from)
}

pub fn request_local_update() -> io::Result<()> {
    UnixDatagram::unbound()?.send_to(b"UPDATE", socket_path())?;
    Ok(())
}

#[derive(Deserialize)]
struct Fetcher {
    is_fetching: bool,
}

#[derive(Deserialize)]
struct Builder {
    is_evaluating: bool,
    is_building: bool,
    generation: Option<Generation>,
}

#[derive(Deserialize)]
struct Generation {
    eval_status: Option<String>,
    build_status: Option<String>,
}

#[derive(Deserialize)]
struct Deployer {
    is_deploying: bool,
}

#[derive(Deserialize)]
struct Status {
    is_suspended: bool,
    fetcher: Fetcher,
    builder: Builder,
    deployer: Deployer,
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Wait(&'static str),
    RestartFailed,
    Fetch,
}

impl Status {
    fn decision(&self) -> Decision {
        if self.is_suspended {
            return Decision::Wait("Comin is suspended");
        }
        if self.fetcher.is_fetching
            || self.builder.is_evaluating
            || self.builder.is_building
            || self.deployer.is_deploying
        {
            return Decision::Wait("Comin is busy");
        }
        if self.builder.generation.as_ref().is_some_and(|generation| {
            generation.eval_status.as_deref() == Some("failed")
                || generation.build_status.as_deref() == Some("failed")
        }) {
            return Decision::RestartFailed;
        }
        Decision::Fetch
    }
}

fn command(seconds: &str, program: &str, args: &[&str]) -> Result<Output, String> {
    Command::new("timeout")
        .arg(seconds)
        .arg(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not run {program}: {error}"))
}

fn successful(seconds: &str, program: &str, args: &[&str]) -> Result<(), String> {
    let output = command(seconds, program, args)?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            output.status
        ));
    }
    Ok(())
}

fn try_update() -> Result<Decision, String> {
    // Never start a stopped daemon or resume a suspended deployment.
    if !command("5", "systemctl", &["--quiet", "is-active", "comin.service"])?
        .status
        .success()
    {
        return Ok(Decision::Wait("Comin is stopped"));
    }
    let output = command("5", "comin", &["status", "--json"])?;
    if !output.status.success() {
        return Err(format!("Comin status failed: {}", output.status));
    }
    let status: Status = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid Comin status: {error}"))?;
    match status.decision() {
        Decision::Fetch => successful("10", "comin", &["fetch"]),
        // Comin 0.14 does not rebuild an unchanged failed commit after a fetch.
        Decision::RestartFailed => successful("30", "systemctl", &["try-restart", "comin.service"]),
        Decision::Wait(_) => return Ok(status.decision()),
    }?;
    Ok(status.decision())
}

struct Receipt {
    connection: u64,
    publish: Publish,
}

enum Request {
    Local,
    Mqtt(Receipt),
}

fn next_request(requests: &Receiver<Request>, socket: Option<&UnixDatagram>) -> Option<Request> {
    let Some(socket) = socket else {
        return requests.recv().ok();
    };
    let mut buffer = [0_u8; 16];
    loop {
        if let Ok(request) = requests.try_recv() {
            return Some(request);
        }
        match socket.recv(&mut buffer) {
            Ok(length) if &buffer[..length] == b"UPDATE" => return Some(Request::Local),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                eprintln!("Update socket failed: {error}");
                return None;
            }
        }
    }
}

fn drain_requests(requests: &Receiver<Request>, socket: Option<&UnixDatagram>) -> Vec<Request> {
    let mut pending: Vec<_> = requests.try_iter().collect();
    if let Some(socket) = socket {
        socket
            .set_nonblocking(true)
            .expect("could not drain update socket");
        let mut buffer = [0_u8; 16];
        while let Ok(length) = socket.recv(&mut buffer) {
            if &buffer[..length] == b"UPDATE" {
                pending.push(Request::Local);
            }
        }
        socket
            .set_nonblocking(false)
            .expect("could not wait on update socket");
    }
    pending
}

fn worker(
    requests: &Receiver<Request>,
    socket: Option<&UnixDatagram>,
    mut update: impl FnMut() -> Result<Decision, String>,
    mut handled: impl FnMut(Vec<Receipt>),
) {
    while let Some(first) = next_request(requests, socket) {
        let mut pending = vec![first];
        let mut previous = String::new();
        loop {
            pending.extend(drain_requests(requests, socket));
            let message = match update() {
                Ok(Decision::Fetch) => {
                    let mut seen = HashSet::new();
                    let receipts = pending
                        .into_iter()
                        .filter_map(|request| match request {
                            Request::Mqtt(receipt)
                                if seen.insert((receipt.connection, receipt.publish.pkid)) =>
                            {
                                Some(receipt)
                            }
                            _ => None,
                        })
                        .collect();
                    handled(receipts);
                    eprintln!("Comin fetched remotes; update request handled");
                    break;
                }
                Ok(Decision::Wait(reason)) => format!("Update pending: {reason}"),
                Ok(Decision::RestartFailed) => {
                    "Update pending: restarted Comin after a settled failure".into()
                }
                Err(error) => format!("Update pending: {error}"),
            };
            if message != previous {
                eprintln!("{message}");
                previous = message;
            }
            // Only pending work checks readiness. Incoming requests coalesce into
            // this batch; requests arriving during a fetch get their own fetch.
            thread::sleep(CHECK_INTERVAL);
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
    pub fn start(client: Client) -> io::Result<Self> {
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
                worker(&receiver, Some(&socket), try_update, |receipts| {
                    for receipt in receipts {
                        // Old packet IDs belong to the old connection. Persistent
                        // sessions redeliver unacknowledged commands on reconnect.
                        if worker_connection.load(Ordering::SeqCst) == receipt.connection
                            && let Err(error) = client.ack(&receipt.publish)
                        {
                            eprintln!("Could not acknowledge update command: {error}");
                        }
                    }
                });
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
    use std::sync::atomic::AtomicBool;

    fn idle_status() -> serde_json::Value {
        serde_json::json!({
            "is_suspended": false,
            "fetcher": { "is_fetching": false },
            "builder": { "is_evaluating": false, "is_building": false, "generation": null },
            "deployer": { "is_deploying": false }
        })
    }

    #[test]
    fn only_idle_unsuspended_complete_status_allows_fetch() {
        let status = idle_status();
        assert_eq!(
            serde_json::from_value::<Status>(status.clone())
                .unwrap()
                .decision(),
            Decision::Fetch
        );
        for pointer in [
            "/is_suspended",
            "/fetcher/is_fetching",
            "/builder/is_evaluating",
            "/builder/is_building",
            "/deployer/is_deploying",
        ] {
            let mut busy = status.clone();
            *busy.pointer_mut(pointer).unwrap() = true.into();
            assert!(matches!(
                serde_json::from_value::<Status>(busy).unwrap().decision(),
                Decision::Wait(_)
            ));
        }
        for component in ["fetcher", "builder", "deployer"] {
            let mut incomplete = status.clone();
            incomplete.as_object_mut().unwrap().remove(component);
            assert!(serde_json::from_value::<Status>(incomplete).is_err());
        }
        let mut failed = status;
        failed["builder"]["generation"] = serde_json::json!({"eval_status":"failed"});
        assert_eq!(
            serde_json::from_value::<Status>(failed).unwrap().decision(),
            Decision::RestartFailed
        );
    }

    fn request(id: u16) -> Request {
        let mut publish = Publish::new(
            "test/button/update/command",
            rumqttc::QoS::AtLeastOnce,
            "PRESS",
        );
        publish.pkid = id;
        Request::Mqtt(Receipt {
            connection: 3,
            publish,
        })
    }

    #[test]
    fn busy_requests_stay_unacknowledged_and_coalesce_when_idle() {
        let (sender, requests) = mpsc::channel();
        let (acknowledge, received) = mpsc::channel();
        let idle = Arc::new(AtomicBool::new(false));
        let ready = Arc::clone(&idle);
        let task = thread::spawn(move || {
            worker(
                &requests,
                None,
                || {
                    Ok(if ready.load(Ordering::SeqCst) {
                        Decision::Fetch
                    } else {
                        Decision::Wait("Comin is busy")
                    })
                },
                |receipts| {
                    acknowledge
                        .send(receipts.iter().map(|r| r.publish.pkid).collect::<Vec<_>>())
                        .unwrap();
                },
            );
        });
        sender.send(request(1)).unwrap();
        sender.send(request(1)).unwrap();
        sender.send(request(2)).unwrap();
        assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
        idle.store(true, Ordering::SeqCst);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(3)).unwrap(),
            vec![1, 2]
        );
        drop(sender);
        task.join().unwrap();
    }

    #[test]
    fn a_request_arriving_during_fetch_needs_a_subsequent_fetch() {
        let (sender, requests) = mpsc::channel();
        let (started, starting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (acknowledge, received) = mpsc::channel();
        let task = thread::spawn(move || {
            worker(
                &requests,
                None,
                || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                    Ok(Decision::Fetch)
                },
                |receipts| {
                    acknowledge
                        .send(receipts.iter().map(|r| r.publish.pkid).collect::<Vec<_>>())
                        .unwrap();
                },
            );
        });
        sender.send(request(1)).unwrap();
        starting.recv_timeout(Duration::from_secs(3)).unwrap();
        sender.send(request(2)).unwrap();
        release.send(()).unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(3)).unwrap(),
            vec![1]
        );
        starting.recv_timeout(Duration::from_secs(3)).unwrap();
        release.send(()).unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(3)).unwrap(),
            vec![2]
        );
        drop(sender);
        task.join().unwrap();
    }
}
