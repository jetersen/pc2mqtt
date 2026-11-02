//! Change notifications share the agent's thread. The optional native binding
//! owns its C objects and never moves them across threads.
use std::{future::pending, io};

pub const CHANGED: i32 = 1;
pub const RESET: i32 = 2;
pub const DISCONNECTED: i32 = 4;

pub enum Events {
    #[cfg(feature = "pipewire")]
    Native(tokio::io::unix::AsyncFd<native::Listener>),
    // An alternative notification source, also useful for isolated fixtures.
    Socket(tokio::net::UnixDatagram),
    Disabled,
}
impl Events {
    pub fn disconnect(&mut self) -> bool {
        if matches!(self, Self::Socket(_)) {
            return false;
        }
        *self = Self::Disabled;
        true
    }
    pub fn open() -> io::Result<Self> {
        if let Some(path) = std::env::var_os("PC2MQTT_AUDIO_EVENT_SOCKET") {
            use std::os::unix::fs::PermissionsExt;
            let socket = tokio::net::UnixDatagram::bind(&path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            return Ok(Self::Socket(socket));
        }
        if let Some(path) = std::env::var_os("PC2MQTT_AUDIO_SOCKET") {
            #[cfg(feature = "pipewire")]
            return native::Listener::open(&path)
                .and_then(tokio::io::unix::AsyncFd::new)
                .map(Self::Native);
            #[cfg(not(feature = "pipewire"))]
            {
                let _ = path;
                return Err(io::Error::other(
                    "build with --features pipewire for audio events",
                ));
            }
        }
        Ok(Self::Disabled)
    }

    pub fn drain(&mut self) -> i32 {
        match self {
            #[cfg(feature = "pipewire")]
            Self::Native(fd) => fd.get_ref().dispatch(),
            Self::Socket(socket) => {
                let mut flags = 0;
                let mut bytes = [0; 1];
                while socket.try_recv(&mut bytes).is_ok() {
                    flags |= decode(bytes[0]);
                }
                flags
            }
            Self::Disabled => 0,
        }
    }

    pub async fn next(&mut self) -> i32 {
        match self {
            #[cfg(feature = "pipewire")]
            Self::Native(fd) => loop {
                let Ok(mut ready) = fd.readable().await else {
                    return DISCONNECTED;
                };
                let flags = fd.get_ref().dispatch();
                ready.clear_ready();
                if flags != 0 {
                    return flags;
                }
            },
            Self::Socket(socket) => {
                let mut bytes = [0; 1];
                match socket.recv(&mut bytes).await {
                    Ok(1) => decode(bytes[0]),
                    _ => DISCONNECTED,
                }
            }
            Self::Disabled => pending().await,
        }
    }
}
fn decode(byte: u8) -> i32 {
    match byte {
        b'C' => CHANGED,
        b'R' => RESET | CHANGED,
        b'D' => DISCONNECTED,
        _ => 0,
    }
}

#[cfg(feature = "pipewire")]
mod native {
    use super::io;
    use std::{
        ffi::{CString, OsStr, c_char, c_int, c_void},
        marker::PhantomData,
        os::{
            fd::{AsRawFd, RawFd},
            unix::ffi::OsStrExt,
        },
        ptr::NonNull,
        rc::Rc,
    };

    unsafe extern "C" {
        fn pc2mqtt_events_new(socket: *const c_char) -> *mut c_void;
        fn pc2mqtt_events_free(listener: *mut c_void);
        fn pc2mqtt_events_fd(listener: *mut c_void) -> c_int;
        fn pc2mqtt_events_dispatch(listener: *mut c_void) -> c_int;
    }
    pub struct Listener {
        pointer: NonNull<c_void>,
        _thread: PhantomData<Rc<()>>,
    }
    impl Listener {
        pub fn open(path: &OsStr) -> io::Result<Self> {
            let path = CString::new(path.as_bytes())?;
            // C copies the socket name and returns an exclusively owned object.
            let pointer = NonNull::new(unsafe { pc2mqtt_events_new(path.as_ptr()) })
                .ok_or_else(io::Error::last_os_error)?;
            Ok(Self {
                pointer,
                _thread: PhantomData,
            })
        }
        pub fn dispatch(&self) -> i32 {
            // LocalSet keeps creation, callbacks, iteration and destruction on
            // one thread; no callback calls back into Rust or retains references.
            unsafe { pc2mqtt_events_dispatch(self.pointer.as_ptr()) }
        }
    }
    impl AsRawFd for Listener {
        fn as_raw_fd(&self) -> RawFd {
            unsafe { pc2mqtt_events_fd(self.pointer.as_ptr()) }
        }
    }
    impl Drop for Listener {
        fn drop(&mut self) {
            unsafe { pc2mqtt_events_free(self.pointer.as_ptr()) }
        }
    }
}

#[cfg(all(test, feature = "pipewire"))]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        process::{Child, Command, Stdio},
        time::Duration,
    };
    struct Server {
        directory: PathBuf,
        child: Child,
    }
    impl Server {
        async fn start() -> Self {
            let directory =
                std::env::temp_dir().join(format!("pc2mqtt-pipewire-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("pipewire.conf"), r"context.properties = { core.daemon = true core.name = pc2mqtt-fixture context.num-data-loops = 0 }
context.spa-libs = { support.* = support/libspa-support audio.convert.* = audioconvert/libspa-audioconvert }
context.modules = [
 { name = libpipewire-module-protocol-native }
 { name = libpipewire-module-access }
 { name = libpipewire-module-metadata }
 { name = libpipewire-module-spa-node-factory }
 { name = libpipewire-module-adapter }
]
context.objects = [
 { factory = metadata args = { metadata.name = default } }
 { factory = adapter args = { factory.name = support.null-audio-sink node.name = fixture-speaker1 media.class = Audio/Sink audio.position = [ FL FR ] } }
 { factory = adapter args = { factory.name = support.null-audio-sink node.name = fixture-speaker2 media.class = Audio/Sink audio.position = [ FL FR ] } }
]
").unwrap();
            let child = Command::new("pipewire")
                .arg("-c")
                .arg(directory.join("pipewire.conf"))
                .env("PIPEWIRE_RUNTIME_DIR", &directory)
                .env("XDG_RUNTIME_DIR", &directory)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let server = Self { directory, child };
            tokio::time::timeout(Duration::from_secs(3), async {
                while !server.socket().exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("isolated PipeWire server did not start");
            server
        }
        fn socket(&self) -> PathBuf {
            self.directory.join("pc2mqtt-fixture")
        }
        fn command(&self, program: &str, args: &[&str]) -> String {
            let result = Command::new(program)
                .arg("-r")
                .arg(self.socket())
                .args(args)
                .env("PIPEWIRE_RUNTIME_DIR", &self.directory)
                .env("XDG_RUNTIME_DIR", &self.directory)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            String::from_utf8(result.stdout).unwrap()
        }
        fn default(&self, name: &str) {
            self.command(
                "pw-metadata",
                &[
                    "-n",
                    "default",
                    "0",
                    "default.audio.sink",
                    &format!(r#"{{"name":"{name}"}}"#),
                    "Spa:String:JSON",
                ],
            );
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.child.kill().ok();
            self.child.wait().ok();
            fs::remove_dir_all(&self.directory).ok();
        }
    }
    async fn change(events: &mut Events) -> i32 {
        tokio::time::timeout(Duration::from_secs(3), events.next())
            .await
            .expect("missing PipeWire event")
    }
    #[tokio::test(flavor = "current_thread")]
    async fn native_events_follow_volume_default_speaker_and_server_loss_without_threads() {
        let mut server = Server::start().await;
        server.default("fixture-speaker1");
        let listener = native::Listener::open(server.socket().as_os_str()).unwrap();
        let mut events = Events::Native(tokio::io::unix::AsyncFd::new(listener).unwrap());
        assert_eq!(change(&mut events).await & RESET, RESET);
        while tokio::time::timeout(Duration::from_millis(100), events.next())
            .await
            .is_ok()
        {}
        let nodes = server.command("pw-cli", &["ls", "Node"]);
        let ids: Vec<_> = nodes
            .lines()
            .filter_map(|line| line.trim().strip_prefix("id "))
            .map(|line| line.split(',').next().unwrap().to_owned())
            .collect();
        assert_eq!(ids.len(), 2);
        // The other speaker's volume must not wake the audio adapter.
        server.command(
            "pw-cli",
            &[
                "set-param",
                &ids[1],
                "Props",
                "{ channelVolumes = [ 0.2 0.2 ] }",
            ],
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(150), events.next())
                .await
                .is_err()
        );
        server.command(
            "pw-cli",
            &[
                "set-param",
                &ids[0],
                "Props",
                "{ channelVolumes = [ 0.4 0.4 ] }",
            ],
        );
        assert_eq!(change(&mut events).await, CHANGED);
        server.command(
            "pw-cli",
            &["set-param", &ids[0], "Props", "{ mute = true }"],
        );
        assert_eq!(change(&mut events).await, CHANGED);
        server.default("fixture-speaker2");
        assert_ne!(change(&mut events).await & RESET, 0);
        // This exact process has one Tokio thread and one native loop. Libraries
        // must not quietly introduce realtime, GLib, or PipeWire data threads.
        // Other Rust tests run concurrently, so thread count is measured in the
        // separate release benchmark instead of asserted process-wide here.
        server.child.kill().unwrap();
        server.child.wait().unwrap();
        assert_ne!(change(&mut events).await & DISCONNECTED, 0);
    }
}
