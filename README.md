# discord-mute-rs

Local Actix Web API for controlling Discord mute and DualSense mic-button helpers from curl.

Features:

- Save Discord application credentials.
- Authorize Discord RPC and cache the OAuth token.
- Toggle Discord mute once.
- Run continuous DualSense mic-button mute toggle mode, keeping the
  controller's mic LED in sync.
- Survive the controller disconnecting and reconnecting while running.
- Inspect status, live over a WebSocket.

Stop the mic-button listener by stopping the server (Ctrl-C).

## Run

```bash
cargo run
```

The API listens on:

```text
http://127.0.0.1:3219
```

Override the address with:

```bash
DISCORD_MUTE_API_ADDR=127.0.0.1:3220 cargo run
```

Port `0` binds an ephemeral port, for a supervising app that would rather not
race whatever already holds the default. The chosen address is printed on the
first line of stdout, before any other output, so a parent process can parse it:

```text
DISCORD_MUTE_API_LISTENING=127.0.0.1:53538
```

## First-Time Setup

Create or open a Discord application at:

```text
https://discord.com/developers/applications
```

Its OAuth2 redirect URI must include:

```text
http://localhost
```

Then save the client keys:

```bash
curl -X PUT http://127.0.0.1:3219/config \
  -H 'content-type: application/json' \
  -d '{"clientId":"your_client_id","clientSecret":"your_client_secret"}'
```

Config is written to:

```text
$XDG_CONFIG_HOME/discord-mute-rs/config.json
```

If `XDG_CONFIG_HOME` is unset, the server uses:

```text
~/.config/discord-mute-rs/config.json
```

You can also provide credentials with environment variables:

```bash
DISCORD_CLIENT_ID=your_id DISCORD_CLIENT_SECRET=your_secret cargo run
```

## Curl API

Status:

```bash
curl http://127.0.0.1:3219/status
```

Config/token paths:

```bash
curl http://127.0.0.1:3219/config
```

Toggle Discord mute once:

```bash
curl -X POST http://127.0.0.1:3219/discord/toggle
```

The first toggle may ask Discord desktop for authorization. New tokens are saved to:

```text
$XDG_CONFIG_HOME/discord-mute-rs/token.json
```

Override that path with:

```bash
TOKEN_FILE=/Users/emil.laskowski/Documents/files/token.json cargo run
```

Start continuous mute toggle mode with the documented DualSense mic button:

```bash
curl -X POST http://127.0.0.1:3219/listeners/mute
```

This succeeds even with no controller attached — the listener waits for one to
appear, and keeps running across disconnects. `/status` reports whether a
controller is actually connected right now:

```json
{
  "muted": false,
  "controllerConnected": true,
  "listener": { "running": true, "lastError": null }
}
```

Stop the listener:

```bash
curl -X DELETE http://127.0.0.1:3219/listeners/current
```

Stopping the server (Ctrl-C) also stops it.

## Controller Notes

The documented DualSense mic-button mappings are:

```text
USB report 0x01: byte 10, mask 0x04
Bluetooth full report 0x31: byte 11, mask 0x04
```

Over Bluetooth the controller boots into a compatibility mode that only sends a
10-byte report 0x01 — sticks and face buttons, no mic button. The server reads
feature report 0x05 (calibration) on open, which is the documented side effect
that makes it start sending the full 78-byte report 0x31 that carries the mic
button. Without that, the mic button is invisible.

That request is re-issued on every reconnect, because a controller that comes
back is once again in simple mode. Reconnecting also re-scans the HID device
list — `hidapi` caches it, so a reconnected controller resolves to a new path —
and restores the mic LED to the current mute state, which the controller drops
when it disconnects.

The LED output uses the documented/common DualSense output fields:

```text
mute_button_led
power_save_control bit 4
Bluetooth CRC32 seed 0xa2
```

Sources:

```text
https://nondebug.github.io/dualsense/
https://codebrowser.dev/linux/linux/drivers/hid/hid-playstation.c.html
```

## macOS Notes

If HID reads fail or no controller appears, allow Terminal, iTerm, or your launcher in:

```text
System Settings > Privacy & Security > Input Monitoring
```

Then fully quit and reopen the terminal/app before trying again.
