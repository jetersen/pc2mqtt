# pc2mqtt

Control your PC or homeserver from Home Assistant through MQTT.

## Exposed MQTT components

- Power sensor
- Shutdown button
- Reboot button
- Optional desktop speaker volume control
- Comin deployment status, deployed revision, and reboot-required sensor (when updates are enabled)

![homeassistant](.github/images/homeassistant.png)

## Linux install

1. Build or download the `pc2mqtt` binary.
2. Copy it to `/opt/pc2mqtt/pc2mqtt`.
3. Run it with systemd.

Example service:

```ini
[Unit]
Description=PC2MQTT
After=network-online.target

[Service]
ExecStart=/opt/pc2mqtt/pc2mqtt
Restart=always
RuntimeDirectory=pc2mqtt
RuntimeDirectoryMode=0700

[Install]
WantedBy=multi-user.target
```

## Optional speaker volume control

Set `PC2MQTT_AUDIO_COMMAND` to an absolute executable path to expose a speaker-volume
number through MQTT discovery. The adapter must run in the desktop user's audio
session. The agent times out helpers after two seconds. It accepts `read`, `set PERCENT`,
`limit PERCENT`, and `limit-unmute PERCENT`, returning `wpctl get-volume` output.
Limits must never raise quieter output; `limit-unmute` must cap before unmuting.
Audio reads run every five seconds to detect changes made outside MQTT. Unchanged
readings are not republished. Reconnects refresh the state and clear previous cap
confirmations. A failed, expired, or contradicted confirmation cannot become valid
again without a new limit request.

The agent rejects retained audio commands. Correlated, expiring limit requests on
`<device_name>/audio/volume/limit` provide confirmation before another automation
starts playback. Permit the discovery, state, set, and limit topics in the broker
ACL. See [the audio protocol](src/audio.rs) for payload fields and validation.

## Config

On first start, pc2mqtt creates `/etc/pc2mqtt/config.json` unless `PC2MQTT_CONFIG` points elsewhere.

```json
{
  "device_id": "63fbeebb-f107-4903-ab36-6104b9d802b0",
  "device_name": "my-hostname",
  "mqtt": {
    "url": "wss://mqtt.jetersen.dev/socket",
    "username": "MQTT USER",
    "password": "MQTT PASSWORD",
    "auto_discovery_prefix": "homeassistant"
  }
}
```

`mqtt.url` supports `mqtt://`, `mqtts://`, `ws://`, and `wss://`. TLS uses the
system trust store and verifies the broker certificate and hostname. Existing
`host` and `port` configurations continue to use plain TCP.

Commands require the payload `PRESS`. Retained command replays are ignored.
Publish commands without retention; MQTT can forward a newly retained publication
to an existing subscriber without marking it as retained.

On NixOS hosts, set top-level `"updates": true` to enable
`<device_name>/button/update/command`. pc2mqtt requests a Comin fetch immediately
over `/var/lib/comin/grpc.sock` when the daemon is unsuspended and idle. Set
`PC2MQTT_COMIN_SOCKET` to override this path. MQTT, the gRPC event stream, and audio helper I/O share one current-thread Tokio
runtime. Comin waits for events and never polls readiness.
Completion events get one brief settling delay because Comin emits some events
before clearing its busy flags. Requests do not spawn additional workers. A stopped
or suspended Comin stays that way until explicitly started or resumed. A settled failed generation
causes an active Comin daemon to restart before fetching the unchanged commit.

QoS 1 update commands remain queued after fetching until the selected generation
is active, or Comin has staged a boot-only deployment. If an older generation is
running but Comin remembers deploying the selected commit, the worker resubmits
that deployment through gRPC. It preserves the deployment operation and never
bypasses suspension or reboot-only inhibitors. A new commit waits for its own
build rather than resubmitting an older deployment. Deployment telemetry stays
`updating` while a request is pending, unless Comin reports failure or suspension.

Persistent MQTT sessions allow unacknowledged commands to be redelivered after
reconnecting. Startup and broker connections also request an update, covering
missed notifications. Branch-selection errors, including rejected rewritten history,
keep requests queued and report failure even when the Git transport succeeds.
Retained command replays remain ignored.

`pc2mqtt --request-update` queues a local request through
`/run/pc2mqtt/update.sock`, or `PC2MQTT_UPDATE_SOCKET` if set. The socket permits
access only to the service account. Use this for manual checks and resume/network
hooks. Use a systemd path unit watching Comin's socket with `PathChanged` to invoke
it when Comin starts or replaces the socket. Socket failures and failed fetches wait
for another lifecycle, MQTT, or Comin event; there is no periodic retry. The service
needs access to Comin's socket and `systemctl` on `PATH` solely to restart an active
daemon after a settled generation failure (Comin 0.14 has no retry-generation RPC).
Leave updates disabled on hosts without Comin. Restrict command publishers with broker ACLs.

When updates are enabled, Home Assistant discovers three diagnostic entities on the
existing device. Retained JSON on `<device_name>/deployment/state` contains
`state` (`idle`, `updating`, `suspended`, `failed`, or `unavailable`),
`deployed_revision`, `booted_revision`, and `reboot_required`. Revisions refer to
Comin's actual switched and booted deployments, not the pending build. Unknown
revisions or reboot requirements are `null`. The existing Comin event stream
refreshes this snapshot without polling, and MQTT reconnects republish it. Host
availability uses the existing last will; Comin disconnections mark deployment
telemetry unavailable. Allow the state topic and the three discovery topics in
broker ACLs alongside the existing power entities.

## Build

```sh
cargo build --release
```

`--version` prints the embedded git SHA.

For an isolated Linux idle benchmark with a mock broker, Comin, and audio adapter:

```sh
cargo test --release --target x86_64-unknown-linux-gnu --test mqtt_updates benchmark_idle -- --ignored --nocapture
```

It samples `/proc` at each window's boundaries, reporting RSS/PSS/private memory
in KiB, reserved virtual size, thread count, CPU ticks (use `getconf CLK_TCK`),
minor faults, and aggregate thread context switches. It also counts MQTT packets
and fixture helper calls. Connected and reconnected windows last 15 seconds;
broker waiting and connection-refused windows last five seconds. Audio is enabled
by default; set `PC2MQTT_BENCH_ALL=1` to also measure it disabled. The mock broker blocks between packets and
responds to keepalives. `PC2MQTT_TEST_BINARY` selects a saved release binary for
comparisons. Context switches are a wakeup proxy, not a hardware wakeup counter.
