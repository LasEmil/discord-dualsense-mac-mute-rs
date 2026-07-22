use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub client_id: String,
    pub client_secret: String,
}

/// User preferences that aren't Discord credentials. Kept in a separate file so
/// tuning something like rumble strength never has to touch — or require —
/// the OAuth secrets.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    /// Rumble confirmation strength, 0–100. 0 disables the buzz entirely.
    #[serde(default = "default_rumble_strength")]
    pub rumble_strength: u8,
    /// Whether the controller's lightbar mirrors the mute/deafen state. Off
    /// leaves the lightbar dark so it doesn't fight a game that drives it.
    #[serde(default = "default_lightbar_enabled")]
    pub lightbar_enabled: bool,
}

fn default_rumble_strength() -> u8 {
    100
}

fn default_lightbar_enabled() -> bool {
    true
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            rumble_strength: default_rumble_strength(),
            lightbar_enabled: default_lightbar_enabled(),
        }
    }
}

pub fn preferences_path() -> PathBuf {
    config_dir().join("preferences.json")
}

/// Loads preferences, falling back to defaults for anything missing or
/// unreadable — preferences are conveniences, never a reason to fail startup.
pub fn load_preferences() -> Preferences {
    match fs::read_to_string(preferences_path()) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => Preferences::default(),
    }
}

pub fn save_preferences(prefs: &Preferences) -> Result<()> {
    let prefs = Preferences {
        rumble_strength: prefs.rumble_strength.min(100),
        lightbar_enabled: prefs.lightbar_enabled,
    };
    fs::create_dir_all(config_dir()).with_context(|| {
        format!(
            "failed to create config directory at {}",
            config_dir().display()
        )
    })?;
    let contents =
        serde_json::to_string_pretty(&prefs).context("failed to encode preferences.json")?;
    fs::write(preferences_path(), format!("{contents}\n")).with_context(|| {
        format!(
            "failed to write preferences at {}",
            preferences_path().display()
        )
    })
}

pub fn load_config() -> Result<AppConfig> {
    let env_client_id = std::env::var("DISCORD_CLIENT_ID").ok();
    let env_client_secret = std::env::var("DISCORD_CLIENT_SECRET").ok();

    // Surface a broken environment as itself, rather than as a missing file at
    // a path that only looks wrong once you notice it is relative. Credentials
    // supplied purely through the environment don't need a directory at all.
    if env_client_id.is_none() && env_client_secret.is_none() {
        try_config_dir()?;
    }

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
            "missing config at {}; save credentials with \
             `curl -X PUT http://<addr>/config -H 'content-type: application/json' \
             -d '{{\"clientId\":\"...\",\"clientSecret\":\"...\"}}'`",
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

/// Resolves the config directory, or explains why it can't be.
///
/// Falling back to a relative path when `HOME` is unset produces a config that
/// silently depends on the working directory — which is how a GUI-spawned
/// server ends up looking for `./.config/discord-mute-rs/config.json` and
/// reporting a missing file rather than a missing environment.
pub fn try_config_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("discord-mute-rs"));
    }

    match std::env::var_os("HOME") {
        Some(home) => Ok(PathBuf::from(home).join(".config").join("discord-mute-rs")),
        None => bail!(
            "cannot locate the config directory: neither XDG_CONFIG_HOME nor HOME is set. \
             A process launched without an inherited environment must set one of them."
        ),
    }
}

/// Display-oriented form of [`try_config_dir`], for messages and status output
/// where erroring would be less useful than saying what we looked for.
pub fn config_dir() -> PathBuf {
    try_config_dir().unwrap_or_else(|_| PathBuf::from("<unknown: HOME is not set>"))
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
