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
}

impl ServerHost for FakeHost {
    fn settings(&self) -> crate::settings::AppSettings {
        crate::settings::get_default_settings()
    }
    fn models(&self) -> Arc<crate::managers::model::ModelManager> {
        unimplemented!("no test here reaches the model list")
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
    ServerState {
        host: Arc::new(FakeHost::default()),
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
