# discord-mute-rs

Local Actix Web API for controlling Discord mute and DualSense mic-button helpers from curl.

The server keeps the original app features:

- Save Discord application credentials.
- Authorize Discord RPC and cache the OAuth token.
- Toggle Discord mute once.
- List Sony HID devices.
- Test the DualSense mic mute LED.
- Run continuous DualSense mic-button mute toggle mode.
- Run continuous Push-to-Talk mode using macOS Right Option.
- Inspect status and stop the server.

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

List Sony HID devices:

```bash
curl http://127.0.0.1:3219/devices
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

Test the DualSense mic LED:

```bash
curl -X POST http://127.0.0.1:3219/controller/led \
  -H 'content-type: application/json' \
  -d '{"muted":true}'
```

Start continuous mute toggle mode with the documented DualSense mic button:

```bash
curl -X POST http://127.0.0.1:3219/listeners/mute
```

Start Push-to-Talk mode:

```bash
curl -X POST http://127.0.0.1:3219/listeners/ptt
```

Check the current listener:

```bash
curl http://127.0.0.1:3219/listeners/current
```

Stop the current listener:

```bash
curl -X DELETE http://127.0.0.1:3219/listeners/current
```

Stop the server:

```bash
curl -X POST http://127.0.0.1:3219/quit
```

## Controller Notes

The documented DualSense mic-button mappings are:

```text
USB report 0x01: byte 10, mask 0x04
Bluetooth full report 0x31: byte 11, mask 0x04
```

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
System Settings > Privacy & Security > Accessibility
```

Then fully quit and reopen the terminal/app before trying again.

Push-to-Talk mode also needs Accessibility permission because it posts a synthetic Right Option key down/up event.
