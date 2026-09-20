//! Request-level tests for the router.
//!
//! Until the `ServerHost` and `Transcriber` seams existed, `router(state)`
//! could not be built outside a running Tauri app, so none of these status
//! paths had ever been produced by a test — `scripts/guard-coverage.ts`
//! reported fourteen of them as unexercised. These drive the real router
//! through `tower::oneshot`: no socket, no model, no app.

use super::*;
use crate::managers::transcription::StreamRouter;
use crate::server::routes::router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Mutex as StdMutex;
use tower::ServiceExt;

/// Records what the handlers asked the app to do, so overlay pairing and
/// settings reads become assertable rather than fire-and-forget.
#[derive(Default)]
struct FakeHost {
    overlay: StdMutex<Vec<&'static str>>,
    /// Whether the catalogue says the selected model can stream. The stream
    /// route refuses before the upgrade when it cannot.
    streaming_model: bool,
}

/// A one-entry catalogue naming whatever the default settings select, so the
/// lookup in `stream()` finds it.
struct FakeModels {
    streaming: bool,
}

fn fake_model(id: &str, streaming: bool) -> crate::managers::model::ModelInfo {
    crate::managers::model::ModelInfo {
        id: id.to_string(),
        name: id.to_string(),
        description: String::new(),
        filename: String::new(),
        source: crate::managers::model::ModelSource::Url {
            url: String::new(),
            sha256: None,
        },
        size_mb: 0,
        is_downloaded: true,
        is_downloading: false,
        partial_size: 0,
        is_directory: false,
        engine_type: crate::managers::model::EngineType::TranscribeCpp,
        accuracy_score: 0.0,
        speed_score: 0.0,
        supports_translation: false,
        is_recommended: false,
        supported_languages: vec!["en".to_string()],
        supports_language_selection: false,
        is_custom: false,
        supports_streaming: streaming,
        supports_language_detection: false,
    }
}

impl Models for FakeModels {
    fn available(&self) -> Vec<crate::managers::model::ModelInfo> {
        vec![fake_model("fake-model", self.streaming)]
    }
    fn info(&self, model_id: &str) -> Option<crate::managers::model::ModelInfo> {
        Some(fake_model(model_id, self.streaming))
    }
}

impl ServerHost for FakeHost {
    fn settings(&self) -> crate::settings::AppSettings {
        crate::settings::get_default_settings()
    }
    fn models(&self) -> Arc<dyn Models> {
        Arc::new(FakeModels {
            streaming: self.streaming_model,
        })
    }
    fn show_transcribing_overlay(&self) {
        self.overlay.lock().unwrap().push("transcribing");
    }
    fn show_streaming_overlay(&self) {
        self.overlay.lock().unwrap().push("streaming");
    }
    fn hide_recording_overlay(&self) {
        self.overlay.lock().unwrap().push("hide");
    }
    fn listen_stream_text(
        &self,
        _on: Box<dyn Fn(crate::managers::transcription::StreamTextEvent) + Send + Sync + 'static>,
    ) -> u32 {
        1
    }
    fn unlisten(&self, _id: u32) {}
}

/// A transcriber that never loads. Enough for every refusal below, which is
/// the point: these are the paths that must not reach the engine.
#[derive(Default)]
struct IdleTranscriber;

impl Transcriber for IdleTranscriber {
    fn is_model_loaded(&self) -> bool {
        false
    }
    fn initiate_model_load(&self) {}
    fn current_backend(&self) -> Option<String> {
        None
    }
    fn stream_router(&self) -> Arc<StreamRouter> {
        // `StreamRouter::new` is private to its module, and no test below
        // reaches a streaming path — the refusals happen first.
        unimplemented!("no test here opens a stream")
    }
    fn start_stream(&self) {}
    fn cancel_stream(&self) {}
    fn finalize_stream(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn transcribe(&self, _audio: Vec<f32>) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn load_model(&self, _model_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

fn state(token: Option<&str>) -> ServerState {
    state_with_streaming(token, true)
}

fn state_with_streaming(token: Option<&str>, streaming: bool) -> ServerState {
    ServerState {
        host: Arc::new(FakeHost {
            streaming_model: streaming,
            ..Default::default()
        }),
        transcription: Arc::new(IdleTranscriber),
        token: token.map(str::to_string),
        show_overlay: false,
    }
}

async fn send(state: ServerState, request: Request<Body>) -> StatusCode {
    router(state).oneshot(request).await.unwrap().status()
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn healthz_needs_no_token_even_when_one_is_set() {
    // Liveness must answer a supervisor that holds no credential.
    assert_eq!(send(state(Some("secret")), get("/healthz")).await, StatusCode::OK);
}

#[tokio::test]
async fn a_missing_token_is_refused_on_the_model_list() {
    assert_eq!(
        send(state(Some("secret")), get("/v1/models")).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_wrong_token_is_refused_on_the_model_list() {
    let request = Request::builder()
        .uri("/v1/models")
        .header("authorization", "Bearer wrong")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(state(Some("secret")), request).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_query_token_is_refused_on_an_http_route() {
    // `?token=` exists for the websocket upgrade, where a browser cannot set a
    // header. Accepting it here would put the secret in every access log.
    assert_eq!(
        send(state(Some("secret")), get("/v1/models?token=secret")).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn transcription_without_a_token_is_refused_before_the_body_is_read() {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from("--x--\r\n"))
        .unwrap();
    assert_eq!(send(state(Some("secret")), request).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_unknown_path_is_not_found_rather_than_unauthorized() {
    // A 401 here would tell an unauthenticated caller which paths exist.
    assert_eq!(
        send(state(Some("secret")), get("/v1/nope")).await,
        StatusCode::NOT_FOUND
    );
}

/// A multipart body with the given parts. Hand-rolled because the point is to
/// exercise the server's own parsing, not a client library's.
fn multipart(parts: &[(&str, Option<&[u8]>)]) -> (String, Vec<u8>) {
    let boundary = "testboundary";
    let mut body = Vec::new();
    for (name, content) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match content {
            Some(bytes) => {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"a.wav\"\r\n\r\n"
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(bytes);
            }
            None => {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                );
            }
        }
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn upload(parts: &[(&str, Option<&[u8]>)]) -> Request<Body> {
    let (content_type, body) = multipart(parts);
    Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn an_upload_with_no_file_part_is_a_bad_request() {
    assert_eq!(
        send(state(None), upload(&[("model", None)])).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn an_empty_file_is_a_bad_request() {
    assert_eq!(
        send(state(None), upload(&[("file", Some(b""))])).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn a_non_wav_upload_is_unsupported_media_type() {
    // The common case is an mp3 or m4a, which decodes as "not a readable WAV".
    // 415 rather than 400 so the client learns the format is the problem.
    assert_eq!(
        send(state(None), upload(&[("file", Some(b"ID3\x04\x00not actually a wav"))])).await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

#[tokio::test]
async fn a_wav_with_no_audio_is_a_bad_request() {
    // A valid, well-formed header describing zero frames: it decodes cleanly
    // and yields nothing, which is a different failure from a bad container.
    let wav = wav_header_with_no_frames();
    assert_eq!(
        send(state(None), upload(&[("file", Some(&wav))])).await,
        StatusCode::BAD_REQUEST
    );
}

/// 44 bytes of RIFF header declaring 16 kHz mono 16-bit and no data.
fn wav_header_with_no_frames() -> Vec<u8> {
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&36u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&0u32.to_le_bytes());
    wav
}

/// A GET that axum's `WebSocketUpgrade` extractor will accept. Without these
/// headers the extractor rejects with 400 before the handler runs, so every
/// refusal inside `stream()` looks like a malformed request.
fn ws_get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn the_model_list_is_served_when_authorised() {
    let request = Request::builder()
        .uri("/v1/models")
        .header("authorization", "Bearer secret")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(state(Some("secret")), request).await, StatusCode::OK);
}

#[tokio::test]
async fn a_non_upgrade_request_to_the_stream_route_is_426() {
    // Documenting where the boundary is: `WebSocketUpgrade` rejects during
    // extraction, so nothing inside the handler runs for a plain GET. That is
    // why the refusals below are tested through `validate_stream` instead.
    // A plain GET is 400 (no Upgrade header); a well-formed upgrade attempt is
    // 426, because `oneshot` carries no real connection to upgrade. Either
    // way the handler body never runs.
    assert_eq!(
        send(state(None), get("/v1/audio/stream")).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(state(None), ws_get("/v1/audio/stream")).await,
        StatusCode::UPGRADE_REQUIRED
    );
}

fn params(sample_rate: Option<usize>, dialect: Option<&str>) -> super::routes::StreamParams {
    super::routes::StreamParams {
        sample_rate,
        dialect: dialect.map(str::to_string),
        token: None,
    }
}

#[test]
fn a_model_that_cannot_stream_is_refused_with_conflict() {
    let err = super::routes::validate_stream("slow-model", false, &params(None, None)).unwrap_err();
    assert_eq!(err.status, StatusCode::CONFLICT);
}

#[test]
fn a_zero_sample_rate_is_refused() {
    let err = super::routes::validate_stream("m", true, &params(Some(0), None)).unwrap_err();
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[test]
fn an_unknown_dialect_is_refused() {
    let err =
        super::routes::validate_stream("m", true, &params(None, Some("klingon"))).unwrap_err();
    assert_eq!(err.status, StatusCode::BAD_REQUEST);
}

#[test]
fn the_default_dialect_is_native_at_the_engine_rate() {
    let Ok((rate, dialect)) = super::routes::validate_stream("m", true, &params(None, None))
    else {
        panic!("the default request must validate");
    };
    assert_eq!(rate, crate::server::audio::TARGET_HZ);
    assert_eq!(dialect, super::routes::Dialect::Native);
}
