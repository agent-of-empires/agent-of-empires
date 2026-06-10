//! Optional Telegram bridge for controlling sessions without the mobile web UI.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::server::api::{send_text_to_tmux_session, SendTextError};
use crate::server::AppState;
use crate::session::Instance;

const CONFIG_FILE: &str = "telegram.toml";
const TOKEN_ENV: &str = "AOE_TELEGRAM_BOT_TOKEN";
const LEGACY_TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";
const ALLOWED_CHATS_ENV: &str = "AOE_TELEGRAM_ALLOWED_CHAT_IDS";
const DEFAULT_SESSION_ENV: &str = "AOE_TELEGRAM_DEFAULT_SESSION";
const PARAKEET_MODEL_ENV: &str = "AOE_TELEGRAM_PARAKEET_MODEL";
const DEFAULT_POLL_TIMEOUT_SECS: u64 = 30;
const DEFAULT_VOICE_MAX_FILE_SIZE_MB: u64 = 25;
const DEFAULT_VOICE_TRANSCRIPTION_TIMEOUT_SECS: u64 = 600;
const DEFAULT_PARAKEET_MODEL: &str = "mlx-community/parakeet-tdt-0.6b-v2";
const MAX_TELEGRAM_TEXT_CHARS: usize = 3900;
const MAX_TAIL_LINES: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct TelegramConfig {
    enabled: bool,
    bot_token: String,
    allowed_chat_ids: Vec<i64>,
    claim_code: String,
    default_session: String,
    chat_sessions: BTreeMap<String, String>,
    last_update_id: Option<i64>,
    drop_pending_on_start: bool,
    poll_timeout_secs: u64,
    voice_transcription_enabled: bool,
    voice_max_file_size_mb: u64,
    voice_transcription_timeout_secs: u64,
    parakeet_model: String,
    parakeet_command: Vec<String>,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            allowed_chat_ids: Vec::new(),
            claim_code: String::new(),
            default_session: String::new(),
            chat_sessions: BTreeMap::new(),
            last_update_id: None,
            drop_pending_on_start: true,
            poll_timeout_secs: DEFAULT_POLL_TIMEOUT_SECS,
            voice_transcription_enabled: true,
            voice_max_file_size_mb: DEFAULT_VOICE_MAX_FILE_SIZE_MB,
            voice_transcription_timeout_secs: DEFAULT_VOICE_TRANSCRIPTION_TIMEOUT_SECS,
            parakeet_model: DEFAULT_PARAKEET_MODEL.to_string(),
            parakeet_command: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ChatRuntime {
    last_list: Vec<String>,
}

struct TelegramBridge {
    state: Arc<AppState>,
    client: reqwest::Client,
    config_path: PathBuf,
    config: Mutex<TelegramConfig>,
    runtime: Mutex<HashMap<i64, ChatRuntime>>,
}

struct DownloadedTelegramFile {
    #[allow(dead_code)]
    temp_dir: tempfile::TempDir,
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    chat: TelegramChat,
    text: Option<String>,
    voice: Option<TelegramAudioRef>,
    audio: Option<TelegramAudioRef>,
    document: Option<TelegramDocumentRef>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramAudioRef {
    file_id: String,
    #[allow(dead_code)]
    file_unique_id: Option<String>,
    mime_type: Option<String>,
    file_size: Option<u64>,
    duration: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramDocumentRef {
    file_id: String,
    #[allow(dead_code)]
    file_unique_id: Option<String>,
    file_name: Option<String>,
    mime_type: Option<String>,
    file_size: Option<u64>,
}

#[derive(Debug, Clone)]
struct VoiceInput {
    file_id: String,
    file_size: Option<u64>,
    duration: Option<u64>,
    mime_type: Option<String>,
    label: &'static str,
}

#[derive(Debug, Deserialize)]
struct TelegramFileInfo {
    file_id: String,
    #[allow(dead_code)]
    file_unique_id: Option<String>,
    file_size: Option<u64>,
    file_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct TelegramGetUpdates {
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<i64>,
    timeout: u64,
    allowed_updates: [&'static str; 1],
}

#[derive(Debug, Serialize)]
struct TelegramGetFile<'a> {
    file_id: &'a str,
}

#[derive(Debug, Serialize)]
struct TelegramSendMessage<'a> {
    chat_id: i64,
    text: &'a str,
    disable_web_page_preview: bool,
}

#[derive(Debug)]
enum TailError {
    NotFound,
    NotRunning,
    Tmux(String),
    Internal,
}

/// Start the Telegram bridge if `~/.agent-of-empires/telegram.toml` or
/// environment variables enable it. Failures are logged and do not stop
/// the web dashboard from starting.
pub async fn spawn_if_configured(state: Arc<AppState>) {
    match TelegramBridge::load(state).await {
        Ok(Some(bridge)) => {
            info!(target: "telegram.bridge", "Telegram bridge enabled");
            tokio::spawn(async move {
                bridge.run().await;
            });
        }
        Ok(None) => {}
        Err(e) => {
            warn!(target: "telegram.bridge", "Telegram bridge disabled: {e}");
        }
    }
}

impl TelegramBridge {
    async fn load(state: Arc<AppState>) -> Result<Option<Self>> {
        let app_dir = crate::session::get_app_dir()?;
        let config_path = app_dir.join(CONFIG_FILE);
        let config_exists = tokio::fs::try_exists(&config_path).await.unwrap_or(false);
        let mut config = if config_exists {
            let raw = tokio::fs::read_to_string(&config_path)
                .await
                .with_context(|| format!("read {}", config_path.display()))?;
            toml::from_str::<TelegramConfig>(&raw)
                .with_context(|| format!("parse {}", config_path.display()))?
        } else {
            TelegramConfig::default()
        };

        apply_env_overrides(&mut config);

        if !config.enabled && config.bot_token.trim().is_empty() {
            return Ok(None);
        }
        if config.bot_token.trim().is_empty() {
            warn!(
                target: "telegram.bridge",
                "telegram.toml has enabled=true but no bot_token"
            );
            return Ok(None);
        }

        let mut changed = false;
        if config.allowed_chat_ids.is_empty() && config.claim_code.trim().is_empty() {
            config.claim_code = generate_claim_code();
            changed = true;
        }
        config.poll_timeout_secs = config.poll_timeout_secs.clamp(5, 50);
        config.voice_max_file_size_mb = config.voice_max_file_size_mb.clamp(1, 50);
        config.voice_transcription_timeout_secs =
            config.voice_transcription_timeout_secs.clamp(30, 1800);
        if config.parakeet_model.trim().is_empty() {
            config.parakeet_model = DEFAULT_PARAKEET_MODEL.to_string();
        }

        if config_exists && changed {
            save_config(&config_path, &config).await?;
        } else if !config_exists && config.allowed_chat_ids.is_empty() {
            warn!(
                target: "telegram.bridge",
                "Telegram bridge is using env-only config; claim code will rotate on restart"
            );
        }

        Ok(Some(Self {
            state,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(config.poll_timeout_secs + 10))
                .build()
                .context("build telegram http client")?,
            config_path,
            config: Mutex::new(config),
            runtime: Mutex::new(HashMap::new()),
        }))
    }

    async fn run(self) {
        if let Err(e) = self.drop_pending_if_needed().await {
            warn!(target: "telegram.bridge", "initial Telegram drain failed: {e}");
        }

        let mut backoff = Duration::from_secs(1);
        loop {
            tokio::select! {
                _ = self.state.shutdown.cancelled() => {
                    info!(target: "telegram.bridge", "Telegram bridge stopped");
                    return;
                }
                result = self.poll_once() => {
                    match result {
                        Ok(()) => backoff = Duration::from_secs(1),
                        Err(e) => {
                            warn!(target: "telegram.bridge", "Telegram poll failed: {e}");
                            tokio::select! {
                                _ = self.state.shutdown.cancelled() => return,
                                _ = tokio::time::sleep(backoff) => {}
                            }
                            backoff = (backoff * 2).min(Duration::from_secs(60));
                        }
                    }
                }
            }
        }
    }

    async fn drop_pending_if_needed(&self) -> Result<()> {
        let should_drop = {
            let cfg = self.config.lock().await;
            cfg.drop_pending_on_start && cfg.last_update_id.is_none()
        };
        if !should_drop {
            return Ok(());
        }

        let updates = self.fetch_updates(None, 0).await?;
        let Some(max_id) = updates.iter().map(|u| u.update_id).max() else {
            let mut cfg = self.config.lock().await;
            cfg.drop_pending_on_start = false;
            save_config(&self.config_path, &cfg).await?;
            return Ok(());
        };

        let mut cfg = self.config.lock().await;
        cfg.last_update_id = Some(max_id);
        cfg.drop_pending_on_start = false;
        save_config(&self.config_path, &cfg).await?;
        info!(
            target: "telegram.bridge",
            dropped = updates.len(),
            "Telegram bridge dropped pending updates on first start"
        );
        Ok(())
    }

    async fn poll_once(&self) -> Result<()> {
        let (offset, timeout) = {
            let cfg = self.config.lock().await;
            (
                cfg.last_update_id.map(|id| id + 1),
                cfg.poll_timeout_secs.clamp(5, 50),
            )
        };
        let updates = self.fetch_updates(offset, timeout).await?;
        for update in updates {
            let update_id = update.update_id;
            if let Err(e) = self.handle_update(update).await {
                warn!(target: "telegram.bridge", "Telegram update failed: {e}");
            }
            let mut cfg = self.config.lock().await;
            cfg.last_update_id = Some(update_id);
            save_config(&self.config_path, &cfg).await?;
        }
        Ok(())
    }

    async fn fetch_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> Result<Vec<TelegramUpdate>> {
        let token = self.bot_token().await;
        let url = telegram_url(&token, "getUpdates");
        let payload = TelegramGetUpdates {
            offset,
            timeout: timeout_secs,
            allowed_updates: ["message"],
        };
        let res = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("getUpdates request failed")?;
        parse_telegram_response::<Vec<TelegramUpdate>>(res).await
    }

    async fn handle_update(&self, update: TelegramUpdate) -> Result<()> {
        let Some(message) = update.message else {
            return Ok(());
        };
        let chat_id = message.chat.id;
        if let Some(text) = message.text.as_deref() {
            debug!(
                target: "telegram.bridge",
                chat_id,
                message_id = message.message_id,
                "handling Telegram text"
            );
            return self.handle_text(chat_id, text).await;
        }

        if let Some(voice) = voice_input_from_message(&message) {
            debug!(
                target: "telegram.bridge",
                chat_id,
                message_id = message.message_id,
                kind = voice.label,
                duration = voice.duration,
                "handling Telegram voice/audio"
            );
            return self.handle_voice(chat_id, voice).await;
        }

        if self.is_authorized(chat_id).await {
            self.send_reply(chat_id, "Send text or a voice note.")
                .await?;
        }
        Ok(())
    }

    async fn handle_text(&self, chat_id: i64, text: &str) -> Result<()> {
        let trimmed = text.trim();
        if let Some((cmd, rest)) = parse_command(trimmed) {
            return self.handle_command(chat_id, &cmd, rest).await;
        }

        if !self.is_authorized(chat_id).await {
            self.send_reply(
                chat_id,
                "This chat is not authorized. Use /claim <code> from the private AoE Telegram config.",
            )
            .await?;
            return Ok(());
        }

        let Some(session_id) = self.resolve_target_session(chat_id, "").await? else {
            self.send_reply(
                chat_id,
                "No session selected. Use /sessions then /use <number>.",
            )
            .await?;
            return Ok(());
        };
        self.send_prompt_to_session(chat_id, &session_id, text)
            .await
    }

    async fn handle_voice(&self, chat_id: i64, voice: VoiceInput) -> Result<()> {
        if !self.is_authorized(chat_id).await {
            self.send_reply(
                chat_id,
                "This chat is not authorized. Use /claim <code> from the private AoE Telegram config.",
            )
            .await?;
            return Ok(());
        }

        let Some(session_id) = self.resolve_target_session(chat_id, "").await? else {
            self.send_reply(
                chat_id,
                "No session selected. Use /sessions then /use <number> before sending voice notes.",
            )
            .await?;
            return Ok(());
        };

        let cfg = self.config.lock().await.clone();
        if !cfg.voice_transcription_enabled {
            self.send_reply(chat_id, "Voice transcription is disabled in telegram.toml.")
                .await?;
            return Ok(());
        }

        let max_bytes = cfg.voice_max_file_size_mb.saturating_mul(1024 * 1024);
        if let Some(size) = voice.file_size {
            if size > max_bytes {
                self.send_reply(
                    chat_id,
                    &format!(
                        "Voice note is too large ({:.1} MB). Limit is {} MB.",
                        size as f64 / (1024.0 * 1024.0),
                        cfg.voice_max_file_size_mb
                    ),
                )
                .await?;
                return Ok(());
            }
        }

        self.send_reply(
            chat_id,
            &format!("Transcribing {} with NVIDIA Parakeet v2...", voice.label),
        )
        .await?;

        let downloaded = match self.download_telegram_file(&voice, max_bytes).await {
            Ok(file) => file,
            Err(e) => {
                warn!(target: "telegram.bridge", "voice download failed: {e}");
                self.send_reply(chat_id, "Could not download the voice note from Telegram.")
                    .await?;
                return Ok(());
            }
        };

        let transcript = match transcribe_with_parakeet(&downloaded.path, &cfg).await {
            Ok(text) => text,
            Err(e) => {
                warn!(target: "telegram.bridge", "voice transcription failed: {e}");
                self.send_reply(chat_id, &format!("Parakeet transcription failed: {e}"))
                    .await?;
                return Ok(());
            }
        };

        if transcript.trim().is_empty() {
            self.send_reply(chat_id, "Parakeet returned an empty transcript.")
                .await?;
            return Ok(());
        }

        self.send_reply(
            chat_id,
            &format!(
                "Transcript:\n{}",
                truncate_from_end(transcript.trim(), MAX_TELEGRAM_TEXT_CHARS)
            ),
        )
        .await?;
        self.send_prompt_to_session(chat_id, &session_id, transcript.trim())
            .await
    }

    async fn handle_command(&self, chat_id: i64, cmd: &str, rest: &str) -> Result<()> {
        match cmd {
            "start" | "help" => {
                let authorized = self.is_authorized(chat_id).await;
                self.send_reply(chat_id, help_text(authorized)).await
            }
            "claim" => self.handle_claim(chat_id, rest.trim()).await,
            "whoami" => self.send_reply(chat_id, &format!("chat_id: {chat_id}")).await,
            _ if !self.is_authorized(chat_id).await => {
                self.send_reply(
                    chat_id,
                    "This chat is not authorized. Use /claim <code> from the private AoE Telegram config.",
                )
                .await
            }
            "sessions" => self.handle_sessions(chat_id).await,
            "use" => self.handle_use(chat_id, rest.trim()).await,
            "tail" | "output" => self.handle_tail(chat_id, rest.trim()).await,
            "status" => self.handle_status(chat_id).await,
            _ => {
                self.send_reply(chat_id, "Unknown command. Use /help.").await
            }
        }
    }

    async fn handle_claim(&self, chat_id: i64, code: &str) -> Result<()> {
        let mut cfg = self.config.lock().await;
        if cfg.allowed_chat_ids.contains(&chat_id) {
            drop(cfg);
            self.send_reply(chat_id, "This chat is already authorized.")
                .await?;
            return Ok(());
        }
        if cfg.claim_code.trim().is_empty() || code != cfg.claim_code {
            drop(cfg);
            self.send_reply(
                chat_id,
                "Claim failed. Check the private claim_code in telegram.toml.",
            )
            .await?;
            return Ok(());
        }
        cfg.allowed_chat_ids.push(chat_id);
        cfg.allowed_chat_ids.sort_unstable();
        cfg.allowed_chat_ids.dedup();
        save_config(&self.config_path, &cfg).await?;
        drop(cfg);
        self.send_reply(
            chat_id,
            "Chat authorized. Use /sessions, then /use <number>.",
        )
        .await
    }

    async fn handle_sessions(&self, chat_id: i64) -> Result<()> {
        let sessions = self.sessions_snapshot().await;
        if sessions.is_empty() {
            self.send_reply(chat_id, "No AoE sessions found.").await?;
            return Ok(());
        }
        {
            let mut runtime = self.runtime.lock().await;
            runtime.entry(chat_id).or_default().last_list =
                sessions.iter().map(|s| s.id.clone()).collect();
        }
        self.send_reply(chat_id, &format_sessions(&sessions)).await
    }

    async fn handle_use(&self, chat_id: i64, selector: &str) -> Result<()> {
        if selector.is_empty() {
            self.send_reply(chat_id, "Usage: /use <number, id, or title>")
                .await?;
            return Ok(());
        }
        let Some(session_id) = self.resolve_target_session(chat_id, selector).await? else {
            self.send_reply(
                chat_id,
                "No matching session. Use /sessions to list choices.",
            )
            .await?;
            return Ok(());
        };
        let sessions = self.sessions_snapshot().await;
        let title = sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| s.title.as_str())
            .unwrap_or(session_id.as_str())
            .to_string();
        {
            let mut cfg = self.config.lock().await;
            cfg.chat_sessions
                .insert(chat_id.to_string(), session_id.clone());
            save_config(&self.config_path, &cfg).await?;
        }
        self.send_reply(chat_id, &format!("Selected: {title}"))
            .await
    }

    async fn handle_tail(&self, chat_id: i64, rest: &str) -> Result<()> {
        let lines = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(80)
            .clamp(1, MAX_TAIL_LINES);
        let Some(session_id) = self.resolve_target_session(chat_id, "").await? else {
            self.send_reply(
                chat_id,
                "No session selected. Use /sessions then /use <number>.",
            )
            .await?;
            return Ok(());
        };
        match self.capture_tail(&session_id, lines).await {
            Ok(content) => {
                let body = if content.trim().is_empty() {
                    "(no output)".to_string()
                } else {
                    truncate_from_end(&content, MAX_TELEGRAM_TEXT_CHARS)
                };
                self.send_reply(chat_id, &body).await
            }
            Err(TailError::NotFound) => self.send_reply(chat_id, "Session not found.").await,
            Err(TailError::NotRunning) => self.send_reply(chat_id, "Session is not running.").await,
            Err(TailError::Tmux(e)) => {
                self.send_reply(chat_id, &format!("Could not read tmux output: {e}"))
                    .await
            }
            Err(TailError::Internal) => {
                self.send_reply(chat_id, "Could not read output due to an internal error.")
                    .await
            }
        }
    }

    async fn handle_status(&self, chat_id: i64) -> Result<()> {
        let Some(session_id) = self.resolve_target_session(chat_id, "").await? else {
            self.send_reply(
                chat_id,
                "No session selected. Use /sessions then /use <number>.",
            )
            .await?;
            return Ok(());
        };
        let sessions = self.sessions_snapshot().await;
        let Some(inst) = sessions.iter().find(|s| s.id == session_id) else {
            self.send_reply(chat_id, "Session not found.").await?;
            return Ok(());
        };
        self.send_reply(
            chat_id,
            &format!(
                "{}\nstatus: {:?}\ntool: {}\nmode: {}",
                inst.title,
                inst.status,
                inst.tool,
                if inst.is_cockpit_mode() {
                    "cockpit"
                } else {
                    "tmux"
                }
            ),
        )
        .await
    }

    async fn send_prompt_to_session(
        &self,
        chat_id: i64,
        session_id: &str,
        text: &str,
    ) -> Result<()> {
        let Some(instance) = self
            .sessions_snapshot()
            .await
            .into_iter()
            .find(|s| s.id == session_id)
        else {
            self.send_reply(chat_id, "Session not found.").await?;
            return Ok(());
        };

        if instance.is_cockpit_mode() {
            return self
                .send_cockpit_prompt(chat_id, &instance, text.to_string())
                .await;
        }

        match send_text_to_tmux_session(self.state.clone(), session_id, text.to_string(), true)
            .await
        {
            Ok(_) => {
                self.send_reply(chat_id, &format!("Sent to {}.", instance.title))
                    .await
            }
            Err(e) => self.send_reply(chat_id, &format_send_error(&e)).await,
        }
    }

    async fn send_cockpit_prompt(
        &self,
        chat_id: i64,
        instance: &Instance,
        text: String,
    ) -> Result<()> {
        self.state
            .cockpit_supervisor
            .publish_user_prompt(&instance.id, text.clone())
            .await;
        match self
            .state
            .cockpit_supervisor
            .send_prompt(&instance.id, &text)
            .await
        {
            Ok(()) => {
                self.send_reply(chat_id, &format!("Sent to {}.", instance.title))
                    .await
            }
            Err(crate::cockpit::supervisor::SupervisorError::UnknownSession(_)) => {
                self.send_reply(
                    chat_id,
                    "Cockpit worker is not running for this session. Open it in the dashboard or restart aoe serve.",
                )
                .await
            }
            Err(e) => {
                self.send_reply(chat_id, &format!("Cockpit prompt failed: {e}"))
                    .await
            }
        }
    }

    async fn capture_tail(&self, session_id: &str, lines: usize) -> Result<String, TailError> {
        let instances = self.state.instances.read().await;
        let Some(instance) = instances.iter().find(|i| i.id == session_id).cloned() else {
            return Err(TailError::NotFound);
        };
        drop(instances);

        if instance.is_cockpit_mode() {
            return Err(TailError::Tmux(
                "tail is only available for tmux sessions".to_string(),
            ));
        }

        let capture = tokio::task::spawn_blocking(move || {
            let session = instance
                .tmux_session()
                .map_err(|e| TailError::Tmux(e.to_string()))?;
            if !session.exists() {
                return Err(TailError::NotRunning);
            }
            let raw = session
                .capture_pane(lines)
                .map_err(|e| TailError::Tmux(e.to_string()))?;
            Ok(crate::tmux::utils::strip_ansi(&raw))
        })
        .await;
        match capture {
            Ok(result) => result,
            Err(e) => {
                warn!(target: "telegram.bridge", "tail task panicked: {e}");
                Err(TailError::Internal)
            }
        }
    }

    async fn download_telegram_file(
        &self,
        voice: &VoiceInput,
        max_bytes: u64,
    ) -> Result<DownloadedTelegramFile> {
        let token = self.bot_token().await;
        let info = self.get_file_info(&token, &voice.file_id).await?;
        if info.file_id != voice.file_id {
            debug!(
                target: "telegram.bridge",
                requested = %voice.file_id,
                returned = %info.file_id,
                "Telegram getFile returned a canonical file id"
            );
        }
        if let Some(size) = info.file_size.or(voice.file_size) {
            if size > max_bytes {
                return Err(anyhow!(
                    "Telegram file is too large ({size} bytes, limit {max_bytes} bytes)"
                ));
            }
        }
        let file_path = info
            .file_path
            .ok_or_else(|| anyhow!("Telegram getFile did not return file_path"))?;
        let extension = safe_extension(&file_path).unwrap_or_else(|| {
            voice
                .mime_type
                .as_deref()
                .and_then(extension_from_mime)
                .unwrap_or("oga")
                .to_string()
        });
        let temp_dir = tempfile::Builder::new()
            .prefix("aoe-telegram-voice-")
            .tempdir()
            .context("create voice temp dir")?;
        let path = temp_dir.path().join(format!("voice.{extension}"));
        let url = format!("https://api.telegram.org/file/bot{token}/{file_path}");
        let res = self
            .client
            .get(url)
            .send()
            .await
            .context("download Telegram file request failed")?;
        if res.status() != StatusCode::OK {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Telegram file download HTTP {status}: {}",
                truncate_log(&body)
            ));
        }
        let bytes = res.bytes().await.context("read Telegram file bytes")?;
        if bytes.len() as u64 > max_bytes {
            return Err(anyhow!(
                "downloaded file is too large ({} bytes, limit {max_bytes} bytes)",
                bytes.len()
            ));
        }
        tokio::fs::write(&path, &bytes)
            .await
            .with_context(|| format!("write {}", path.display()))?;
        Ok(DownloadedTelegramFile { temp_dir, path })
    }

    async fn get_file_info(&self, token: &str, file_id: &str) -> Result<TelegramFileInfo> {
        let url = telegram_url(token, "getFile");
        let payload = TelegramGetFile { file_id };
        let res = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("getFile request failed")?;
        parse_telegram_response(res).await
    }

    async fn resolve_target_session(&self, chat_id: i64, selector: &str) -> Result<Option<String>> {
        let sessions = self.sessions_snapshot().await;
        if sessions.is_empty() {
            return Ok(None);
        }

        if selector.trim().is_empty() {
            if let Some(id) = self.selected_session_for_chat(chat_id).await {
                if sessions.iter().any(|s| s.id == id) {
                    return Ok(Some(id));
                }
            }
            let default_session = {
                let cfg = self.config.lock().await;
                cfg.default_session.trim().to_string()
            };
            if !default_session.is_empty() {
                if let Some(id) = resolve_selector(&sessions, &default_session, None)? {
                    return Ok(Some(id));
                }
            }
            let pi_sessions: Vec<&Instance> = sessions.iter().filter(|s| s.tool == "pi").collect();
            if pi_sessions.len() == 1 {
                return Ok(Some(pi_sessions[0].id.clone()));
            }
            if sessions.len() == 1 {
                return Ok(Some(sessions[0].id.clone()));
            }
            return Ok(None);
        }

        let last_list = {
            let runtime = self.runtime.lock().await;
            runtime
                .get(&chat_id)
                .map(|s| s.last_list.clone())
                .unwrap_or_default()
        };
        resolve_selector(&sessions, selector, Some(&last_list))
    }

    async fn selected_session_for_chat(&self, chat_id: i64) -> Option<String> {
        let cfg = self.config.lock().await;
        cfg.chat_sessions.get(&chat_id.to_string()).cloned()
    }

    async fn sessions_snapshot(&self) -> Vec<Instance> {
        self.state.instances.read().await.clone()
    }

    async fn is_authorized(&self, chat_id: i64) -> bool {
        let cfg = self.config.lock().await;
        cfg.allowed_chat_ids.contains(&chat_id)
    }

    async fn bot_token(&self) -> String {
        self.config.lock().await.bot_token.clone()
    }

    async fn send_reply(&self, chat_id: i64, text: &str) -> Result<()> {
        for chunk in split_telegram_text(text) {
            self.send_message_chunk(chat_id, &chunk).await?;
        }
        Ok(())
    }

    async fn send_message_chunk(&self, chat_id: i64, text: &str) -> Result<()> {
        let token = self.bot_token().await;
        let url = telegram_url(&token, "sendMessage");
        let payload = TelegramSendMessage {
            chat_id,
            text,
            disable_web_page_preview: true,
        };
        let res = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .context("sendMessage request failed")?;
        let _: serde_json::Value = parse_telegram_response(res).await?;
        Ok(())
    }
}

fn apply_env_overrides(config: &mut TelegramConfig) {
    if let Some(token) = std::env::var(TOKEN_ENV)
        .ok()
        .or_else(|| std::env::var(LEGACY_TOKEN_ENV).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        config.bot_token = token;
        config.enabled = true;
    }
    if let Ok(raw) = std::env::var(ALLOWED_CHATS_ENV) {
        config.allowed_chat_ids = parse_chat_ids(&raw);
    }
    if let Ok(raw) = std::env::var(DEFAULT_SESSION_ENV) {
        config.default_session = raw.trim().to_string();
    }
    if let Ok(raw) = std::env::var(PARAKEET_MODEL_ENV) {
        let model = raw.trim();
        if !model.is_empty() {
            config.parakeet_model = model.to_string();
        }
    }
}

fn voice_input_from_message(message: &TelegramMessage) -> Option<VoiceInput> {
    if let Some(voice) = &message.voice {
        return Some(VoiceInput {
            file_id: voice.file_id.clone(),
            file_size: voice.file_size,
            duration: voice.duration,
            mime_type: voice.mime_type.clone(),
            label: "voice note",
        });
    }
    if let Some(audio) = &message.audio {
        return Some(VoiceInput {
            file_id: audio.file_id.clone(),
            file_size: audio.file_size,
            duration: audio.duration,
            mime_type: audio.mime_type.clone(),
            label: "audio message",
        });
    }
    let document = message.document.as_ref()?;
    let mime_is_audio = document
        .mime_type
        .as_deref()
        .is_some_and(|mime| mime.starts_with("audio/"));
    let name_is_audio = document
        .file_name
        .as_deref()
        .is_some_and(has_audio_extension);
    if !mime_is_audio && !name_is_audio {
        return None;
    }
    Some(VoiceInput {
        file_id: document.file_id.clone(),
        file_size: document.file_size,
        duration: None,
        mime_type: document.mime_type.clone(),
        label: "audio file",
    })
}

async fn transcribe_with_parakeet(audio_path: &Path, config: &TelegramConfig) -> Result<String> {
    let out_dir = tempfile::Builder::new()
        .prefix("aoe-parakeet-output-")
        .tempdir()
        .context("create Parakeet output temp dir")?;
    let (program, args) = build_parakeet_command(config, audio_path, out_dir.path())?;
    let timeout = Duration::from_secs(config.voice_transcription_timeout_secs.clamp(30, 1800));
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow!("Parakeet timed out after {}s", timeout.as_secs()))?
        .with_context(|| format!("spawn {}", program.to_string_lossy()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(anyhow!(
            "Parakeet exited with {}: {}",
            output.status,
            truncate_log(stderr.trim())
        ));
    }

    if let Some(path) = find_transcript_file(out_dir.path()).await? {
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        if !text.trim().is_empty() {
            return Ok(text.trim().to_string());
        }
    }

    let fallback = stdout
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default();
    if fallback.is_empty() {
        Err(anyhow!("Parakeet produced no transcript"))
    } else {
        Ok(fallback.to_string())
    }
}

fn build_parakeet_command(
    config: &TelegramConfig,
    audio_path: &Path,
    out_dir: &Path,
) -> Result<(OsString, Vec<OsString>)> {
    let (program, mut args) = if config.parakeet_command.is_empty() {
        default_parakeet_command()
    } else {
        let mut configured = config.parakeet_command.iter();
        let Some(program) = configured.next() else {
            return Err(anyhow!("parakeet_command is empty"));
        };
        (
            OsString::from(program),
            configured.map(OsString::from).collect::<Vec<_>>(),
        )
    };

    args.push(audio_path.as_os_str().to_os_string());
    args.push("--model".into());
    args.push(config.parakeet_model.as_str().into());
    args.push("--output-format".into());
    args.push("txt".into());
    args.push("--output-dir".into());
    args.push(out_dir.as_os_str().to_os_string());
    args.push("--output-template".into());
    args.push("transcript".into());
    Ok((program, args))
}

fn default_parakeet_command() -> (OsString, Vec<OsString>) {
    if which::which("parakeet-mlx").is_ok() {
        return ("parakeet-mlx".into(), Vec::new());
    }
    if which::which("uvx").is_ok() {
        return (
            "uvx".into(),
            vec![
                "--from".into(),
                "parakeet-mlx".into(),
                "parakeet-mlx".into(),
            ],
        );
    }
    (
        "uv".into(),
        vec![
            "tool".into(),
            "run".into(),
            "--from".into(),
            "parakeet-mlx".into(),
            "parakeet-mlx".into(),
        ],
    )
}

async fn find_transcript_file(dir: &Path) -> Result<Option<PathBuf>> {
    let expected = dir.join("transcript.txt");
    if tokio::fs::try_exists(&expected).await.unwrap_or(false) {
        return Ok(Some(expected));
    }
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .with_context(|| format!("read {}", dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn safe_extension(path: &str) -> Option<String> {
    let ext = Path::new(path).extension()?.to_str()?;
    if ext
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() && ext.len() <= 8)
    {
        Some(ext.to_ascii_lowercase())
    } else {
        None
    }
}

fn extension_from_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "audio/ogg" | "audio/opus" => Some("oga"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/mp4" | "audio/x-m4a" => Some("m4a"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/flac" => Some("flac"),
        _ => None,
    }
}

fn has_audio_extension(name: &str) -> bool {
    let Some(ext) = Path::new(name).extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "oga" | "ogg" | "opus" | "mp3" | "m4a" | "mp4" | "wav" | "flac" | "aac"
    )
}

fn parse_chat_ids(raw: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = raw
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter_map(|part| {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                None
            } else {
                trimmed.parse::<i64>().ok()
            }
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn parse_command(text: &str) -> Option<(String, &str)> {
    let text = text.trim_start();
    let without_slash = text.strip_prefix('/')?;
    let mut parts = without_slash.splitn(2, char::is_whitespace);
    let raw = parts.next()?.trim();
    let rest = parts.next().unwrap_or("").trim_start();
    let cmd = raw
        .split_once('@')
        .map(|(name, _)| name)
        .unwrap_or(raw)
        .to_ascii_lowercase();
    if cmd.is_empty() {
        None
    } else {
        Some((cmd, rest))
    }
}

fn resolve_selector(
    sessions: &[Instance],
    selector: &str,
    last_list: Option<&Vec<String>>,
) -> Result<Option<String>> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Ok(None);
    }

    if let Ok(index) = selector.parse::<usize>() {
        if index > 0 {
            if let Some(list) = last_list {
                if let Some(id) = list.get(index - 1) {
                    if sessions.iter().any(|s| s.id == *id) {
                        return Ok(Some(id.clone()));
                    }
                }
            }
        }
    }

    let exact: Vec<&Instance> = sessions
        .iter()
        .filter(|s| s.id == selector || s.title.eq_ignore_ascii_case(selector))
        .collect();
    if exact.len() == 1 {
        return Ok(Some(exact[0].id.clone()));
    }
    if exact.len() > 1 {
        return Err(anyhow!("session selector is ambiguous"));
    }

    let lower = selector.to_ascii_lowercase();
    let prefix: Vec<&Instance> = sessions
        .iter()
        .filter(|s| s.id.starts_with(selector))
        .collect();
    if prefix.len() == 1 {
        return Ok(Some(prefix[0].id.clone()));
    }
    if prefix.len() > 1 {
        return Err(anyhow!("session id prefix is ambiguous"));
    }

    let title_contains: Vec<&Instance> = sessions
        .iter()
        .filter(|s| s.title.to_ascii_lowercase().contains(&lower))
        .collect();
    if title_contains.len() == 1 {
        return Ok(Some(title_contains[0].id.clone()));
    }
    if title_contains.len() > 1 {
        return Err(anyhow!("session title selector is ambiguous"));
    }

    Ok(None)
}

fn format_sessions(sessions: &[Instance]) -> String {
    let mut out = String::from("Sessions:\n");
    for (i, session) in sessions.iter().take(40).enumerate() {
        let short_id: String = session.id.chars().take(8).collect();
        let mode = if session.is_cockpit_mode() {
            "cockpit"
        } else {
            "tmux"
        };
        out.push_str(&format!(
            "{}. {} | {:?} | {} | {} | {}\n",
            i + 1,
            short_id,
            session.status,
            session.tool,
            mode,
            session.title
        ));
    }
    if sessions.len() > 40 {
        out.push_str(&format!("...and {} more\n", sessions.len() - 40));
    }
    out.push_str("Use /use <number> to select.");
    out
}

fn format_send_error(error: &SendTextError) -> String {
    match error {
        SendTextError::ReadOnly => "AoE serve is in read-only mode.".to_string(),
        SendTextError::MessageEmpty => "Message cannot be empty.".to_string(),
        SendTextError::NotFound => "Session not found.".to_string(),
        SendTextError::NotRunning => "Session is not running.".to_string(),
        SendTextError::Transient(status) => {
            format!("Session is mid-lifecycle ({status:?}); try again shortly.")
        }
        SendTextError::CockpitModeUnsupported => {
            "This is a cockpit session, but Telegram could not route it through tmux.".to_string()
        }
        SendTextError::Tmux(e) => format!("tmux send failed: {e}"),
        SendTextError::Internal => "Send failed due to an internal error.".to_string(),
    }
}

fn help_text(authorized: bool) -> &'static str {
    if authorized {
        "AoE Telegram commands:\n/sessions - list sessions\n/use <number, id, or title> - select a session\n/status - selected session status\n/tail [lines] - show recent tmux output\n/whoami - show this chat id\n\nSend normal text or a voice note to the selected session. Voice notes are transcribed locally with NVIDIA Parakeet v2."
    } else {
        "AoE Telegram bridge is locked. Use /claim <code> from the private AoE Telegram config, or /whoami to get this chat id."
    }
}

fn generate_claim_code() -> String {
    crate::server::generate_token().chars().take(10).collect()
}

fn split_telegram_text(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        if count >= MAX_TELEGRAM_TEXT_CHARS {
            chunks.push(current);
            current = String::new();
            count = 0;
        }
        current.push(ch);
        count += 1;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn truncate_from_end(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(32);
    let tail: String = text
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("[truncated to last {keep} chars]\n{tail}")
}

fn telegram_url(token: &str, method: &str) -> String {
    format!("https://api.telegram.org/bot{token}/{method}")
}

async fn parse_telegram_response<T: for<'de> Deserialize<'de>>(
    res: reqwest::Response,
) -> Result<T> {
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if status != StatusCode::OK {
        return Err(anyhow!("Telegram HTTP {status}: {}", truncate_log(&body)));
    }
    let parsed: TelegramApiResponse<T> =
        serde_json::from_str(&body).context("parse Telegram response")?;
    if !parsed.ok {
        return Err(anyhow!(
            "Telegram API error: {}",
            parsed.description.unwrap_or_else(|| "unknown".to_string())
        ));
    }
    parsed
        .result
        .ok_or_else(|| anyhow!("Telegram response missing result"))
}

fn truncate_log(text: &str) -> String {
    let max = 240;
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("...");
    out
}

async fn save_config(path: &Path, config: &TelegramConfig) -> Result<()> {
    let body = toml::to_string_pretty(config).context("serialize telegram config")?;
    let tmp = path.with_extension("toml.tmp");
    write_secret_file(&tmp, &body)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(unix)]
async fn write_secret_file(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(contents.as_bytes()).await?;
    file.flush().await
}

#[cfg(not(unix))]
async fn write_secret_file(path: &Path, contents: &str) -> std::io::Result<()> {
    tokio::fs::write(path, contents).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_chat_ids_accepts_common_separators() {
        assert_eq!(parse_chat_ids("123, 456\n123; -789"), vec![-789, 123, 456]);
    }

    #[test]
    fn parse_command_strips_bot_username() {
        assert_eq!(
            parse_command("/use@Pi_agent_l337_bot 1"),
            Some(("use".to_string(), "1"))
        );
    }

    #[test]
    fn split_telegram_text_preserves_text() {
        let text = "x".repeat(MAX_TELEGRAM_TEXT_CHARS + 12);
        let chunks = split_telegram_text(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn truncate_from_end_keeps_short_text() {
        assert_eq!(truncate_from_end("hello", 10), "hello");
    }

    #[test]
    fn voice_input_accepts_voice_note() {
        let message = TelegramMessage {
            message_id: 1,
            chat: TelegramChat { id: 42 },
            text: None,
            voice: Some(TelegramAudioRef {
                file_id: "voice-file".to_string(),
                file_unique_id: None,
                mime_type: Some("audio/ogg".to_string()),
                file_size: Some(123),
                duration: Some(4),
            }),
            audio: None,
            document: None,
        };

        let voice = voice_input_from_message(&message).expect("voice input");
        assert_eq!(voice.file_id, "voice-file");
        assert_eq!(voice.label, "voice note");
    }

    #[test]
    fn build_parakeet_command_uses_v2_model_and_txt_output() {
        let config = TelegramConfig {
            parakeet_command: vec!["parakeet-mlx".to_string()],
            ..TelegramConfig::default()
        };
        let (_program, args) =
            build_parakeet_command(&config, Path::new("/tmp/audio.oga"), Path::new("/tmp/out"))
                .expect("command");
        let rendered: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert!(rendered.contains(&DEFAULT_PARAKEET_MODEL.to_string()));
        assert!(rendered.windows(2).any(|w| w == ["--output-format", "txt"]));
    }
}
