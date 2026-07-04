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
    "host": "YOUR MQTT HOST",
    "port": 1883,
    "username": "MQTT USER",
    "password": "MQTT PASSWORD",
    "auto_discovery_prefix": "homeassistant"
  }
}
```

## Build

```sh
cargo build --release
```

`--version` prints the embedded git SHA.
