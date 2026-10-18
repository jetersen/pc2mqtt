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
`<device_name>/button/update/command`. This starts the fixed
`comin-fetch.service` unit, and also requests an update on every broker connection.
The service account needs permission to start that unit. Leave this disabled on
hosts without Comin. Use broker ACLs to restrict publishers to the required topics.

## Build

```sh
cargo build --release
```

`--version` prints the embedded git SHA.
