use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    env,
    io::{ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    thread,
    time::Duration,
};
use uuid::Uuid;

const OPCODE_HANDSHAKE: i32 = 0;
const OPCODE_FRAME: i32 = 1;
const OPCODE_CLOSE: i32 = 2;
const OPCODE_PING: i32 = 3;
const OPCODE_PONG: i32 = 4;

pub struct DiscordIpc {
    stream: UnixStream,
}

impl DiscordIpc {
    pub fn connect(client_id: impl Into<String>) -> Result<Self> {
        let client_id = client_id.into();
        let mut stream = connect_to_discord_ipc()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .context("failed to set Discord IPC read timeout")?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .context("failed to set Discord IPC write timeout")?;

        write_frame(
            &mut stream,
            OPCODE_HANDSHAKE,
            &json!({
                "v": 1,
                "client_id": client_id.clone(),
            }),
        )?;

        let mut client = Self { stream };
        let ready = client.read_next_frame()?;
        if ready.get("evt").and_then(Value::as_str) != Some("READY") {
            bail!("Discord IPC did not return READY after handshake: {ready}");
        }

        Ok(client)
    }

    pub fn authenticate(&mut self, access_token: &str) -> Result<()> {
        let response = self.request(json!({
            "cmd": "AUTHENTICATE",
            "args": {
                "access_token": access_token,
            },
        }))?;

        if response.get("evt").and_then(Value::as_str) == Some("ERROR") {
            return Err(discord_error(&response));
        }

        Ok(())
    }

    pub fn authorize(&mut self, client_id: &str, scopes: &[&str]) -> Result<String> {
        let response = self.request(json!({
            "cmd": "AUTHORIZE",
            "args": {
                "client_id": client_id,
                "scopes": scopes,
            },
        }))?;

        if response.get("evt").and_then(Value::as_str) == Some("ERROR") {
            return Err(discord_error(&response));
        }

        response
            .get("data")
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow!("Discord AUTHORIZE response did not contain data.code: {response}")
            })
    }

    pub fn toggle_mute(&mut self) -> Result<bool> {
        let settings = self.request(json!({
            "cmd": "GET_VOICE_SETTINGS",
            "args": {},
        }))?;

        if settings.get("evt").and_then(Value::as_str) == Some("ERROR") {
            return Err(discord_error(&settings));
        }

        let currently_muted = settings
            .get("data")
            .and_then(|data| data.get("mute"))
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("Discord response did not contain data.mute: {settings}"))?;

        let mute = !currently_muted;
        println!(
            "Discord reports current mute={}, setting mute={}",
            currently_muted, mute
        );
        let response = self.request(json!({
            "cmd": "SET_VOICE_SETTINGS",
            "args": {
                "mute": mute,
            },
        }))?;

        if response.get("evt").and_then(Value::as_str) == Some("ERROR") {
            return Err(discord_error(&response));
        }

        println!("Discord accepted SET_VOICE_SETTINGS.");
        Ok(mute)
    }

    pub fn close(&mut self) -> Result<()> {
        write_frame(&mut self.stream, OPCODE_CLOSE, &json!({}))
    }

    fn request(&mut self, mut payload: Value) -> Result<Value> {
        let nonce = Uuid::new_v4().to_string();
        payload["nonce"] = Value::String(nonce.clone());
        write_frame(&mut self.stream, OPCODE_FRAME, &payload)?;

        loop {
            let frame = self.read_next_frame()?;
            if frame.get("nonce").and_then(Value::as_str) == Some(nonce.as_str()) {
                return Ok(frame);
            }
        }
    }

    fn read_next_frame(&mut self) -> Result<Value> {
        loop {
            let (opcode, value) = read_frame(&mut self.stream)?;
            match opcode {
                OPCODE_FRAME => return Ok(value),
                OPCODE_PING => write_frame(&mut self.stream, OPCODE_PONG, &value)?,
                OPCODE_CLOSE => bail!("Discord closed the IPC connection: {value}"),
                other => bail!("received unsupported Discord IPC opcode {other}: {value}"),
            }
        }
    }
}

impl Drop for DiscordIpc {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn connect_to_discord_ipc() -> Result<UnixStream> {
    let prefix = env::var_os("XDG_RUNTIME_DIR")
        .or_else(|| env::var_os("TMPDIR"))
        .or_else(|| env::var_os("TMP"))
        .or_else(|| env::var_os("TEMP"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    let mut last_error = None;
    for id in 0..10 {
        let path = prefix.join(format!("discord-ipc-{id}"));
        match UnixStream::connect(&path) {
            Ok(stream) => return Ok(stream),
            Err(err) => last_error = Some((path, err)),
        }
    }

    if let Some((path, err)) = last_error {
        bail!(
            "could not connect to Discord IPC near {}; is Discord desktop running? ({err})",
            path.display()
        );
    }

    bail!("could not connect to Discord IPC; is Discord desktop running?");
}

fn write_frame(stream: &mut UnixStream, opcode: i32, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value).context("failed to encode Discord IPC JSON")?;
    let mut frame = Vec::with_capacity(8 + body.len());
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(&(body.len() as i32).to_le_bytes());
    frame.extend_from_slice(&body);
    stream
        .write_all(&frame)
        .context("failed to write Discord IPC frame")
}

fn read_frame(stream: &mut UnixStream) -> Result<(i32, Value)> {
    let mut header = [0_u8; 8];
    read_exact_retry(stream, &mut header).context("failed to read Discord IPC frame header")?;

    let opcode = i32::from_le_bytes(header[0..4].try_into().expect("4-byte opcode"));
    let len = i32::from_le_bytes(header[4..8].try_into().expect("4-byte length"));
    if len < 0 {
        bail!("Discord IPC frame had a negative length: {len}");
    }

    let mut body = vec![0_u8; len as usize];
    read_exact_retry(stream, &mut body).context("failed to read Discord IPC frame body")?;

    let value = serde_json::from_slice(&body).context("failed to decode Discord IPC JSON")?;
    Ok((opcode, value))
}

fn read_exact_retry(stream: &mut UnixStream, mut buffer: &mut [u8]) -> Result<()> {
    while !buffer.is_empty() {
        match stream.read(buffer) {
            Ok(0) => bail!("Discord IPC socket closed"),
            Ok(n) => {
                let (_, rest) = buffer.split_at_mut(n);
                buffer = rest;
            }
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err).context("failed to read from Discord IPC socket"),
        }
    }

    Ok(())
}

fn discord_error(response: &Value) -> anyhow::Error {
    let message = response
        .get("data")
        .and_then(|data| data.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Discord returned an RPC error");

    let code = response
        .get("data")
        .and_then(|data| data.get("code"))
        .and_then(Value::as_i64);

    if let Some(code) = code {
        return anyhow!("{message} (code {code})");
    }

    anyhow!("{message}")
}
