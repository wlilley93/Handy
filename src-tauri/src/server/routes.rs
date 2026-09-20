//! HTTP surface of the local transcription API.
//!
//! The shape is OpenAI's `/v1/audio/transcriptions` so existing clients work
//! unchanged, plus a `/healthz` that answers without loading a model and a
//! WebSocket that exposes the live streaming path the hotkey already uses.

use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket},
        DefaultBodyLimit, Multipart, Query, State, WebSocketUpgrade,
    },
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tauri::{Listener, Manager};
use tauri_specta::Event;

use super::audio::{decode_pcm16, decode_wav, MAX_AUDIO_SECS, TARGET_HZ};
use super::ServerState;
use crate::managers::model::ModelManager;
use crate::managers::transcription::StreamTextEvent;
use crate::settings::get_settings;

/// Largest upload accepted. Generous enough for ten minutes of 44.1 kHz stereo
/// WAV, which is what `MAX_AUDIO_SECS` allows once decoded.
const MAX_UPLOAD_BYTES: usize = 128 * 1024 * 1024;

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(list_models))
        .route("/v1/audio/transcriptions", post(transcriptions))
        .route("/v1/audio/stream", get(stream))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}

// ---------------------------------------------------------------- errors

/// An error the client should see, with the status it maps to. Internal detail
/// (paths, engine internals) never reaches here — the message is the one the
/// caller can act on.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token".to_string(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // OpenAI's error envelope, so a client's existing error handling works.
        (
            self.status,
            Json(json!({ "error": { "message": self.message, "type": "invalid_request_error" } })),
        )
            .into_response()
    }
}

// ------------------------------------------------------------------ auth

/// Compare in constant time. A token check that returns early on the first
/// wrong byte leaks the token's prefix to anyone who can time the endpoint.
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Decide whether a request is authorised, with no HTTP types in it.
///
/// `expected` is `None` when no token is configured, which
/// `ServerConfig::resolve` only permits on loopback — so "no token" here means
/// "the caller is already a process on this machine", not "anyone may call".
fn authorize(expected: Option<&str>, header: Option<&str>, query: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    // A header wins over the query string, but a wrong header does not fall
    // through to the query: a client that presents a credential is judged on
    // the one it presented.
    match header.or(query) {
        Some(t) => secret_eq(t, expected),
        None => false,
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
}

fn check_auth(
    state: &ServerState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<(), ApiError> {
    if authorize(state.token.as_deref(), bearer(headers), query_token) {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

// --------------------------------------------------------------- healthz

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    /// The model the app would use for a request that names none.
    selected_model: String,
    /// Whether an engine is currently resident. False is normal — the unload
    /// timeout releases it, and the next request loads it again.
    model_loaded: bool,
    backend: Option<String>,
}

/// Unauthenticated on purpose: it reports no transcript and no token, and a
/// supervisor needs to answer "is it up?" without holding the credential.
async fn healthz(State(state): State<ServerState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        selected_model: get_settings(&state.app).selected_model,
        model_loaded: state.transcription.is_model_loaded(),
        backend: state.transcription.current_backend(),
    })
}

#[derive(Serialize)]
struct ModelRow {
    id: String,
    object: &'static str,
    owned_by: &'static str,
    /// Extension to OpenAI's shape: a model that is not downloaded cannot be
    /// used, and a client should be able to see that before it uploads audio.
    downloaded: bool,
    supports_streaming: bool,
}

async fn list_models(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    // No query-string token on the HTTP routes: URLs end up in proxy and shell
    // history in a way headers do not. The WebSocket accepts one only because a
    // browser cannot set a header on an upgrade.
    check_auth(&state, &headers, None)?;
    let manager = state.app.state::<Arc<ModelManager>>();
    let rows: Vec<ModelRow> = manager
        .get_available_models()
        .into_iter()
        .map(|m| ModelRow {
            id: m.id,
            object: "model",
            owned_by: "handy",
            downloaded: m.is_downloaded,
            supports_streaming: m.supports_streaming,
        })
        .collect();
    Ok(Json(json!({ "object": "list", "data": rows })))
}

// -------------------------------------------------------- transcriptions

#[derive(Default)]
struct TranscriptionRequest {
    file: Option<Bytes>,
    model: Option<String>,
    response_format: Option<String>,
}

async fn transcriptions(
    State(state): State<ServerState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    check_auth(&state, &headers, None)?;

    let req = read_multipart(multipart).await?;
    let bytes = req
        .file
        .ok_or_else(|| ApiError::bad_request("no `file` field in the request"))?;
    if bytes.is_empty() {
        return Err(ApiError::bad_request("`file` is empty"));
    }

    let (samples, source_rate) = decode_wav(&bytes).map_err(|e| {
        // Compressed uploads are the common case of this error, so say what is
        // accepted rather than only what failed.
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("{e}. This server accepts WAV only (any rate, any channel count)."),
        )
    })?;
    if samples.is_empty() {
        return Err(ApiError::bad_request("decoded audio contains no samples"));
    }
    let duration = samples.len() as f64 / TARGET_HZ as f64;

    let text = run_transcription(&state, samples, req.model.as_deref()).await?;

    Ok(match req.response_format.as_deref().unwrap_or("json") {
        "text" => text.into_response(),
        "verbose_json" => Json(json!({
            "task": "transcribe",
            "language": get_settings(&state.app).selected_language,
            "duration": duration,
            "source_sample_rate": source_rate,
            "text": text,
        }))
        .into_response(),
        "json" => Json(json!({ "text": text })).into_response(),
        other => {
            return Err(ApiError::bad_request(format!(
                "unsupported response_format `{other}` (use json, text or verbose_json)"
            )))
        }
    })
}

async fn read_multipart(mut multipart: Multipart) -> Result<TranscriptionRequest, ApiError> {
    let mut req = TranscriptionRequest::default();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                return Err(ApiError::bad_request(format!(
                    "could not read the multipart body: {e}"
                )))
            }
        };
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => {
                req.file = Some(field.bytes().await.map_err(|e| {
                    ApiError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!("could not read `file`: {e}"),
                    )
                })?)
            }
            "model" => req.model = field.text().await.ok().filter(|s| !s.is_empty()),
            "response_format" => req.response_format = field.text().await.ok(),
            // language / prompt / temperature are accepted and ignored so an
            // OpenAI client's default payload does not 400. Handy takes the
            // language from its own settings.
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    Ok(req)
}

/// Load the requested model if needed, then transcribe off the async runtime.
///
/// `transcribe` blocks for the length of the inference, so it must not run on
/// a tokio worker — one long request would stall every other connection,
/// including `/healthz`.
async fn run_transcription(
    state: &ServerState,
    samples: Vec<f32>,
    model: Option<&str>,
) -> Result<String, ApiError> {
    if let Some(id) = model {
        let current = get_settings(&state.app).selected_model;
        if id != current {
            // A per-request model swap changes what the hotkey uses too: there
            // is one engine. That is the deliberate trade for not holding two
            // copies of the weights.
            state
                .transcription
                .load_model(id)
                .map_err(|e| ApiError::bad_request(format!("could not load model `{id}`: {e}")))?;
        }
    }

    state.transcription.initiate_model_load();

    if state.show_overlay {
        crate::overlay::show_transcribing_overlay(&state.app);
    }

    let tm = state.transcription.clone();
    let result = tauri::async_runtime::spawn_blocking(move || tm.transcribe(samples)).await;

    if state.show_overlay {
        crate::overlay::hide_recording_overlay(&state.app);
    }

    match result {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("transcription failed: {e}"),
        )),
        Err(e) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("transcription task did not finish: {e}"),
        )),
    }
}

// ---------------------------------------------------------------- stream

#[derive(Deserialize)]
struct StreamParams {
    /// Sample rate of the PCM16 frames the client will send. Defaults to the
    /// engine's own rate.
    #[serde(default)]
    sample_rate: Option<usize>,
    /// WebSocket clients cannot set an Authorization header from a browser, so
    /// the token is accepted here too.
    #[serde(default)]
    token: Option<String>,
}

async fn stream(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<StreamParams>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    check_auth(&state, &headers, params.token.as_deref())?;

    let model_id = get_settings(&state.app).selected_model;
    let streams = state
        .app
        .state::<Arc<ModelManager>>()
        .get_model_info(&model_id)
        .map(|m| m.supports_streaming)
        .unwrap_or(false);
    if !streams {
        // Failing here, before the upgrade, gives the client an HTTP status it
        // can read. A close frame after a successful upgrade is far easier to
        // mistake for a network problem.
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "the selected model `{model_id}` does not support live streaming; \
                 use POST /v1/audio/transcriptions instead"
            ),
        ));
    }

    let rate = params.sample_rate.unwrap_or(TARGET_HZ);
    if rate == 0 {
        return Err(ApiError::bad_request("sample_rate must be greater than 0"));
    }
    Ok(ws.on_upgrade(move |socket| run_stream(socket, state, rate)))
}

/// One streaming session: PCM16 in as binary frames, partial transcripts out
/// as JSON, a final transcript on commit or close.
async fn run_stream(socket: WebSocket, state: ServerState, rate: usize) {
    let (mut sink, mut source) = socket.split();

    state.transcription.initiate_model_load();
    if !state.transcription.is_model_loaded() {
        let _ = sink
            .send(Message::Text(
                json!({"type": "error", "message": "model is still loading, retry shortly"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }

    // Partials reach the overlay as a Tauri event, so that is where the server
    // reads them from too — rather than a second tap inside the engine that
    // could drift from what the UI shows.
    let (partial_tx, mut partial_rx) = tokio::sync::mpsc::unbounded_channel::<StreamTextEvent>();
    let listener = StreamTextEvent::listen(&state.app, move |event| {
        let _ = partial_tx.send(event.payload);
    });

    state.transcription.start_stream();
    if state.show_overlay {
        crate::overlay::show_streaming_overlay(&state.app);
    }

    let _ = sink
        .send(Message::Text(
            json!({"type": "ready", "sample_rate": rate})
                .to_string()
                .into(),
        ))
        .await;

    let mut fed_samples: usize = 0;
    let max_samples = (MAX_AUDIO_SECS * TARGET_HZ as f64) as usize;
    let router = state.transcription.stream_router();
    let mut commit = false;

    loop {
        tokio::select! {
            Some(partial) = partial_rx.recv() => {
                let msg = json!({
                    "type": "partial",
                    "committed": partial.committed,
                    "tentative": partial.tentative,
                });
                if sink.send(Message::Text(msg.to_string().into())).await.is_err() {
                    break;
                }
            }
            incoming = source.next() => {
                match incoming {
                    Some(Ok(Message::Binary(bytes))) => {
                        match decode_pcm16(&bytes, rate) {
                            Ok(frame) => {
                                fed_samples += frame.len();
                                if fed_samples > max_samples {
                                    let _ = sink.send(Message::Text(json!({
                                        "type": "error",
                                        "message": format!("stream exceeded the {MAX_AUDIO_SECS:.0}s limit"),
                                    }).to_string().into())).await;
                                    break;
                                }
                                router.feed(&frame);
                            }
                            Err(e) => {
                                let _ = sink.send(Message::Text(json!({
                                    "type": "error", "message": e.to_string(),
                                }).to_string().into())).await;
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        // A commit closes the audio and asks for the final text.
                        // Anything else is ignored rather than fatal: a client
                        // sending a keepalive should not lose its session.
                        if serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                            .as_deref()
                            == Some("commit")
                        {
                            commit = true;
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        // A clean close still wants its transcript — the client
                        // may simply have finished speaking and hung up.
                        commit = true;
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        log::warn!("Streaming client dropped: {}", e);
                        break;
                    }
                }
            }
        }
    }

    state.app.unlisten(listener);

    if commit {
        if state.show_overlay {
            crate::overlay::show_transcribing_overlay(&state.app);
        }
        let tm = state.transcription.clone();
        let final_text = tauri::async_runtime::spawn_blocking(move || tm.finalize_stream()).await;
        let payload = match final_text {
            Ok(Ok(Some(text))) => json!({"type": "final", "text": text}),
            Ok(Ok(None)) => json!({"type": "final", "text": ""}),
            Ok(Err(e)) => json!({"type": "error", "message": format!("finalize failed: {e}")}),
            Err(e) => json!({"type": "error", "message": format!("finalize did not finish: {e}")}),
        };
        let _ = sink.send(Message::Text(payload.to_string().into())).await;
    } else {
        state.transcription.cancel_stream();
    }

    if state.show_overlay {
        crate::overlay::hide_recording_overlay(&state.app);
    }
    let _ = sink.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::{authorize, secret_eq};

    #[test]
    fn secret_eq_matches_only_the_exact_token() {
        assert!(secret_eq("swordfish", "swordfish"));
        assert!(!secret_eq("swordfish", "swordfisi"));
        assert!(!secret_eq("swordfish", "swordfis"));
        assert!(!secret_eq("", "x"));
        assert!(secret_eq("", ""));
    }

    #[test]
    fn no_configured_token_lets_the_caller_through() {
        // Only reachable on loopback; see ServerConfig::resolve.
        assert!(authorize(None, None, None));
    }

    #[test]
    fn a_configured_token_is_required() {
        assert!(!authorize(Some("swordfish"), None, None));
        assert!(!authorize(Some("swordfish"), Some("wrong"), None));
        assert!(!authorize(Some("swordfish"), None, Some("wrong")));
    }

    #[test]
    fn either_the_header_or_the_query_may_carry_it() {
        assert!(authorize(Some("swordfish"), Some("swordfish"), None));
        assert!(authorize(Some("swordfish"), None, Some("swordfish")));
    }

    #[test]
    fn a_wrong_header_does_not_fall_through_to_the_query() {
        // Otherwise a client could brute-force the header while holding a
        // valid query token, and every attempt would still return 200.
        assert!(!authorize(
            Some("swordfish"),
            Some("wrong"),
            Some("swordfish")
        ));
    }
}
