use crate::{
    auth, config,
    discord_ipc::{DiscordIpc, VoiceSettings},
    token,
};
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

    /// Flips self-mute and returns the resulting voice settings.
    pub fn toggle_mute(&mut self) -> Result<VoiceSettings> {
        self.with_session("mute toggle", |discord| {
            let current = discord.get_voice_settings()?;
            let mute = !current.mute;
            println!("Discord reports mute={}, setting mute={mute}", current.mute);
            discord.set_mute(mute)?;
            Ok(VoiceSettings { mute, ..current })
        })
    }

    /// Flips self-deafen and returns the resulting voice settings. Discord
    /// couples the two — deafening also mutes — so we read the state back rather
    /// than assume what mute ended up as.
    pub fn toggle_deafen(&mut self) -> Result<VoiceSettings> {
        self.with_session("deafen toggle", |discord| {
            let current = discord.get_voice_settings()?;
            let deaf = !current.deaf;
            println!("Discord reports deaf={}, setting deaf={deaf}", current.deaf);
            discord.set_deaf(deaf)?;
            discord.get_voice_settings()
        })
    }

    /// Reads the current voice settings without changing them — used to keep the
    /// app and controller in sync with mutes made inside Discord itself.
    pub fn voice_settings(&mut self) -> Result<VoiceSettings> {
        self.with_session("voice settings read", |discord| discord.get_voice_settings())
    }

    /// Runs an IPC exchange, reconnecting and retrying once if the current
    /// session errors out. A single retry covers Discord tearing down a stale
    /// session without turning a transient hiccup into a failed action.
    fn with_session<T>(
        &mut self,
        what: &str,
        exchange: impl Fn(&mut DiscordIpc) -> Result<T>,
    ) -> Result<T> {
        match self.run_exchange(&exchange) {
            Ok(value) => Ok(value),
            Err(first_error) => {
                println!("Discord {what} failed: {first_error}");
                println!("Reconnecting Discord IPC and retrying once...");
                self.discord = None;
                self.reconnect()
                    .with_context(|| format!("failed to reconnect Discord IPC after {what}"))?;
                self.run_exchange(&exchange).with_context(|| {
                    format!("retry after Discord {what} also failed; first error was: {first_error}")
                })
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

    fn run_exchange<T>(&mut self, exchange: &impl Fn(&mut DiscordIpc) -> Result<T>) -> Result<T> {
        let discord = self
            .discord
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Discord IPC session is not connected"))?;
        exchange(discord)
        // Mute/deafen banners are posted by the macOS app (see `Notifier.swift`),
        // which reacts to the state changing in the status snapshot — that way
        // they carry the app's own icon, which an `osascript` notification from
        // here could not.
    }
}
