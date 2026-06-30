use std::env;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use super::AppState;

const DEFAULT_MODEL: &str = "gpt-4o-transcribe";
const OPENAI_TRANSCRIPTIONS_URL: &str = "https://api.openai.com/v1/audio/transcriptions";
const MAX_AUDIO_BYTES: usize = 24 * 1024 * 1024;
const MAX_CONCURRENT_TRANSCRIPTIONS: usize = 2;
static TRANSCRIPTION_IN_FLIGHT: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_TRANSCRIPTIONS));

const DEFAULT_PROMPT: &str = "This is dictated text for a technical coding assistant. \
Preserve developer terms, product names, acronyms, package names, and code-like words. \
Examples include Agent of Empires, AoE, Supabase, TypeScript, JavaScript, React, Rust, \
Cargo, tmux, GitHub, GitLab, npm, pnpm, Vite, Tailwind, Docker, Kubernetes, Postgres, \
SQLite, WebSocket, PWA, API, CLI, TUI, MCP, ACP, OpenAI, Whisper, Claude Code, and Codex.";

#[derive(Debug, Clone)]
struct TranscriptionConfig {
    api_key: Option<String>,
    model: String,
    prompt: String,
    language: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TranscriptionStatusResponse {
    available: bool,
    provider: Option<&'static str>,
    model: Option<String>,
    reason: Option<&'static str>,
}

#[derive(Debug, Deserialize)]
struct OpenAiTranscriptionResponse {
    text: String,
}

#[derive(Debug, Serialize)]
pub struct TranscriptionResponse {
    text: String,
    provider: &'static str,
    model: String,
    corrected: bool,
}

pub async fn transcription_status() -> impl IntoResponse {
    let config = TranscriptionConfig::from_env();
    if config.api_key.is_none() {
        return Json(TranscriptionStatusResponse {
            available: false,
            provider: None,
            model: None,
            reason: Some("missing_openai_api_key"),
        });
    }

    Json(TranscriptionStatusResponse {
        available: true,
        provider: Some("openai"),
        model: Some(config.model),
        reason: None,
    })
}

pub async fn transcribe_audio(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    audio: Bytes,
) -> impl IntoResponse {
    let config = TranscriptionConfig::from_env();
    if let Err((status, message)) =
        validate_transcription_request(state.read_only, audio.len(), config.api_key.is_some())
    {
        return (status, message).into_response();
    }
    let api_key = config
        .api_key
        .as_deref()
        .expect("validated transcription config must include an API key");

    let Ok(_permit) = TRANSCRIPTION_IN_FLIGHT.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many transcription requests in flight",
        )
            .into_response();
    };

    match transcribe_with_openai(&config, api_key, &headers, audio).await {
        Ok(raw_text) => {
            let corrected_text = correct_technical_terms(&raw_text);
            let corrected = corrected_text != raw_text;
            (
                StatusCode::OK,
                Json(TranscriptionResponse {
                    text: corrected_text,
                    provider: "openai",
                    model: config.model,
                    corrected,
                }),
            )
                .into_response()
        }
        Err(message) => (StatusCode::BAD_GATEWAY, message).into_response(),
    }
}

fn validate_transcription_request(
    read_only: bool,
    audio_len: usize,
    configured: bool,
) -> Result<(), (StatusCode, &'static str)> {
    if read_only {
        return Err((StatusCode::FORBIDDEN, "server is in read-only mode"));
    }
    if audio_len == 0 {
        return Err((StatusCode::BAD_REQUEST, "empty audio body"));
    }
    if audio_len > MAX_AUDIO_BYTES {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "audio body too large"));
    }
    if !configured {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "server transcription is not configured",
        ));
    }
    Ok(())
}

async fn transcribe_with_openai(
    config: &TranscriptionConfig,
    api_key: &str,
    headers: &HeaderMap,
    audio: Bytes,
) -> Result<String, String> {
    let content_type = request_content_type(headers);
    let part = reqwest::multipart::Part::bytes(audio.to_vec())
        .file_name(filename_for_content_type(content_type))
        .mime_str(content_type)
        .or_else(|_| {
            reqwest::multipart::Part::bytes(audio.to_vec())
                .file_name("dictation.webm")
                .mime_str("application/octet-stream")
        })
        .map_err(|e| format!("failed to build audio upload: {e}"))?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", config.model.clone())
        .text("prompt", config.prompt.clone())
        .text("response_format", "json");
    if let Some(language) = &config.language {
        form = form.text("language", language.clone());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|e| format!("failed to build transcription client: {e}"))?;

    let response = client
        .post(OPENAI_TRANSCRIPTIONS_URL)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("transcription request failed: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("failed to read transcription response: {e}"))?;

    if !status.is_success() {
        tracing::warn!(
            target: "http.api.transcription",
            %status,
            response_bytes = body.len(),
            "transcription provider returned an error"
        );
        return Err(format!("transcription provider returned {status}"));
    }

    let parsed: OpenAiTranscriptionResponse =
        serde_json::from_str(&body).map_err(|e| format!("invalid transcription response: {e}"))?;
    let text = parsed.text.trim();
    if text.is_empty() {
        return Err("transcription provider returned empty text".to_string());
    }
    Ok(text.to_string())
}

impl TranscriptionConfig {
    fn from_env() -> Self {
        let provider =
            env::var("AOE_TRANSCRIPTION_PROVIDER").unwrap_or_else(|_| "openai".to_string());
        let api_key = if provider.eq_ignore_ascii_case("openai") {
            env::var("OPENAI_API_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty())
        } else {
            None
        };
        let model = env::var("AOE_TRANSCRIPTION_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let extra_glossary = env::var("AOE_TRANSCRIPTION_GLOSSARY").unwrap_or_default();
        let prompt = env::var("AOE_TRANSCRIPTION_PROMPT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                if extra_glossary.trim().is_empty() {
                    DEFAULT_PROMPT.to_string()
                } else {
                    format!("{DEFAULT_PROMPT} Additional project glossary: {extra_glossary}")
                }
            });
        let language = env::var("AOE_TRANSCRIPTION_LANGUAGE")
            .ok()
            .filter(|value| !value.trim().is_empty());

        Self {
            api_key,
            model,
            prompt,
            language,
        }
    }
}

fn request_content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("audio/webm")
}

fn filename_for_content_type(content_type: &str) -> &'static str {
    if content_type.contains("mpeg") || content_type.contains("mp3") {
        "dictation.mp3"
    } else if content_type.contains("mp4") {
        "dictation.mp4"
    } else if content_type.contains("wav") {
        "dictation.wav"
    } else {
        "dictation.webm"
    }
}

pub fn correct_technical_terms(text: &str) -> String {
    let replacements = [
        (r"\bsuper\s+bass\b", "Supabase"),
        (r"\bsuperbase\b", "Supabase"),
        (r"\btype\s+script\b", "TypeScript"),
        (r"\bjava\s+script\b", "JavaScript"),
        (r"\bpost\s+gress\b", "Postgres"),
        (r"\bpostgre\s*s\s*q\s*l\b", "PostgreSQL"),
        (r"\bweb\s+socket\b", "WebSocket"),
        (r"\bgithub\b", "GitHub"),
        (r"\bgit\s+hub\b", "GitHub"),
        (r"\bgit\s+lab\b", "GitLab"),
        (r"\bt\s+mux\b", "tmux"),
        (r"\bnode\s+j\s+s\b", "Node.js"),
        (r"\breact\s+query\b", "TanStack Query"),
        (r"\btail\s+wind\b", "Tailwind"),
        (r"\bopen\s+a\s+i\b", "OpenAI"),
        (r"\bwhispe?r\b", "Whisper"),
        (r"\bclaude\s+coat\b", "Claude Code"),
        (r"\bclaude\s+code\b", "Claude Code"),
        (r"\bco\s+dex\b", "Codex"),
    ];

    let mut corrected = text.to_string();
    for (pattern, replacement) in replacements {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .expect("technical correction regex must compile");
        corrected = re.replace_all(&corrected, replacement).into_owned();
    }
    corrected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrects_common_technical_misrecognitions() {
        assert_eq!(
            correct_technical_terms("Use Super Bass with Type Script and git hub."),
            "Use Supabase with TypeScript and GitHub.",
        );
    }

    #[test]
    fn preserves_unrelated_text() {
        assert_eq!(
            correct_technical_terms("Review the diff before sending."),
            "Review the diff before sending.",
        );
    }

    #[test]
    fn uses_audio_filename_extensions_for_uploads() {
        assert_eq!(filename_for_content_type("audio/mpeg"), "dictation.mp3");
        assert_eq!(
            filename_for_content_type("audio/webm;codecs=opus"),
            "dictation.webm"
        );
    }

    #[test]
    fn rejects_unavailable_or_unsafe_requests_before_provider_call() {
        assert_eq!(
            validate_transcription_request(true, 1024, true),
            Err((StatusCode::FORBIDDEN, "server is in read-only mode")),
        );
        assert_eq!(
            validate_transcription_request(false, 0, true),
            Err((StatusCode::BAD_REQUEST, "empty audio body")),
        );
        assert_eq!(
            validate_transcription_request(false, MAX_AUDIO_BYTES + 1, true),
            Err((StatusCode::PAYLOAD_TOO_LARGE, "audio body too large")),
        );
        assert_eq!(
            validate_transcription_request(false, 1024, false),
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "server transcription is not configured"
            )),
        );
    }
}
