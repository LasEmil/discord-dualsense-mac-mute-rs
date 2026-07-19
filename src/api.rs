use crate::{config, controller, discord_mute, keyboard, token};
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
            .route("/devices", web::get().to(devices))
            .route("/discord/toggle", web::post().to(toggle))
            .route("/controller/led", web::post().to(set_led))
            .route("/listeners/current", web::get().to(listener_status))
            .route("/listeners/current", web::delete().to(stop_listener))
            .route("/listeners/mute", web::post().to(start_mute_listener))
            .route("/listeners/ptt", web::post().to(start_ptt_listener))
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
    /// A single, persistent Discord IPC session reused across toggles.
    /// `toggle_once()` reconnects from scratch every call, which races
    /// Discord's IPC server tearing down the previous session and can hang
    /// indefinitely on the second toggle — see `toggle()` below.
    discord: Mutex<Option<discord_mute::DiscordMute>>,
    events: broadcast::Sender<String>,
}

struct ListenerWorker {
    mode: ListenerMode,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum ListenerMode {
    Mute,
    PushToTalk,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    ok: bool,
    pid: u32,
    uptime_seconds: u64,
    api: String,
    muted: Option<bool>,
    listener: Option<ListenerStatus>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ListenerStatus {
    mode: ListenerMode,
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

#[derive(Deserialize)]
struct LedRequest {
    muted: bool,
}

#[derive(Serialize)]
struct ToggleResponse {
    ok: bool,
    muted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DevicesResponse {
    ok: bool,
    devices: Vec<controller::DeviceInfo>,
}

#[derive(Serialize)]
struct OkResponse {
    ok: bool,
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

async fn devices() -> impl Responder {
    match web::block(controller::devices).await {
        Ok(Ok(devices)) => HttpResponse::Ok().json(DevicesResponse { ok: true, devices }),
        Ok(Err(err)) => api_error(err),
        Err(err) => api_error(anyhow!("device scan worker failed: {err}")),
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

async fn set_led(state: web::Data<ApiState>, request: web::Json<LedRequest>) -> impl Responder {
    let muted = request.muted;
    match web::block(move || controller::test_mic_led(muted)).await {
        Ok(Ok(())) => {
            set_muted(&state, muted);
            HttpResponse::Ok().json(OkResponse { ok: true })
        }
        Ok(Err(err)) => api_error(err),
        Err(err) => api_error(anyhow!("LED worker failed: {err}")),
    }
}

async fn listener_status(state: web::Data<ApiState>) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "ok": true,
        "listener": current_listener_status(&state),
    }))
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
    let result = start_listener(&state, ListenerMode::Mute, move |stop, finished, last_error| {
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

async fn start_ptt_listener(state: web::Data<ApiState>) -> impl Responder {
    match start_listener(&state, ListenerMode::PushToTalk, run_push_to_talk_listener) {
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
    mode: ListenerMode,
    run: impl FnOnce(Arc<AtomicBool>, Arc<AtomicBool>, Arc<Mutex<Option<String>>>) + Send + 'static,
) -> Result<ListenerStatus> {
    stop_current_listener(state)?;

    let stop = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let last_error = Arc::new(Mutex::new(None));

    let worker = ListenerWorker {
        mode,
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

    Ok(true)
}

fn run_mute_listener(
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    state: web::Data<ApiState>,
) {
    let result = || -> Result<()> {
        let mut discord = discord_mute::DiscordMute::connect()?;
        let mut on_press = || {
            let muted = discord.toggle()?;
            set_muted(&state, muted);
            Ok(muted)
        };
        controller::listen_mic_button_until(Some(stop), &mut on_press)
    }();

    finish_listener(result, finished, last_error);
    broadcast_snapshot(&state);
}

fn run_push_to_talk_listener(
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
) {
    let result = || -> Result<()> {
        let mut key = keyboard::RightOptionKey::new();
        let mut on_change = |pressed| if pressed { key.press() } else { key.release() };
        controller::listen_mic_button_hold_until(Some(stop), &mut on_change)
    }();

    finish_listener(result, finished, last_error);
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
            mode: self.mode,
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
