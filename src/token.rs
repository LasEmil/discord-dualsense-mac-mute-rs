use crate::config;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const REQUIRED_SCOPES: &[&str] = &["rpc", "rpc.voice.read", "rpc.voice.write"];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenData {
    pub access_token: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

pub fn load_token() -> Result<TokenData> {
    let path = existing_token_path().unwrap_or_else(token_path);
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read token file at {}", path.display()))?;
    let token: TokenData = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse token file at {}", path.display()))?;

    if token.access_token.trim().is_empty() {
        bail!("token.json is missing accessToken");
    }

    ensure_required_scopes(&token.scopes)?;
    ensure_not_expired(&token)?;

    Ok(token)
}

pub fn save_token(token: &TokenData) -> Result<()> {
    let path = token_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create token directory at {}", parent.display()))?;
    }
    let contents =
        serde_json::to_string_pretty(token).context("failed to encode token.json contents")?;
    fs::write(&path, format!("{contents}\n"))
        .with_context(|| format!("failed to write token file at {}", path.display()))
}

pub fn ensure_required_scopes(scopes: &[String]) -> Result<()> {
    if scopes.is_empty() {
        return Ok(());
    }

    let missing = REQUIRED_SCOPES
        .iter()
        .filter(|scope| !scopes.iter().any(|granted| granted == **scope))
        .copied()
        .collect::<Vec<_>>();

    if !missing.is_empty() {
        bail!(
            "token.json is missing required scopes: {}",
            missing.join(", ")
        );
    }

    Ok(())
}

fn ensure_not_expired(token: &TokenData) -> Result<()> {
    let Some(expires_at) = token.expires_at.as_deref() else {
        return Ok(());
    };

    let expires_at = OffsetDateTime::parse(expires_at, &Rfc3339)
        .with_context(|| format!("token.json has an invalid expiresAt value: {expires_at}"))?;

    if expires_at <= OffsetDateTime::now_utc() {
        bail!("token.json expired at {expires_at}");
    }

    Ok(())
}

pub fn token_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TOKEN_FILE") {
        return PathBuf::from(path);
    }

    config::config_dir().join("token.json")
}

fn existing_token_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TOKEN_FILE") {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }

    let xdg = config::config_dir().join("token.json");
    if xdg.exists() {
        return Some(xdg);
    }

    let local = PathBuf::from("token.json");
    if local.exists() {
        return Some(local);
    }

    let legacy = PathBuf::from("../token.json");
    legacy.exists().then_some(legacy)
}
