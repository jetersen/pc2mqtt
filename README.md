# pc2mqtt

Control your PC or homeserver from Home Assistant through MQTT.

## Exposed MQTT components

- Power sensor
- Shutdown button
- Reboot button

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
when the daemon is active, unsuspended, and idle. Pending requests check readiness
once per second. One shared update worker blocks when idle; requests do not spawn
additional workers. A stopped or suspended Comin
stays that way until explicitly started or resumed. A settled failed generation
causes an active Comin daemon to restart before fetching the unchanged commit.

QoS 1 update commands are acknowledged after Comin successfully fetches. Persistent
MQTT sessions allow unacknowledged commands to be redelivered after reconnecting.
Startup and broker connections also request a fetch, covering missed notifications.
Retained command replays remain ignored.

`pc2mqtt --request-update` queues a local request through
`/run/pc2mqtt/update.sock`, or `PC2MQTT_UPDATE_SOCKET` if set. The socket permits
access only to the service account. Use this for manual checks and resume/network
hooks. The service needs `comin`, `systemctl`, and `timeout` on `PATH`, with permission
to query/fetch Comin and restart its active service after a failure. Leave updates
disabled on hosts without Comin. Restrict command publishers with broker ACLs.

## Build

```sh
cargo build --release
```

`--version` prints the embedded git SHA.
