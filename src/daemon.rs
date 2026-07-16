use crate::config;
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const DEFAULT_API_ADDR: &str = "127.0.0.1:3219";

#[derive(Clone, Copy)]
pub enum DaemonMode {
    Mute,
    PushToTalk,
}

impl DaemonMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mute => "mute",
            Self::PushToTalk => "ptt",
        }
    }

    fn serve_command(self) -> &'static str {
        match self {
            Self::Mute => "serve",
            Self::PushToTalk => "serve-ptt",
        }
    }
}

pub fn start(mode: DaemonMode) -> Result<()> {
    if let Ok(status) = request("GET", "/status") {
        bail!("discord-mute-rs already appears to be running:\n{status}");
    }

    config::load_config().with_context(|| {
        format!("daemon setup check failed; run `cargo run -- setup` before `cargo run -- start`")
    })?;

    std::fs::create_dir_all(config::config_dir()).with_context(|| {
        format!(
            "failed to create config directory at {}",
            config::config_dir().display()
        )
    })?;

    let log_path = config::config_dir().join("daemon.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log at {}", log_path.display()))?;
    let log_for_stderr = log
        .try_clone()
        .context("failed to clone daemon log handle")?;

    let exe = std::env::current_exe().context("failed to find current executable path")?;
    let mut command = Command::new(exe);
    command
        .arg(mode.serve_command())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_for_stderr));

    detach_command(&mut command);

    let child = command
        .spawn()
        .with_context(|| format!("failed to start detached {} daemon", mode.label()))?;

    println!(
        "Started detached {} daemon with pid {}.",
        mode.label(),
        child.id()
    );
    println!("API: http://{}", api_addr());
    println!("Log: {}", log_path.display());

    wait_until_ready(&log_path)?;

    Ok(())
}

pub fn status() -> Result<()> {
    let response = request("GET", "/status")?;
    println!("{response}");
    Ok(())
}

pub fn quit() -> Result<()> {
    let response = request("POST", "/quit")?;
    println!("{response}");
    Ok(())
}

pub fn run_control_api(mode: DaemonMode, started_at: Instant) -> Result<()> {
    let listener = TcpListener::bind(api_addr())
        .with_context(|| format!("failed to bind control API at {}", api_addr()))?;
    println!("Control API listening at http://{}", api_addr());

    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    if let Err(err) = handle_connection(&mut stream, mode, started_at) {
                        eprintln!("Control API error: {err}");
                    }
                }
                Err(err) => eprintln!("Control API accept error: {err}"),
            }
        }
    });

    Ok(())
}

fn handle_connection(stream: &mut TcpStream, mode: DaemonMode, started_at: Instant) -> Result<()> {
    let mut request = [0_u8; 1024];
    let read = stream
        .read(&mut request)
        .context("failed to read control API request")?;
    let request = String::from_utf8_lossy(&request[..read]);
    let first_line = request.lines().next().unwrap_or_default();

    if first_line.starts_with("GET /status ") {
        let uptime = Instant::now().duration_since(started_at).as_secs();
        let body = format!(
            "{{\"ok\":true,\"mode\":\"{}\",\"pid\":{},\"uptimeSeconds\":{},\"api\":\"{}\"}}\n",
            mode.label(),
            std::process::id(),
            uptime,
            api_addr()
        );
        write_response(stream, 200, "OK", &body)?;
        return Ok(());
    }

    if first_line.starts_with("POST /quit ") {
        write_response(
            stream,
            200,
            "OK",
            "{\"ok\":true,\"message\":\"quitting\"}\n",
        )?;
        thread::spawn(|| {
            thread::sleep(Duration::from_millis(100));
            std::process::exit(0);
        });
        return Ok(());
    }

    write_response(
        stream,
        404,
        "Not Found",
        "{\"ok\":false,\"error\":\"not found\"}\n",
    )
}

fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .context("failed to write control API response")
}

fn request(method: &str, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(api_addr())
        .with_context(|| format!("failed to connect to control API at {}", api_addr()))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        api_addr()
    )
    .context("failed to write control API request")?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("failed to read control API response")?;

    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("invalid control API response: {response}"))?;

    if !headers.starts_with("HTTP/1.1 200 ") {
        bail!("control API returned an error:\n{body}");
    }

    Ok(body.trim().to_string())
}

fn wait_until_ready(log_path: &std::path::Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_error = None;

    while Instant::now() < deadline {
        match request("GET", "/status") {
            Ok(status) => {
                println!("Daemon is ready: {status}");
                return Ok(());
            }
            Err(err) => {
                last_error = Some(err);
                thread::sleep(Duration::from_millis(150));
            }
        }
    }

    let detail = last_error
        .map(|err| err.to_string())
        .unwrap_or_else(|| "no status response".to_string());
    let log_tail =
        read_log_tail(log_path).unwrap_or_else(|err| format!("could not read log: {err}"));
    bail!(
        "daemon did not become ready within 5 seconds ({detail}).\nLog: {}\n\n{}",
        log_path.display(),
        log_tail
    );
}

fn read_log_tail(path: &std::path::Path) -> Result<String> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read daemon log at {}", path.display()))?;
    let lines = contents.lines().rev().take(40).collect::<Vec<_>>();
    let tail = lines.into_iter().rev().collect::<Vec<_>>().join("\n");

    if tail.trim().is_empty() {
        Ok("(daemon log is empty)".to_string())
    } else {
        Ok(tail)
    }
}

fn api_addr() -> String {
    std::env::var("DISCORD_MUTE_API_ADDR").unwrap_or_else(|_| DEFAULT_API_ADDR.to_string())
}

#[cfg(unix)]
fn detach_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            unsafe extern "C" {
                fn setsid() -> i32;
            }

            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_command(_command: &mut Command) {}
