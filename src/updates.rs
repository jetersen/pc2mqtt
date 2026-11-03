//! Launch a systemd-owned upgrade and publish its latest persisted result.
use rumqttc::{AsyncClient, QoS};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    fs, io,
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::UnixDatagram,
    },
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn socket_path() -> PathBuf {
    std::env::var_os("PC2MQTT_UPDATE_SOCKET")
        .map_or_else(|| PathBuf::from("/run/pc2mqtt/update.sock"), PathBuf::from)
}

fn result_path() -> PathBuf {
    std::env::var_os("PC2MQTT_UPDATE_RESULT").map_or_else(
        || PathBuf::from("/var/lib/pc2mqtt/update-result.json"),
        PathBuf::from,
    )
}

pub fn request_local_update() -> io::Result<()> {
    UnixDatagram::unbound()?.send_to(b"UPDATE", socket_path())?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct ResultState<'a> {
    #[serde(borrow)]
    state: Cow<'a, str>,
    #[serde(borrow)]
    result: Cow<'a, str>,
    #[serde(borrow)]
    exit_status: Option<Cow<'a, str>>,
    completed_at: u64,
}

fn save_result(result: &str, exit_status: Option<&str>) -> io::Result<()> {
    use io::Write;
    let state = ResultState {
        state: if result == "success" {
            "idle"
        } else {
            "failed"
        }
        .into(),
        result: Cow::Borrowed(result),
        exit_status: exit_status.map(Cow::Borrowed),
        completed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let path = result_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(&state)?)?;
        fs::rename(&temporary, &path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(temporary);
    }
    written
}

/// Called by `ExecStopPost`, even if the main MQTT agent is being restarted.
pub fn report_result() -> io::Result<()> {
    let result = std::env::var("SERVICE_RESULT")
        .map_err(|_| io::Error::other("SERVICE_RESULT is required"))?;
    save_result(&result, std::env::var("EXIT_STATUS").ok().as_deref())?;
    // The file survives agent downtime and broker outages. A wakeup is best effort.
    if let Ok(socket) = UnixDatagram::unbound() {
        let _ = socket.set_nonblocking(true);
        let _ = socket.send_to(b"RESULT", socket_path());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Wake {
    Request,
    Connected,
}

pub struct Updates {
    wake: tokio::sync::mpsc::Sender<Wake>,
}

async fn launch() -> Result<(), String> {
    let program = std::env::var_os("PC2MQTT_UPDATE_COMMAND").unwrap_or_else(|| "systemctl".into());
    let mut command = tokio::process::Command::new(program);
    command
        .args(["--no-block", "start", "nixos-upgrade.service"])
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(2), command.status())
        .await
        .map_err(|_| "upgrade service launch timed out".to_owned())?
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(status.to_string())
    }
}

fn latest_result() -> io::Result<String> {
    let path = result_path();
    if fs::metadata(&path)?.len() > 16 * 1024 {
        return Err(io::Error::other("upgrade result is too large"));
    }
    // Validate before forwarding a file to MQTT.
    let input = fs::read(path)?;
    let result: ResultState<'_> = serde_json::from_slice(&input)?;
    Ok(serde_json::to_string(&result)?)
}

async fn publish_latest(client: &AsyncClient, topic: &str, published: &mut Option<String>) {
    let result = match latest_result() {
        Ok(result) => result,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!("Could not read upgrade result: {error}");
            return;
        }
    };
    if published.as_ref() != Some(&result)
        && client
            .publish(topic, QoS::AtLeastOnce, true, result.as_bytes())
            .await
            .is_ok()
    {
        *published = Some(result);
    }
}

impl Updates {
    pub fn start(client: AsyncClient, name: &str) -> io::Result<Self> {
        let path = socket_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if !metadata.file_type().is_socket() {
                return Err(io::Error::other("update path is not a socket"));
            }
            if UnixDatagram::unbound()?.connect(&path).is_ok() {
                return Err(io::Error::other("update socket is in use"));
            }
            fs::remove_file(&path)?;
        }
        let socket = UnixDatagram::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        socket.set_nonblocking(true)?;
        let socket = tokio::net::UnixDatagram::from_std(socket)?;
        let (wake, mut receiver) = tokio::sync::mpsc::channel(8);
        let topic = format!("{name}/deployment/state");
        tokio::spawn(async move {
            let mut published = None;
            let mut buffer = [0; 16];
            loop {
                let request = tokio::select! {
                    wake = receiver.recv() => match wake {
                        Some(Wake::Request) => true,
                        Some(Wake::Connected) => { published = None; true },
                        None => return,
                    },
                    packet = socket.recv(&mut buffer) => match packet {
                        Ok(length) => &buffer[..length] == b"UPDATE",
                        Err(error) => { eprintln!("Update socket failed: {error}"); return; }
                    },
                };
                if request && let Err(error) = launch().await {
                    eprintln!("Could not start upgrade: {error}");
                    if let Err(error) = save_result("launch-failed", Some(&error)) {
                        eprintln!("Could not save upgrade failure: {error}");
                    }
                }
                publish_latest(&client, &topic, &mut published).await;
            }
        });
        Ok(Self { wake })
    }

    pub fn request(&self) {
        let _ = self.wake.try_send(Wake::Request);
    }
    pub fn connected(&self) {
        let _ = self.wake.try_send(Wake::Connected);
    }
}
