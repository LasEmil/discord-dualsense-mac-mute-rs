mod api;
mod auth;
mod config;
mod controller;
mod discord_ipc;
mod discord_mute;
mod notify;
mod token;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    if std::env::args().any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help")) {
        print_help();
        return Ok(());
    }

    api::serve().await
}

fn print_help() {
    println!(
        "discord-mute-rs

Starts a local Actix Web API for controlling Discord mute and DualSense helpers.

Run:
  cargo run

Environment:
  DISCORD_MUTE_API_ADDR=127.0.0.1:3219        API bind address
  TOKEN_FILE=/path/to/token.json              Override the token file path
  DISCORD_CLIENT_ID=your_client_id            Override configured Discord client ID
  DISCORD_CLIENT_SECRET=your_client_secret    Override configured Discord client secret

Useful curl commands:
  curl http://127.0.0.1:3219/status
  curl -X PUT http://127.0.0.1:3219/config \\
    -H 'content-type: application/json' \\
    -d '{{\"clientId\":\"...\",\"clientSecret\":\"...\"}}'
  curl -X POST http://127.0.0.1:3219/discord/toggle
  curl -X POST http://127.0.0.1:3219/listeners/mute

Stop the mic-button listener by stopping the server (Ctrl-C)."
    );
}
