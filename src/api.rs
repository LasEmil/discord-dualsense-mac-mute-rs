use crate::{config, controller, discord_mute, keyboard, token};
use actix_web::{App, HttpResponse, HttpServer, Responder, web};
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

const DEFAULT_API_ADDR: &str = "127.0.0.1:3219";

pub async fn serve() -> std::io::Result<()> {
    let addr = api_addr();
    let state = web::Data::new(ApiState {
        started_at: Instant::now(),
        listener: Mutex::new(None),
    });

    println!("Discord mute API listening at http://{addr}");
    println!("Try: curl http://{addr}/status");

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
            .route("/quit", web::post().to(quit))
    })
    .bind(addr)?
    .run()
    .await
}

struct ApiState {
    started_at: Instant,
    listener: Mutex<Option<ListenerWorker>>,
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
    listener: Option<ListenerStatus>,
}

#[derive(Serialize)]
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
    HttpResponse::Ok().json(StatusResponse {
        ok: true,
        pid: std::process::id(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        api: api_addr(),
        listener: current_listener_status(&state),
    })
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

async fn toggle() -> impl Responder {
    match web::block(discord_mute::toggle_once).await {
        Ok(Ok(muted)) => HttpResponse::Ok().json(ToggleResponse { ok: true, muted }),
        Ok(Err(err)) => api_error(err),
        Err(err) => api_error(anyhow!("Discord toggle worker failed: {err}")),
    }
}

async fn set_led(request: web::Json<LedRequest>) -> impl Responder {
    let muted = request.muted;
    match web::block(move || controller::test_mic_led(muted)).await {
        Ok(Ok(())) => HttpResponse::Ok().json(OkResponse { ok: true }),
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
        Ok(stopped) => HttpResponse::Ok().json(serde_json::json!({
            "ok": true,
            "stopped": stopped,
        })),
        Err(err) => api_error(err),
    }
}

async fn start_mute_listener(state: web::Data<ApiState>) -> impl Responder {
    match start_listener(
        &state,
        ListenerMode::Mute,
        move |stop, finished, last_error| run_mute_listener(stop, finished, last_error),
    ) {
        Ok(status) => HttpResponse::Ok().json(serde_json::json!({
            "ok": true,
            "listener": status,
        })),
        Err(err) => api_error(err),
    }
}

async fn start_ptt_listener(state: web::Data<ApiState>) -> impl Responder {
    match start_listener(&state, ListenerMode::PushToTalk, run_push_to_talk_listener) {
        Ok(status) => HttpResponse::Ok().json(serde_json::json!({
            "ok": true,
            "listener": status,
        })),
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
) {
    let result = || -> Result<()> {
        let mut discord = discord_mute::DiscordMute::connect()?;
        let mut on_press = || discord.toggle();
        controller::listen_mic_button_until(Some(stop), &mut on_press)
    }();

    finish_listener(result, finished, last_error);
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
