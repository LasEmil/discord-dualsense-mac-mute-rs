use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub client_id: String,
    pub client_secret: String,
}

pub fn load_config() -> Result<AppConfig> {
    let env_client_id = std::env::var("DISCORD_CLIENT_ID").ok();
    let env_client_secret = std::env::var("DISCORD_CLIENT_SECRET").ok();

    let mut config = if config_path().exists() {
        let contents = fs::read_to_string(config_path())
            .with_context(|| format!("failed to read config at {}", config_path().display()))?;
        serde_json::from_str::<AppConfig>(&contents)
            .with_context(|| format!("failed to parse config at {}", config_path().display()))?
    } else if env_client_id.is_some() || env_client_secret.is_some() {
        AppConfig {
            client_id: String::new(),
            client_secret: String::new(),
        }
    } else {
        bail!(
            "missing config at {}; run `cargo run -- setup` first",
            config_path().display()
        );
    };

    if let Some(client_id) = env_client_id {
        config.client_id = client_id;
    }
    if let Some(client_secret) = env_client_secret {
        config.client_secret = client_secret;
    }

    validate_config(&config)?;
    Ok(config)
}

pub fn config_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("discord-mute-rs");
    }

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("discord-mute-rs")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn save_config(config: &AppConfig) -> Result<()> {
    validate_config(config)?;
    fs::create_dir_all(config_dir()).with_context(|| {
        format!(
            "failed to create config directory at {}",
            config_dir().display()
        )
    })?;
    let contents =
        serde_json::to_string_pretty(config).context("failed to encode config.json contents")?;
    fs::write(config_path(), format!("{contents}\n"))
        .with_context(|| format!("failed to write config at {}", config_path().display()))
}

fn validate_config(config: &AppConfig) -> Result<()> {
    if config.client_id.trim().is_empty() {
        bail!("Discord Client ID cannot be empty");
    }
    if config.client_secret.trim().is_empty() {
        bail!("Discord Client Secret cannot be empty");
    }
    Ok(())
}
