use crate::{auth, config, discord_ipc::DiscordIpc, notify, token};
use anyhow::{Context, Result};

pub struct DiscordMute {
    config: config::AppConfig,
    token: Option<token::TokenData>,
    discord: Option<DiscordIpc>,
}

impl DiscordMute {
    pub fn connect() -> Result<Self> {
        let config = config::load_config()?;
        let token = match token::load_token() {
            Ok(token) => Some(token),
            Err(err) => {
                println!("No usable cached Discord token: {err}");
                None
            }
        };

        let mut discord_mute = Self {
            config,
            token,
            discord: None,
        };
        discord_mute.reconnect()?;

        Ok(discord_mute)
    }

    pub fn toggle(&mut self) -> Result<bool> {
        match self.toggle_with_current_session() {
            Ok(muted) => Ok(muted),
            Err(first_error) => {
                println!("Discord toggle failed: {first_error}");
                println!("Reconnecting Discord IPC and retrying once...");
                self.discord = None;
                self.reconnect()
                    .context("failed to reconnect Discord IPC after toggle failure")?;
                self.toggle_with_current_session()
                    .with_context(|| format!("retry after Discord toggle failure also failed; first error was: {first_error}"))
            }
        }
    }

    fn reconnect(&mut self) -> Result<()> {
        let mut discord = DiscordIpc::connect(&self.config.client_id)?;

        match self.token.as_ref() {
            Some(token) => {
                if let Err(err) = discord.authenticate(&token.access_token) {
                    println!("Cached token authentication failed: {err}");
                    self.refresh_token(&mut discord)?;
                }
            }
            None => self.refresh_token(&mut discord)?,
        }

        self.discord = Some(discord);
        Ok(())
    }

    fn refresh_token(&mut self, discord: &mut DiscordIpc) -> Result<()> {
        let token = auth::authorize_and_save_token(
            discord,
            &self.config.client_id,
            &self.config.client_secret,
        )?;

        discord.authenticate(&token.access_token)?;
        self.token = Some(token);

        Ok(())
    }

    fn toggle_with_current_session(&mut self) -> Result<bool> {
        let discord = self
            .discord
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Discord IPC session is not connected"))?;
        let muted = discord.toggle_mute()?;

        notify::show(
            "Discord Mute Toggle",
            if muted { "Muted" } else { "Unmuted" },
        );

        Ok(muted)
    }
}
