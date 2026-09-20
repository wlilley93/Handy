//! HTTP surface of the local transcription API.
//!
//! The shape is OpenAI's `/v1/audio/transcriptions` so existing clients work
//! unchanged, plus a `/healthz` that answers without loading a model and a
//! WebSocket that exposes the live streaming path the hotkey already uses.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

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

use super::audio::{decode_pcm16, decode_wav, MAX_AUDIO_SECS, TARGET_HZ};
use super::ServerState;
use crate::managers::transcription::StreamTextEvent;

/// Largest upload accepted. Generous enough for ten minutes of 44.1 kHz stereo
/// WAV, which is what `MAX_AUDIO_SECS` allows once decoded.
pub(super) const MAX_UPLOAD_BYTES: usize = 128 * 1024 * 1024;

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
pub(super) struct ApiError {
    pub(super) status: StatusCode,
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
        selected_model: state.host.settings().selected_model,
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
    let manager = state.host.models();
    let rows: Vec<ModelRow> = manager
        .available()
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

    // Say "too big" when it is too big. `DefaultBodyLimit` counts bytes as it
    // reads, and its rejection surfaces as a multipart parse error — so an
    // oversized upload was answered "could not read the multipart body",
    // which sends the caller looking for a malformed request. A client that
    // declares its length gets a straight answer here; the read limit stays
    // as the backstop for one that does not, or lies.
    if let Some(declared) = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if declared > MAX_UPLOAD_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "upload is {declared} bytes, over the {MAX_UPLOAD_BYTES} byte limit"
                ),
            ));
        }
    }

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
            "language": state.host.settings().selected_language,
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
        let current = state.host.settings().selected_model;
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
        state.host.show_transcribing_overlay();
    }

    let tm = state.transcription.clone();
    let result = tauri::async_runtime::spawn_blocking(move || tm.transcribe(samples)).await;

    if state.show_overlay {
        state.host.hide_recording_overlay();
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
pub(super) struct StreamParams {
    /// Sample rate of the PCM16 frames the client will send. Defaults to the
    /// engine's own rate. In the Codex dialect `session.start` supplies it
    /// instead, and wins.
    #[serde(default)]
    pub(super) sample_rate: Option<usize>,
    /// WebSocket clients cannot set an Authorization header from a browser, so
    /// the token is accepted here too.
    #[serde(default)]
    pub(super) token: Option<String>,
    /// `codex` selects the OpenCodex dictation dialect. Omitted means native.
    /// Stated in the URL rather than sniffed, because the two dialects differ
    /// on who speaks first: a native client waits for `ready` on connect, a
    /// Codex client sends `session.start` and waits for `session.started`. A
    /// socket that guessed would deadlock one of them.
    #[serde(default)]
    pub(super) dialect: Option<String>,
}

/// What a stream request must satisfy before the socket is upgraded.
///
/// Pure, and separate from the handler on purpose: `WebSocketUpgrade` runs as
/// an extractor, so it rejects a non-upgrade request with 426 *before* any
/// handler body executes. Every refusal below was therefore unreachable by a
/// test while it lived inside `stream()` — not hard to reach, impossible.
pub(super) fn validate_stream(
    model_id: &str,
    supports_streaming: bool,
    params: &StreamParams,
) -> Result<(usize, Dialect), ApiError> {
    if !supports_streaming {
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
    let dialect = match params.dialect.as_deref() {
        None | Some("") | Some("native") => Dialect::Native,
        Some("codex") => Dialect::Codex,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unknown dialect `{other}` (use native or codex)"
            )))
        }
    };
    Ok((rate, dialect))
}

async fn stream(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<StreamParams>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    check_auth(&state, &headers, params.token.as_deref())?;

    let model_id = state.host.settings().selected_model;
    let streams = state
        .host
        .models()
        .info(&model_id)
        .map(|m| m.supports_streaming)
        .unwrap_or(false);
    let (rate, dialect) = validate_stream(&model_id, streams, &params)?;
    Ok(ws.on_upgrade(move |socket| run_stream(socket, state, rate, dialect)))
}

/// How long a streaming client waits for a cold engine before giving up. A
/// first GGUF load off disk onto Metal is measured in seconds, not milliseconds.
const MODEL_LOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// Poll until the engine is resident or the deadline passes.
///
/// Polling rather than a condvar because `TranscriptionManager` exposes the
/// loading state as a bool, and reaching into its internals from the server
/// would couple the two; the wait happens once per session, off the hot path.
pub(super) async fn wait_for_model(state: &ServerState, limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if state.transcription.is_model_loaded() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Which wire dialect a connected client speaks.
///
/// Two clients matter and they disagree about framing, so the socket adapts
/// rather than forcing one of them through a translating proxy:
///
/// * `Native` — raw little-endian PCM16 in binary frames, `{"type":"commit"}`
///   to finish. What a script or a shell client would write by hand.
/// * `Codex` — the OpenCodex streaming-dictation extension: base64 PCM16
///   inside JSON text frames, `session.start` / `audio.append` /
///   `session.close`, answered with `session.started`, `transcript.segment`,
///   `transcript.final` and `session.updated`. OpenCodex relays dictation
///   frames verbatim, so a backend it can use has to speak this itself.
///
/// The dialect is decided by the first frame and never changes after: a binary
/// frame means Native, a `session.start` text frame means Codex.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Dialect {
    Native,
    Codex,
}

/// Classify one client frame without committing to a dialect.
#[derive(Debug, PartialEq)]
enum ClientFrame {
    /// Codex `session.start`, carrying the sample rate.
    Start(usize),
    /// PCM16 samples to feed, already decoded from whichever framing carried them.
    Audio(Vec<u8>),
    /// The client has finished speaking and wants the final transcript.
    Commit,
    /// Understood, but nothing to do — a keepalive, or an unknown type that is
    /// not worth dropping the session over.
    Ignore,
    /// Malformed enough that continuing would be guessing.
    Bad(String),
}

/// Parse a Codex-dialect text frame.
fn parse_codex_frame(text: &str) -> ClientFrame {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return ClientFrame::Bad("frame is not JSON".into());
    };
    match value.get("type").and_then(|t| t.as_str()) {
        Some("session.start") => {
            // `config.sample_rate_hz` is required by the dialect, so a frame
            // without it is refused rather than assumed.
            //
            // Assuming 16 kHz was the previous behaviour and it fails
            // silently: a client sending 24 kHz audio had it read as 16 kHz,
            // never resampled, and transcribed slightly wrong — measured, the
            // same clip came back "dog Pack my box" instead of "dog. Pack my
            // box". OpenCodex's relay rejects such a frame outright, so no
            // real client sends one; the only thing this leniency bought was a
            // way to be quietly wrong.
            let Some(rate) = value
                .get("config")
                .and_then(|c| c.get("sample_rate_hz"))
                .and_then(|r| r.as_u64())
            else {
                return ClientFrame::Bad(
                    "session.start needs config.sample_rate_hz — see LOCAL_API.md".into(),
                );
            };
            if !(8_000..=192_000).contains(&rate) {
                return ClientFrame::Bad(format!("sample_rate_hz {rate} is outside 8000-192000"));
            }
            ClientFrame::Start(rate as usize)
        }
        Some("audio.append") => match value.get("audio").and_then(|a| a.as_str()) {
            Some(b64) => match B64.decode(b64) {
                Ok(bytes) if bytes.is_empty() => ClientFrame::Ignore,
                Ok(bytes) => ClientFrame::Audio(bytes),
                Err(e) => ClientFrame::Bad(format!("audio is not valid base64: {e}")),
            },
            None => ClientFrame::Bad("audio.append has no `audio` string".into()),
        },
        Some("session.close") | Some("commit") => ClientFrame::Commit,
        _ => ClientFrame::Ignore,
    }
}

/// Parse a Native-dialect text frame. Only `commit` means anything; a
/// keepalive must not end the session.
fn parse_native_text(text: &str) -> ClientFrame {
    match serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
        .as_deref()
    {
        Some("commit") | Some("session.close") => ClientFrame::Commit,
        _ => ClientFrame::Ignore,
    }
}

/// Outgoing events, rendered per dialect so the session body never branches on
/// the wire format while it is running.
struct Wire {
    dialect: Dialect,
    session_id: String,
    utterance_id: String,
    revision: u64,
}

impl Wire {
    fn new(dialect: Dialect) -> Self {
        // Ids only have to be unique within this process's lifetime and
        // distinguishable in a log; the clock gives that without a uuid dep.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self {
            dialect,
            session_id: format!("handy-{stamp:x}"),
            utterance_id: format!("utt-{stamp:x}"),
            revision: 0,
        }
    }

    fn ready(&self, rate: usize) -> serde_json::Value {
        match self.dialect {
            Dialect::Native => json!({"type": "ready", "sample_rate": rate}),
            Dialect::Codex => json!({
                "type": "session.started",
                "session": { "id": self.session_id, "status": "open" },
            }),
        }
    }

    /// A partial transcript. The Codex contract is that a revision replaces the
    /// previous text for the same utterance rather than appending to it, which
    /// is exactly what committed+tentative already is.
    fn partial(&mut self, committed: &str, tentative: &str) -> serde_json::Value {
        match self.dialect {
            Dialect::Native => json!({
                "type": "partial",
                "committed": committed,
                "tentative": tentative,
            }),
            Dialect::Codex => {
                self.revision += 1;
                json!({
                    "type": "transcript.segment",
                    "utterance_id": self.utterance_id,
                    "revision": self.revision,
                    "text": format!("{committed}{tentative}"),
                })
            }
        }
    }

    fn final_text(&mut self, text: &str) -> serde_json::Value {
        match self.dialect {
            Dialect::Native => json!({"type": "final", "text": text}),
            Dialect::Codex => {
                self.revision += 1;
                json!({
                    "type": "transcript.final",
                    "utterance_id": self.utterance_id,
                    "revision": self.revision,
                    "text": text,
                })
            }
        }
    }

    /// Sent after the final transcript so a Codex client knows the session is
    /// done rather than merely quiet.
    fn closed(&self) -> Option<serde_json::Value> {
        match self.dialect {
            Dialect::Native => None,
            Dialect::Codex => Some(json!({
                "type": "session.updated",
                "session": { "id": self.session_id, "status": "closed" },
            })),
        }
    }

    fn error(&self, message: &str) -> serde_json::Value {
        match self.dialect {
            Dialect::Native => json!({"type": "error", "message": message}),
            Dialect::Codex => json!({
                "type": "error",
                "error": { "type": "invalid_request_error", "message": message },
            }),
        }
    }
}

/// One streaming session: PCM16 in, partial transcripts out, a final
/// transcript on commit or close. Speaks whichever dialect the client opens
/// with — see [`Dialect`].
async fn run_stream(socket: WebSocket, state: ServerState, default_rate: usize, dialect: Dialect) {
    let (mut sink, mut source) = socket.split();

    // Wait for the engine rather than refusing. The model unloads on a timer,
    // so "not loaded" is the ordinary state between dictations, not a fault —
    // telling the first caller after every idle period to retry would make a
    // cold start look like a broken backend. The batch path already blocks on
    // the same load inside `transcribe()`.
    state.transcription.initiate_model_load();
    if !wait_for_model(&state, MODEL_LOAD_WAIT).await {
        let wire = Wire::new(dialect);
        let _ = sink
            .send(Message::Text(
                wire.error(&format!(
                    "model did not finish loading within {}s",
                    MODEL_LOAD_WAIT.as_secs()
                ))
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
    let listener = state.host.listen_stream_text(Box::new(move |payload| {
        let _ = partial_tx.send(payload);
    }));

    state.transcription.start_stream();
    if state.show_overlay {
        state.host.show_streaming_overlay();
    }

    let mut wire = Wire::new(dialect);
    let mut rate = default_rate;
    let mut fed_samples: usize = 0;
    let max_samples = (MAX_AUDIO_SECS * TARGET_HZ as f64) as usize;
    let router = state.transcription.stream_router();
    let mut commit = false;

    // The native dialect greets on connect, because its clients block on that
    // first frame. The Codex dialect stays silent until `session.start`, which
    // is what its clients wait to answer.
    if dialect == Dialect::Native {
        let _ = sink
            .send(Message::Text(wire.ready(rate).to_string().into()))
            .await;
    }

    loop {
        tokio::select! {
            Some(partial) = partial_rx.recv() => {
                let msg = wire.partial(&partial.committed, &partial.tentative);
                if sink.send(Message::Text(msg.to_string().into())).await.is_err() {
                    break;
                }
            }
            incoming = source.next() => {
                let frame = match incoming {
                    Some(Ok(Message::Binary(bytes))) => ClientFrame::Audio(bytes.to_vec()),
                    Some(Ok(Message::Text(text))) => {
                        match dialect {
                            Dialect::Codex => parse_codex_frame(&text),
                            Dialect::Native => parse_native_text(&text),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        // A clean close still wants its transcript — the client
                        // may simply have finished speaking and hung up.
                        commit = true;
                        break;
                    }
                    Some(Ok(_)) => ClientFrame::Ignore,
                    Some(Err(e)) => {
                        log::warn!("Streaming client dropped: {}", e);
                        break;
                    }
                };

                match frame {
                    ClientFrame::Start(r) => {
                        // The rate travels in session.start, so it may differ
                        // from the query default. Honour the frame.
                        rate = r;
                        if sink
                            .send(Message::Text(wire.ready(rate).to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    ClientFrame::Audio(bytes) => match decode_pcm16(&bytes, rate) {
                        Ok(frame) => {
                            fed_samples += frame.len();
                            if fed_samples > max_samples {
                                let msg = wire.error(&format!(
                                    "stream exceeded the {MAX_AUDIO_SECS:.0}s limit"
                                ));
                                let _ = sink.send(Message::Text(msg.to_string().into())).await;
                                break;
                            }
                            router.feed(&frame);
                        }
                        Err(e) => {
                            let msg = wire.error(&e.to_string());
                            let _ = sink.send(Message::Text(msg.to_string().into())).await;
                            break;
                        }
                    },
                    ClientFrame::Commit => {
                        commit = true;
                        break;
                    }
                    ClientFrame::Bad(reason) => {
                        let msg = wire.error(&reason);
                        let _ = sink.send(Message::Text(msg.to_string().into())).await;
                        break;
                    }
                    ClientFrame::Ignore => {}
                }
            }
        }
    }

    state.host.unlisten(listener);

    if commit {
        if state.show_overlay {
            state.host.show_transcribing_overlay();
        }
        let tm = state.transcription.clone();
        let final_text = tauri::async_runtime::spawn_blocking(move || tm.finalize_stream()).await;
        let payload = match final_text {
            Ok(Ok(Some(text))) => wire.final_text(&text),
            Ok(Ok(None)) => wire.final_text(""),
            Ok(Err(e)) => wire.error(&format!("finalize failed: {e}")),
            Err(e) => wire.error(&format!("finalize did not finish: {e}")),
        };
        let _ = sink.send(Message::Text(payload.to_string().into())).await;
        if let Some(closed) = wire.closed() {
            let _ = sink.send(Message::Text(closed.to_string().into())).await;
        }
    } else {
        state.transcription.cancel_stream();
    }

    if state.show_overlay {
        state.host.hide_recording_overlay();
    }
    let _ = sink.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn codex_session_start_carries_the_sample_rate() {
        let frame = parse_codex_frame(
            r#"{"type":"session.start","config":{"input_audio_format":"pcm16","sample_rate_hz":48000,"num_channels":1}}"#,
        );
        assert_eq!(frame, ClientFrame::Start(48_000));
    }

    #[test]
    fn codex_session_start_without_a_rate_is_refused() {
        // Not defaulted: sending 24 kHz audio into a session read as 16 kHz
        // transcribes slightly wrong and says nothing.
        let frame = parse_codex_frame(r#"{"type":"session.start"}"#);
        assert!(
            matches!(&frame, ClientFrame::Bad(m) if m.contains("sample_rate_hz")),
            "got {frame:?}"
        );
    }

    #[test]
    fn codex_rejects_a_sample_rate_outside_the_documented_range() {
        // The OpenCodex gateway accepts 8k-192k; anything else would be fed to
        // the resampler as a silent mis-decode.
        assert!(matches!(
            parse_codex_frame(r#"{"type":"session.start","config":{"sample_rate_hz":400}}"#),
            ClientFrame::Bad(_)
        ));
        assert!(matches!(
            parse_codex_frame(r#"{"type":"session.start","config":{"sample_rate_hz":999999}}"#),
            ClientFrame::Bad(_)
        ));
    }

    #[test]
    fn codex_audio_append_decodes_base64() {
        // Two PCM16 samples: 0 and 1.
        let frame = parse_codex_frame(r#"{"type":"audio.append","audio":"AAABAA=="}"#);
        assert_eq!(frame, ClientFrame::Audio(vec![0, 0, 1, 0]));
    }

    #[test]
    fn codex_rejects_audio_that_is_not_base64() {
        assert!(matches!(
            parse_codex_frame(r#"{"type":"audio.append","audio":"not!base64"}"#),
            ClientFrame::Bad(_)
        ));
    }

    #[test]
    fn codex_session_close_commits() {
        assert_eq!(
            parse_codex_frame(r#"{"type":"session.close"}"#),
            ClientFrame::Commit
        );
    }

    #[test]
    fn an_unknown_codex_event_is_ignored_not_fatal() {
        // A keepalive or a future event type must not end someone's dictation.
        assert_eq!(
            parse_codex_frame(r#"{"type":"session.ping"}"#),
            ClientFrame::Ignore
        );
    }

    #[test]
    fn native_text_only_commits_on_commit() {
        assert_eq!(
            parse_native_text(r#"{"type":"commit"}"#),
            ClientFrame::Commit
        );
        assert_eq!(parse_native_text(r#"{"type":"ping"}"#), ClientFrame::Ignore);
        assert_eq!(parse_native_text("not json"), ClientFrame::Ignore);
    }

    #[test]
    fn codex_events_use_the_names_opencodex_relays() {
        let mut wire = Wire::new(Dialect::Codex);
        assert_eq!(wire.ready(48_000)["type"], "session.started");
        assert!(wire.ready(48_000)["session"]["id"].is_string());

        let first = wire.partial("hello ", "wor");
        assert_eq!(first["type"], "transcript.segment");
        assert_eq!(first["text"], "hello wor");
        assert_eq!(first["revision"], 1);

        // A revision replaces the previous text for the same utterance, so the
        // id must not change and the revision must climb.
        let second = wire.partial("hello ", "world");
        assert_eq!(second["utterance_id"], first["utterance_id"]);
        assert_eq!(second["revision"], 2);

        let last = wire.final_text("hello world");
        assert_eq!(last["type"], "transcript.final");
        assert_eq!(last["text"], "hello world");
        assert_eq!(last["utterance_id"], first["utterance_id"]);

        let closed = wire.closed().expect("codex sessions acknowledge the close");
        assert_eq!(closed["type"], "session.updated");
        assert_eq!(closed["session"]["status"], "closed");
    }

    #[test]
    fn native_events_keep_their_own_names() {
        let mut wire = Wire::new(Dialect::Native);
        assert_eq!(wire.ready(16_000)["type"], "ready");
        let partial = wire.partial("a", "b");
        assert_eq!(partial["type"], "partial");
        assert_eq!(partial["committed"], "a");
        assert_eq!(partial["tentative"], "b");
        assert_eq!(wire.final_text("ab")["type"], "final");
        // Native has no close acknowledgment; the socket closing is the signal.
        assert!(wire.closed().is_none());
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
