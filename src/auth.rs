use crate::{discord_ipc::DiscordIpc, token};
use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::Deserialize;

const DISCORD_API: &str = "https://discord.com/api";
const REDIRECT_URI: &str = "http://localhost";

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct OAuthMeResponse {
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    expires: Option<String>,
}

pub fn authorize_and_save_token(
    discord: &mut DiscordIpc,
    client_id: &str,
    client_secret: &str,
) -> Result<token::TokenData> {
    println!("Requesting Discord authorization for voice RPC scopes...");
    let code = discord.authorize(client_id, token::REQUIRED_SCOPES)?;

    println!("Exchanging Discord authorization code for access token...");
    let access_token = exchange_code_for_token(client_id, client_secret, &code)?;

    let authorization_info = match fetch_authorization_info(&access_token) {
        Ok(info) => Some(info),
        Err(err) => {
            println!("Warning: could not verify granted scopes via /oauth2/@me: {err}");
            None
        }
    };

    let scopes = authorization_info
        .as_ref()
        .map(|info| info.scopes.clone())
        .unwrap_or_default();
    token::ensure_required_scopes(&scopes)?;

    let token = token::TokenData {
        access_token,
        scopes,
        expires_at: authorization_info.and_then(|info| info.expires),
    };

    token::save_token(&token)?;
    println!(
        "Saved fresh Discord token to {}.",
        token::token_path().display()
    );

    Ok(token)
}

fn exchange_code_for_token(client_id: &str, client_secret: &str, code: &str) -> Result<String> {
    let client = Client::new();
    let response = client
        .post(format!("{DISCORD_API}/oauth2/token"))
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .context("failed to send Discord OAuth token request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .unwrap_or_else(|_| "(unreadable body)".to_string());
        bail!("Discord OAuth token request failed with {status}: {body}");
    }

    let token = response
        .json::<OAuthTokenResponse>()
        .context("failed to decode Discord OAuth token response")?;
    Ok(token.access_token)
}

fn fetch_authorization_info(access_token: &str) -> Result<OAuthMeResponse> {
    let client = Client::new();
    let response = client
        .get(format!("{DISCORD_API}/oauth2/@me"))
        .bearer_auth(access_token)
        .send()
        .context("failed to send Discord /oauth2/@me request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .unwrap_or_else(|_| "(unreadable body)".to_string());
        bail!("Discord /oauth2/@me request failed with {status}: {body}");
    }

    response
        .json::<OAuthMeResponse>()
        .context("failed to decode Discord /oauth2/@me response")
}
