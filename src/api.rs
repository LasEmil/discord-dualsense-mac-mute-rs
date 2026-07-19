use crate::{config, controller, discord_mute, token};
use actix_files::Files;
use actix_web::{App, HttpRequest, HttpResponse, HttpServer, Responder, web};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{
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
// How often we push a fresh snapshot to connected UIs, in addition to
// pushing immediately after every mutating request.
const BROADCAST_TICK: Duration = Duration::from_millis(500);

pub async fn serve() -> std::io::Result<()> {
    let addr = api_addr();
    let (events_tx, _) = broadcast::channel::<String>(32);

    let state = web::Data::new(ApiState {
        started_at: Instant::now(),
        listener: Mutex::new(None),
        last_muted: Mutex::new(None),
        controller_connected: AtomicBool::new(false),
        discord: Mutex::new(None),
        events: events_tx,
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

    println!("Discord mute API listening at http://{addr}");
    println!("Try: curl http://{addr}/status");
    println!("UI:  http://{addr}/");

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/status", web::get().to(status))
            .route("/config", web::get().to(get_config))
            .route("/config", web::put().to(save_config))
            .route("/discord/toggle", web::post().to(toggle))
            .route("/listeners/mute", web::post().to(start_mute_listener))
            .route("/ws", web::get().to(ws_index))
            .route("/quit", web::post().to(quit))
            // React build output. Keep this last so it never shadows the
            // API routes above; it only matches what nothing else claimed.
            .service(Files::new("/", STATIC_DIR).index_file("index.html"))
    })
    .bind(addr)?
    .run()
    .await
}

struct ApiState {
    started_at: Instant,
    listener: Mutex<Option<ListenerWorker>>,
    /// Last known mute state, updated by both the HTTP toggle endpoint and
    /// the DualSense button listener, so the UI reflects reality either way.
    last_muted: Mutex<Option<bool>>,
    /// Whether the running listener currently holds an open controller. A
    /// listener can be running while this is false: it waits for a controller
    /// to appear rather than failing.
    controller_connected: AtomicBool,
    /// A single, persistent Discord IPC session reused across toggles.
    /// Reconnecting from scratch on every call races Discord's IPC server
    /// tearing down the previous session, and can hang indefinitely on the
    /// second toggle — see `toggle()` below.
    discord: Mutex<Option<discord_mute::DiscordMute>>,
    events: broadcast::Sender<String>,
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
    listener: Option<ListenerStatus>,
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
    let system = actix_web::rt::System::current();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        system.stop();
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
        broadcast_snapshot(&self.state);
        self.state.last_muted.lock().ok().and_then(|muted| *muted)
    }

    fn on_disconnected(&mut self) {
        self.state
            .controller_connected
            .store(false, Ordering::Relaxed);
        broadcast_snapshot(&self.state);
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
        api: api_addr(),
        muted: state.last_muted.lock().ok().and_then(|m| *m),
        controller_connected: state.controller_connected.load(Ordering::Relaxed),
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
