# pc2mqtt

Control your PC or homeserver from Home Assistant through MQTT.

## Exposed MQTT components

- Power sensor
- Shutdown button
- Reboot button
- Optional desktop speaker volume control
- Last system upgrade result (when updates are enabled)

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
session. The agent times out helpers after two seconds and limits stdout to
256 bytes. It accepts `read`, `set PERCENT`,
`limit PERCENT`, and `limit-unmute PERCENT`, returning `wpctl get-volume` output.
Limits must never raise quieter output; `limit-unmute` must cap before unmuting.
Audio reads run when MQTT connects, on commands, or when a non-retained `GET`
is sent to `<device_name>/audio/volume/get`. Each `GET` publishes a fresh reply on
`<device_name>/audio/volume/state`, even when the value is unchanged. There is no
polling, background audio subscription, or native PipeWire dependency in the
agent. Changes made outside MQTT appear on the next read.

For music preparation, publish non-retained JSON to
`<device_name>/audio/music/set` containing a fresh `request_id` and `expires_at`
(Unix seconds within the next 60 seconds).
The built-in profile atomically caps speaker volume at 50% without raising quieter
output, then unmutes. The reply on `<device_name>/audio/volume/state` carries the
same `request_id`; `limit_ok: true` means readback confirmed both steps. A failure
replies with `limit_ok: false`. Wait for the matching successful reply before
starting playback. This profile controls host speakers; a media player's separate
software volume remains the caller's responsibility.

Ordinary `set` commands and correlated requests on `<device_name>/audio/volume/limit`
remain supported. The agent rejects retained audio commands. Reconnects clear
previous confirmations. Failed, expired, or contradicted confirmations cannot
become valid again without a new request. Expiry uses a one-shot timer, without an
audio read. Permit the discovery, state, set, get, limit, and music topics in the
broker ACL. See [the audio protocol](src/audio.rs) for validation.

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
`<device_name>/button/update/command`. A non-retained `PRESS` starts
`nixos-upgrade.service` through `systemctl --no-block`. The service owns the job,
so upgrading or restarting pc2mqtt does not interrupt it. systemd coalesces starts
while the service is active. Commands use ordinary MQTT acknowledgements; an
acknowledgement means receipt, not that the upgrade completed.

Use NixOS's `system.autoUpgrade` module to configure the service. No Comin daemon,
gRPC connection, or progress polling is needed. pc2mqtt checks for updates when
MQTT connects. `pc2mqtt --request-update` sends the same request through the private
`/run/pc2mqtt/update.sock`; `PC2MQTT_UPDATE_SOCKET` overrides its location.
`PC2MQTT_UPDATE_COMMAND` can select a launcher executable for another installation;
it receives `--no-block start nixos-upgrade.service`. Launchers time out after two
seconds; the upgrade itself runs independently under systemd.

Set the upgrade unit's `ExecStopPost` to `/path/to/pc2mqtt --report-update-result`.
This reads systemd's `SERVICE_RESULT` and `EXIT_STATUS`, atomically saves the last
completed result in `/var/lib/pc2mqtt/update-result.json`, and wakes the MQTT agent.
Set `StateDirectory=pc2mqtt` with mode `0700` on both services. Override the result
path with `PC2MQTT_UPDATE_RESULT` if needed. Reporting also works while the agent
is stopped. The latest result is published when MQTT reconnects.

Home Assistant discovers the Deployment result sensor. Retained JSON on
`<device_name>/deployment/state` contains `state` (`idle` on success, `failed`
on failure), `result` (systemd's outcome), `exit_status`, and `completed_at`
(Unix seconds). Launcher failures use `result: "launch-failed"`. Allow this topic
in the broker ACL and use a Home Assistant automation to notify your phone when
`state` is `failed`. Retained results replay after reconnects; `completed_at` can
identify the same result when deduplicating notifications. Detailed logs remain
in `journalctl -u nixos-upgrade.service`.

The former Comin revision and reboot-required entities are removed from discovery.
Audio cap acknowledgements remain correlated and expiring independently of upgrades.

## Build

```sh
cargo build --release
```

`--version` prints the embedded git SHA.

For an isolated Linux idle benchmark with a mock broker, upgrade launcher, and audio adapter:

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

For 30 correlated music-preparation round trips through the mock adapter:

```sh
cargo test --release --target x86_64-unknown-linux-gnu --test mqtt_updates benchmark_audio_requests -- --ignored --nocapture
```

This reports median and 95th-percentile latency and helper calls. It measures the
MQTT command path with a fixed-output adapter, without changing real audio.
