use crate::{config, controller, discord_mute, token};
use actix_files::Files;
use actix_web::{App, HttpRequest, HttpResponse, HttpServer, Responder, dev::ServerHandle, web};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

const DEFAULT_API_ADDR: &str = "127.0.0.1:3219";
const STATIC_DIR: &str = "./static";
/// Prefix of the handshake line a supervising process parses out of our stdout
/// to learn which address we actually bound, which it cannot know when it asks
/// for an ephemeral port.
const LISTENING_PREFIX: &str = "DISCORD_MUTE_API_LISTENING=";
/// Set by a supervising process to ask us to exit if it dies. macOS has no
/// equivalent of Linux's `PR_SET_PDEATHSIG`, so we poll instead.
const EXIT_WITH_PARENT_VAR: &str = "DISCORD_MUTE_EXIT_WITH_PARENT";
/// How often the orphan watchdog checks whether its parent is still alive.
const PARENT_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// How long an orphaned server waits for a graceful shutdown before exiting
/// outright.
const ORPHAN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
// How often we push a fresh snapshot to connected UIs, in addition to
// pushing immediately after every mutating request.
const BROADCAST_TICK: Duration = Duration::from_millis(500);

pub async fn serve() -> std::io::Result<()> {
    // Bind before building state so the real port is known up front. That
    // matters when the address ends in `:0`: a supervising app can ask for an
    // ephemeral port instead of racing whatever already holds the default.
    let socket = bind_api_socket()?;
    let addr = socket.local_addr()?.to_string();

    let (events_tx, _) = broadcast::channel::<String>(32);

    let state = web::Data::new(ApiState {
        started_at: Instant::now(),
        bound_addr: addr.clone(),
        listener: Mutex::new(None),
        last_muted: Mutex::new(None),
        controller_connected: AtomicBool::new(false),
        controller_error: Mutex::new(None),
        controller_battery: Mutex::new(None),
        discord: Mutex::new(None),
        events: events_tx,
        server_handle: Mutex::new(None),
    });

    // Background ticker: keeps the UI in sync even when state changes
    // originate outside an HTTP request (e.g. a DualSense button press
    // flips mute from a background thread).
    {
        let state = state.clone();
        actix_web::rt::spawn(async move {
            let mut interval = actix_web::rt::time::interval(BROADCAST_TICK);
            loop {
                interval.tick().await;
                broadcast_snapshot(&state);
            }
        });
    }

    // Machine-readable first, for a supervising process reading our stdout.
    // Rust's stdout is line buffered, but flush explicitly: this line is a
    // handshake, and a parent blocking on it must not wait on a buffer.
    println!("{LISTENING_PREFIX}{addr}");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    println!("Discord mute API listening at http://{addr}");
    println!("Try: curl http://{addr}/status");
    println!("UI:  http://{addr}/");

    // Keep a reference outside the factory closure, which takes ownership of
    // the one above, so the server handle can be stored after construction.
    let state_for_handle = state.clone();

    let server = HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/status", web::get().to(status))
            .route("/config", web::get().to(get_config))
            .route("/config", web::put().to(save_config))
            .route("/discord/toggle", web::post().to(toggle))
            .route("/listeners/mute", web::post().to(start_mute_listener))
            .route("/listeners/current", web::delete().to(stop_listener))
            .route("/ws", web::get().to(ws_index))
            .route("/quit", web::post().to(quit))
            // React build output. Keep this last so it never shadows the
            // API routes above; it only matches what nothing else claimed.
            .service(Files::new("/", STATIC_DIR).index_file("index.html"))
    })
    .listen(socket)?
    .run();

    // Hand the handle to anything that needs to end the process: the /quit
    // endpoint, and the orphan watchdog below.
    let handle = server.handle();
    if let Ok(mut slot) = state_for_handle.server_handle.lock() {
        *slot = Some(handle.clone());
    }

    // A supervising app sets this so a server orphaned by a crashed or
    // force-quit parent doesn't linger holding the port.
    if std::env::var_os(EXIT_WITH_PARENT_VAR).is_some() {
        spawn_parent_watchdog(handle);
    }

    server.await
}

struct ApiState {
    started_at: Instant,
    /// The address actually bound, which is not necessarily the one requested:
    /// asking for port 0 yields an ephemeral one.
    bound_addr: String,
    listener: Mutex<Option<ListenerWorker>>,
    /// Last known mute state, updated by both the HTTP toggle endpoint and
    /// the DualSense button listener, so the UI reflects reality either way.
    last_muted: Mutex<Option<bool>>,
    /// Whether the running listener currently holds an open controller. A
    /// listener can be running while this is false: it waits for a controller
    /// to appear rather than failing.
    controller_connected: AtomicBool,
    /// Why the listener cannot open a controller, when it cannot. Distinguishes
    /// "nothing is plugged in" from "something is, and we are not allowed to
    /// read it" — which otherwise look identical from outside.
    controller_error: Mutex<Option<String>>,
    /// Last battery reading from the connected controller, or `None` when no
    /// controller is attached or it hasn't sent a full report yet. macOS itself
    /// doesn't surface the DualSense battery, so this is the only place it shows.
    controller_battery: Mutex<Option<controller::Battery>>,
    /// A single, persistent Discord IPC session reused across toggles.
    /// Reconnecting from scratch on every call races Discord's IPC server
    /// tearing down the previous session, and can hang indefinitely on the
    /// second toggle — see `toggle()` below.
    discord: Mutex<Option<discord_mute::DiscordMute>>,
    events: broadcast::Sender<String>,
    /// Set once the server is built. Stopping through this handle is what
    /// actually ends `run()`; `System::stop()` does not, because
    /// `#[actix_web::main]` is blocking on the server future rather than on
    /// the system's own event loop.
    server_handle: Mutex<Option<ServerHandle>>,
}

struct ListenerWorker {
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    ok: bool,
    pid: u32,
    uptime_seconds: u64,
    api: String,
    muted: Option<bool>,
    controller_connected: bool,
    controller_error: Option<String>,
    battery: Option<BatteryStatus>,
    listener: Option<ListenerStatus>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct BatteryStatus {
    /// Charge level 0–100, in the controller's own ~10% steps.
    percent: u8,
    /// "discharging" | "charging" | "full" | "unknown".
    state: &'static str,
}

impl From<controller::Battery> for BatteryStatus {
    fn from(battery: controller::Battery) -> Self {
        BatteryStatus {
            percent: battery.percent,
            state: battery.state.label(),
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ListenerStatus {
    running: bool,
    last_error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigStatus {
    ok: bool,
    configured: bool,
    config_path: String,
    token_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveConfigRequest {
    client_id: String,
    client_secret: String,
}

#[derive(Serialize)]
struct SaveConfigResponse {
    ok: bool,
    path: String,
}

#[derive(Serialize)]
struct ToggleResponse {
    ok: bool,
    muted: bool,
}

#[derive(Serialize)]
struct ErrorResponse {
    ok: bool,
    error: String,
}

async fn status(state: web::Data<ApiState>) -> impl Responder {
    HttpResponse::Ok().json(build_status(&state))
}

async fn get_config() -> impl Responder {
    HttpResponse::Ok().json(ConfigStatus {
        ok: true,
        configured: config::load_config().is_ok(),
        config_path: config::config_path().display().to_string(),
        token_path: token::token_path().display().to_string(),
    })
}

async fn save_config(request: web::Json<SaveConfigRequest>) -> impl Responder {
    let config = config::AppConfig {
        client_id: request.client_id.trim().to_string(),
        client_secret: request.client_secret.trim().to_string(),
    };

    match config::save_config(&config) {
        Ok(()) => HttpResponse::Ok().json(SaveConfigResponse {
            ok: true,
            path: config::config_path().display().to_string(),
        }),
        Err(err) => api_error(err),
    }
}

async fn toggle(state: web::Data<ApiState>) -> impl Responder {
    let state_for_worker = state.clone();
    let result = web::block(move || -> Result<bool> {
        let mut guard = state_for_worker
            .discord
            .lock()
            .map_err(|_| anyhow!("Discord connection lock is poisoned"))?;
        if guard.is_none() {
            *guard = Some(discord_mute::DiscordMute::connect()?);
        }
        // `toggle()` already retries once with a fresh reconnect internally
        // if the current session errors out, so this stays resilient
        // without us tearing the connection down between calls.
        guard.as_mut().expect("just ensured Some above").toggle()
    })
    .await;

    match result {
        Ok(Ok(muted)) => {
            set_muted(&state, muted);
            HttpResponse::Ok().json(ToggleResponse { ok: true, muted })
        }
        Ok(Err(err)) => api_error(err),
        Err(err) => api_error(anyhow!("Discord toggle worker failed: {err}")),
    }
}

async fn stop_listener(state: web::Data<ApiState>) -> impl Responder {
    match stop_current_listener(&state) {
        Ok(stopped) => {
            broadcast_snapshot(&state);
            HttpResponse::Ok().json(serde_json::json!({
                "ok": true,
                "stopped": stopped,
            }))
        }
        Err(err) => api_error(err),
    }
}

async fn start_mute_listener(state: web::Data<ApiState>) -> impl Responder {
    let state_for_toggle = state.clone();
    let result = start_listener(&state, move |stop, finished, last_error| {
        run_mute_listener(stop, finished, last_error, state_for_toggle)
    });

    match result {
        Ok(status) => {
            broadcast_snapshot(&state);
            HttpResponse::Ok().json(serde_json::json!({
                "ok": true,
                "listener": status,
            }))
        }
        Err(err) => api_error(err),
    }
}

async fn quit(state: web::Data<ApiState>) -> impl Responder {
    let _ = stop_current_listener(&state);

    let handle = state
        .server_handle
        .lock()
        .ok()
        .and_then(|handle| handle.clone());
    let Some(handle) = handle else {
        return api_error(anyhow!("server handle is unavailable; cannot quit"));
    };

    // Stop after this response has gone out, so the caller sees the
    // acknowledgement rather than a dropped connection.
    actix_web::rt::spawn(async move {
        actix_web::rt::time::sleep(Duration::from_millis(100)).await;
        handle.stop(true).await;
    });

    HttpResponse::Ok().json(serde_json::json!({
        "ok": true,
        "message": "quitting",
    }))
}

/// Upgrades to a WebSocket and streams status snapshots to the browser as
/// they happen, so the UI never has to poll.
async fn ws_index(
    req: HttpRequest,
    stream: web::Payload,
    state: web::Data<ApiState>,
) -> actix_web::Result<HttpResponse> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, stream)?;
    let mut rx = state.events.subscribe();

    // Send an initial snapshot immediately so the UI has something to
    // render before the first tick or the first mutation.
    let initial = serde_json::to_string(&build_status(&state)).unwrap_or_default();

    actix_web::rt::spawn(async move {
        if session.text(initial).await.is_err() {
            return;
        }

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(payload) => {
                            if session.text(payload).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                frame = futures_util::StreamExt::next(&mut msg_stream) => {
                    match frame {
                        Some(Ok(actix_ws::Message::Ping(bytes))) => {
                            if session.pong(&bytes).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(actix_ws::Message::Close(_))) | None => break,
                        Some(Err(_)) => break,
                        _ => {}
                    }
                }
            }
        }

        let _ = session.close(None).await;
    });

    Ok(response)
}

fn start_listener(
    state: &web::Data<ApiState>,
    run: impl FnOnce(Arc<AtomicBool>, Arc<AtomicBool>, Arc<Mutex<Option<String>>>) + Send + 'static,
) -> Result<ListenerStatus> {
    stop_current_listener(state)?;

    let stop = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let last_error = Arc::new(Mutex::new(None));

    let worker = ListenerWorker {
        stop: stop.clone(),
        finished: finished.clone(),
        last_error: last_error.clone(),
        handle: Some(thread::spawn(move || run(stop, finished, last_error))),
    };
    let status = worker.status();

    *state
        .listener
        .lock()
        .map_err(|_| anyhow!("listener state lock is poisoned"))? = Some(worker);

    Ok(status)
}

fn stop_current_listener(state: &web::Data<ApiState>) -> Result<bool> {
    // Take the worker out and drop the guard before joining below. The listener
    // thread broadcasts snapshots on every connect/disconnect, and those read
    // `state.listener` — holding the lock across `join()` would deadlock.
    let worker = state
        .listener
        .lock()
        .map_err(|_| anyhow!("listener state lock is poisoned"))?
        .take();
    let Some(mut worker) = worker else {
        return Ok(false);
    };

    worker.stop.store(true, Ordering::Relaxed);
    if let Some(handle) = worker.handle.take() {
        handle
            .join()
            .map_err(|_| anyhow!("listener thread panicked while stopping"))?;
    }
    state.controller_connected.store(false, Ordering::Relaxed);
    set_controller_error(state, None);
    set_controller_battery(state, None);

    Ok(true)
}

fn run_mute_listener(
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    state: web::Data<ApiState>,
) {
    let result = || -> Result<()> {
        let mut handler = MuteButtonHandler {
            discord: discord_mute::DiscordMute::connect()?,
            state: state.clone(),
            last_error: last_error.clone(),
        };
        controller::listen_mic_button_until(Some(stop), &mut handler)
    }();

    // The listener no longer dies on a disconnect, so reaching here means it
    // was stopped or hit something genuinely fatal.
    state.controller_connected.store(false, Ordering::Relaxed);
    set_controller_error(&state, None);
    set_controller_battery(&state, None);
    finish_listener(result, finished, last_error);
    broadcast_snapshot(&state);
}

/// Bridges the controller listener to Discord and the shared API state.
struct MuteButtonHandler {
    discord: discord_mute::DiscordMute,
    state: web::Data<ApiState>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl MuteButtonHandler {
    fn record_error(&self, error: Option<String>) {
        if let Ok(mut last_error) = self.last_error.lock() {
            *last_error = error;
        }
    }
}

impl controller::MicButtonHandler for MuteButtonHandler {
    fn on_press(&mut self) -> Result<bool> {
        match self.discord.toggle() {
            Ok(muted) => {
                // Clear a stale failure so the UI stops showing an error that
                // has since resolved itself.
                self.record_error(None);
                set_muted(&self.state, muted);
                Ok(muted)
            }
            Err(err) => {
                self.record_error(Some(err.to_string()));
                broadcast_snapshot(&self.state);
                Err(err)
            }
        }
    }

    fn on_connected(&mut self) -> Option<bool> {
        self.state.controller_connected.store(true, Ordering::Relaxed);
        set_controller_error(&self.state, None);
        broadcast_snapshot(&self.state);
        self.state.last_muted.lock().ok().and_then(|muted| *muted)
    }

    fn on_battery(&mut self, battery: controller::Battery) {
        set_controller_battery(&self.state, Some(battery));
        broadcast_snapshot(&self.state);
    }

    fn on_disconnected(&mut self) {
        self.state
            .controller_connected
            .store(false, Ordering::Relaxed);
        // A stale battery reading outlives the controller that reported it, so
        // clear it rather than leave the UI showing a ghost level.
        set_controller_battery(&self.state, None);
        broadcast_snapshot(&self.state);
    }

    fn on_waiting(&mut self, reason: &str) {
        // Only broadcast when the reason actually changes: this fires on every
        // reconnect attempt, and the UI does not need a snapshot per attempt.
        let changed = self
            .state
            .controller_error
            .lock()
            .map(|current| current.as_deref() != Some(reason))
            .unwrap_or(false);

        if changed {
            set_controller_error(&self.state, Some(reason.to_string()));
            broadcast_snapshot(&self.state);
        }
    }
}

fn set_controller_error(state: &web::Data<ApiState>, error: Option<String>) {
    if let Ok(mut slot) = state.controller_error.lock() {
        *slot = error;
    }
}

fn set_controller_battery(state: &web::Data<ApiState>, battery: Option<controller::Battery>) {
    if let Ok(mut slot) = state.controller_battery.lock() {
        *slot = battery;
    }
}

fn finish_listener(
    result: Result<()>,
    finished: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
) {
    if let Err(err) = result {
        if let Ok(mut last_error) = last_error.lock() {
            *last_error = Some(err.to_string());
        }
    }
    finished.store(true, Ordering::Relaxed);
}

fn current_listener_status(state: &web::Data<ApiState>) -> Option<ListenerStatus> {
    state
        .listener
        .lock()
        .ok()
        .and_then(|worker| worker.as_ref().map(ListenerWorker::status))
}

fn set_muted(state: &web::Data<ApiState>, muted: bool) {
    if let Ok(mut last_muted) = state.last_muted.lock() {
        *last_muted = Some(muted);
    }
    broadcast_snapshot(state);
}

fn build_status(state: &web::Data<ApiState>) -> StatusResponse {
    StatusResponse {
        ok: true,
        pid: std::process::id(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        api: state.bound_addr.clone(),
        muted: state.last_muted.lock().ok().and_then(|m| *m),
        controller_connected: state.controller_connected.load(Ordering::Relaxed),
        controller_error: state.controller_error.lock().ok().and_then(|e| e.clone()),
        battery: state
            .controller_battery
            .lock()
            .ok()
            .and_then(|b| *b)
            .map(BatteryStatus::from),
        listener: current_listener_status(state),
    }
}

fn broadcast_snapshot(state: &web::Data<ApiState>) {
    if let Ok(payload) = serde_json::to_string(&build_status(state)) {
        // No receivers is not an error, it just means no UI is open.
        let _ = state.events.send(payload);
    }
}

impl ListenerWorker {
    fn status(&self) -> ListenerStatus {
        ListenerStatus {
            running: !self.finished.load(Ordering::Relaxed),
            last_error: self.last_error.lock().ok().and_then(|error| error.clone()),
        }
    }
}

fn api_error(err: impl std::fmt::Display) -> HttpResponse {
    HttpResponse::InternalServerError().json(ErrorResponse {
        ok: false,
        error: err.to_string(),
    })
}

fn api_addr() -> String {
    std::env::var("DISCORD_MUTE_API_ADDR").unwrap_or_else(|_| DEFAULT_API_ADDR.to_string())
}

/// Binds the API socket, turning the usual "Address already in use" into
/// something that says what to do about it.
fn bind_api_socket() -> std::io::Result<std::net::TcpListener> {
    let addr = api_addr();
    std::net::TcpListener::bind(&addr).map_err(|err| {
        if err.kind() != std::io::ErrorKind::AddrInUse {
            return err;
        }

        std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!(
                "{addr} is already in use — another discord-mute-rs is probably still running.\n\
                 Stop it with `curl -X POST http://{addr}/quit`, or start this one on a free \
                 port with `DISCORD_MUTE_API_ADDR=127.0.0.1:0` (port 0 picks any free port and \
                 prints it as `{LISTENING_PREFIX}<addr>`)."
            ),
        )
    })
}

/// Exits if our parent process goes away.
///
/// An app that spawns this server can't always clean up after itself — a crash
/// or a force quit leaves the child running and holding the port, so the next
/// launch fails to bind. Polling `getppid` is the portable way to notice: once
/// the parent dies we are reparented to launchd (pid 1).
fn spawn_parent_watchdog(handle: ServerHandle) {
    let original_parent = std::os::unix::process::parent_id();
    if original_parent <= 1 {
        println!("Started without a live parent; skipping the orphan watchdog.");
        return;
    }

    println!("Watching parent pid {original_parent}; will exit if it goes away.");

    actix_web::rt::spawn(async move {
        let mut ticker = actix_web::rt::time::interval(PARENT_CHECK_INTERVAL);
        loop {
            ticker.tick().await;
            // Once the parent dies we are reparented to launchd, so either a
            // changed parent or pid 1 means we've been orphaned.
            let parent = std::os::unix::process::parent_id();
            if parent != original_parent || parent <= 1 {
                // Deliberately not `println!`. Our stdout is typically a pipe
                // owned by the very parent that just died, so writing to it
                // fails with EPIPE — and `println!` panics on write failure.
                // That panic would kill this task before it could shut
                // anything down, leaving the server alive and holding the
                // port, which is exactly the bug this watchdog exists to
                // prevent.
                let _ = writeln!(
                    std::io::stderr(),
                    "Parent pid {original_parent} is gone; shutting down."
                );

                // Try to stop gracefully, but never wait indefinitely: an open
                // WebSocket can hold a graceful shutdown open, and there is
                // nobody left to be graceful for.
                let _ = actix_web::rt::time::timeout(
                    ORPHAN_SHUTDOWN_TIMEOUT,
                    handle.stop(true),
                )
                .await;

                // Whatever happened above, do not linger.
                std::process::exit(0);
            }
        }
    });
}
