# pc2mqtt

Control your PC or homeserver from Home Assistant through MQTT.

## Exposed MQTT components

- Power sensor
- Shutdown button
- Reboot button
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
`PC2MQTT_COMIN_SOCKET` to override this path. The gRPC event stream wakes one shared
update worker; it uses a single-thread async runtime and never polls readiness.
Completion events get one brief settling delay because Comin emits some events
before clearing its busy flags. Requests do not spawn additional workers. A stopped
or suspended Comin stays that way until explicitly started or resumed. A settled failed generation
causes an active Comin daemon to restart before fetching the unchanged commit.

QoS 1 update commands are acknowledged after Comin successfully fetches. Persistent
MQTT sessions allow unacknowledged commands to be redelivered after reconnecting.
Startup and broker connections also request a fetch, covering missed notifications.
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
