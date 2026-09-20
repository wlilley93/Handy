//! A local HTTP transcription API on top of the app's existing engine.
//!
//! The point of serving from inside Handy rather than beside it is that there
//! is exactly one loaded model. A sidecar would hold a second copy of the
//! weights, compete for the same accelerator, and drift out of sync with the
//! model the user picked in the UI. Every request here goes through the same
//! `TranscriptionManager` the hotkey uses, so switching models in the app
//! switches them for the API too, and the unload timeout still applies.
//!
//! Binds loopback by default. `server_allow_lan` opens it to the network and
//! is refused without a token.

pub mod audio;
mod routes;

use anyhow::{bail, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};
use tokio::sync::oneshot;

use crate::managers::transcription::TranscriptionManager;
use crate::settings::get_settings;

/// Resolved, validated server configuration. Built from settings so the
/// validation lives in one place rather than at each call site.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub addr: SocketAddr,
    pub token: Option<String>,
}

impl ServerConfig {
    pub fn from_settings(app: &AppHandle, port_override: Option<u16>) -> Result<Self> {
        let settings = get_settings(app);
        Self::resolve(
            port_override.unwrap_or(settings.server_port),
            settings.server_token.as_deref(),
            settings.server_allow_lan,
        )
    }

    /// The binding rules, with no Tauri in them so they can be tested directly.
    ///
    /// The one rule worth stating plainly: an unauthenticated listener is
    /// confined to loopback. Transcription is a capability — whoever can reach
    /// it can spend this machine's accelerator and read back text — so opening
    /// it to the network without a token is refused rather than warned about.
    pub fn resolve(port: u16, token: Option<&str>, allow_lan: bool) -> Result<Self> {
        if port == 0 {
            bail!("server port is 0; pick a port between 1 and 65535");
        }
        let token = token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);

        let ip = if allow_lan {
            if token.is_none() {
                bail!("refusing to bind 0.0.0.0 without a token: set one, or turn off LAN access");
            }
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        };

        Ok(Self {
            addr: SocketAddr::new(ip, port),
            token,
        })
    }
}

/// What the HTTP layer needs from the application around it.
///
/// `ServerState` held a Tauri `AppHandle` directly, which made `router()`
/// impossible to build outside a running app — so every status path in
/// `routes.rs` was unreachable by a test, and `guard-coverage` reports 14 of
/// them as unexercised. This is the seam: production passes the app, a test
/// passes a fake. `show_overlay` was already half of this admission, since
/// headless mode has no window.
pub trait ServerHost: Send + Sync + 'static {
    fn settings(&self) -> crate::settings::AppSettings;
    fn models(&self) -> Arc<dyn Models>;
    fn show_transcribing_overlay(&self);
    fn show_streaming_overlay(&self);
    fn hide_recording_overlay(&self);
    /// Returns the listener id to hand back to `unlisten`.
    fn listen_stream_text(
        &self,
        on: Box<dyn Fn(crate::managers::transcription::StreamTextEvent) + Send + Sync + 'static>,
    ) -> u32;
    fn unlisten(&self, id: u32);
}

/// The production host: everything routed through the Tauri app handle.
pub struct TauriHost(pub AppHandle);

impl ServerHost for TauriHost {
    fn settings(&self) -> crate::settings::AppSettings {
        get_settings(&self.0)
    }
    fn models(&self) -> Arc<dyn Models> {
        Arc::clone(&self.0.state::<Arc<crate::managers::model::ModelManager>>()) as Arc<dyn Models>
    }
    fn show_transcribing_overlay(&self) {
        crate::overlay::show_transcribing_overlay(&self.0);
    }
    fn show_streaming_overlay(&self) {
        crate::overlay::show_streaming_overlay(&self.0);
    }
    fn hide_recording_overlay(&self) {
        crate::overlay::hide_recording_overlay(&self.0);
    }
    fn listen_stream_text(
        &self,
        on: Box<dyn Fn(crate::managers::transcription::StreamTextEvent) + Send + Sync + 'static>,
    ) -> u32 {
        use tauri_specta::Event;
        crate::managers::transcription::StreamTextEvent::listen(&self.0, move |event| {
            on(event.payload)
        })
    }
    fn unlisten(&self, id: u32) {
        use tauri::Listener;
        self.0.unlisten(id);
    }
}

/// The model catalogue, as the HTTP layer uses it.
///
/// The third and last of these seams. `ModelManager` is reached through the
/// Tauri state map, so holding it concretely kept the stream route untestable
/// even after the host and the engine were abstracted: `stream()` looks the
/// model up before it checks the sample rate, the dialect, or whether another
/// stream is already open, so every refusal after that point was unreachable.
pub trait Models: Send + Sync + 'static {
    fn available(&self) -> Vec<crate::managers::model::ModelInfo>;
    fn info(&self, model_id: &str) -> Option<crate::managers::model::ModelInfo>;
}

impl Models for crate::managers::model::ModelManager {
    fn available(&self) -> Vec<crate::managers::model::ModelInfo> {
        self.get_available_models()
    }
    fn info(&self, model_id: &str) -> Option<crate::managers::model::ModelInfo> {
        self.get_model_info(model_id)
    }
}

/// The transcription engine, as the HTTP layer uses it.
///
/// The second half of the same seam as [`ServerHost`]: `TranscriptionManager`
/// is built from an `AppHandle` too, so holding it concretely kept
/// `ServerState` unconstructible even after the host was abstracted. These are
/// the eight methods `routes.rs` actually calls — a narrower surface than the
/// manager's, which is the point of naming it.
pub trait Transcriber: Send + Sync + 'static {
    fn is_model_loaded(&self) -> bool;
    fn initiate_model_load(&self);
    fn current_backend(&self) -> Option<String>;
    fn stream_router(&self) -> Arc<crate::managers::transcription::StreamRouter>;
    fn start_stream(&self);
    fn cancel_stream(&self);
    fn finalize_stream(&self) -> Result<Option<String>>;
    fn transcribe(&self, audio: Vec<f32>) -> Result<String>;
    fn load_model(&self, model_id: &str) -> Result<()>;
}

impl Transcriber for TranscriptionManager {
    fn is_model_loaded(&self) -> bool {
        TranscriptionManager::is_model_loaded(self)
    }
    fn initiate_model_load(&self) {
        TranscriptionManager::initiate_model_load(self)
    }
    fn current_backend(&self) -> Option<String> {
        TranscriptionManager::current_backend(self)
    }
    fn stream_router(&self) -> Arc<crate::managers::transcription::StreamRouter> {
        TranscriptionManager::stream_router(self)
    }
    fn start_stream(&self) {
        TranscriptionManager::start_stream(self)
    }
    fn cancel_stream(&self) {
        TranscriptionManager::cancel_stream(self)
    }
    fn finalize_stream(&self) -> Result<Option<String>> {
        TranscriptionManager::finalize_stream(self)
    }
    fn transcribe(&self, audio: Vec<f32>) -> Result<String> {
        TranscriptionManager::transcribe(self, audio)
    }
    fn load_model(&self, model_id: &str) -> Result<()> {
        TranscriptionManager::load_model(self, model_id)
    }
}

/// Everything a handler needs. Cloned per request; the expensive parts are
/// behind `Arc`.
#[derive(Clone)]
pub struct ServerState {
    pub host: Arc<dyn ServerHost>,
    pub transcription: Arc<dyn Transcriber>,
    pub token: Option<String>,
    /// Drive the recording overlay for server-triggered work. False when the
    /// app runs headless (`--serve`), where no overlay window exists.
    pub show_overlay: bool,
}

/// Handle to a running server, stored in Tauri state so the settings UI can
/// start and stop it without restarting the app.
pub struct ServerHandle {
    running: AtomicBool,
    addr: Mutex<Option<SocketAddr>>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
}

impl Default for ServerHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerHandle {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            addr: Mutex::new(None),
            shutdown: Mutex::new(None),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn bound_addr(&self) -> Option<SocketAddr> {
        *self.addr.lock().unwrap()
    }

    /// Stop a running server. Idempotent: stopping a stopped server is fine.
    pub fn stop(&self) {
        if let Some(tx) = self.shutdown.lock().unwrap().take() {
            let _ = tx.send(());
        }
        self.running.store(false, Ordering::Release);
        *self.addr.lock().unwrap() = None;
    }
}

/// Start the server in the app's async runtime, replacing any running instance.
///
/// Returns once the listener is bound, so a caller that reports success is
/// reporting a port that is actually accepting connections — an "enabled"
/// toggle that goes green while the bind fails is worse than an error.
pub async fn start(
    app: &AppHandle,
    config: ServerConfig,
    show_overlay: bool,
) -> Result<SocketAddr> {
    let handle = app.state::<Arc<ServerHandle>>().inner().clone();
    handle.stop();

    let transcription = app.state::<Arc<TranscriptionManager>>().inner().clone();
    let state = ServerState {
        host: Arc::new(TauriHost(app.clone())),
        transcription,
        token: config.token.clone(),
        show_overlay,
    };

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "could not bind {}: {} (is another Handy or service already on that port?)",
                config.addr,
                e
            )
        })?;
    let addr = listener.local_addr()?;

    let (tx, rx) = oneshot::channel::<()>();
    *handle.shutdown.lock().unwrap() = Some(tx);
    *handle.addr.lock().unwrap() = Some(addr);
    handle.running.store(true, Ordering::Release);

    let router = routes::router(state);
    let served = handle.clone();
    tauri::async_runtime::spawn(async move {
        let result = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
        if let Err(e) = result {
            log::error!("Local API server stopped with an error: {}", e);
        } else {
            log::info!("Local API server stopped.");
        }
        served.running.store(false, Ordering::Release);
        *served.addr.lock().unwrap() = None;
    });

    log::info!(
        "Local API server listening on http://{} (auth: {})",
        addr,
        if config.token.is_some() {
            "bearer token"
        } else {
            "none, loopback only"
        }
    );
    Ok(addr)
}

/// Start the server at app launch if the user has it enabled. Failures are
/// logged, never fatal: a port collision must not stop the app from starting.
pub fn start_if_enabled(app: &AppHandle) {
    if !get_settings(app).server_enabled {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match ServerConfig::from_settings(&app, None) {
            Ok(config) => {
                if let Err(e) = start(&app, config, true).await {
                    log::error!("Local API server failed to start: {}", e);
                }
            }
            Err(e) => log::error!("Local API server is misconfigured: {}", e),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_without_a_token_is_allowed() {
        let c = ServerConfig::resolve(8915, None, false).unwrap();
        assert_eq!(c.addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(c.token.is_none());
    }

    #[test]
    fn lan_without_a_token_is_refused() {
        let err = ServerConfig::resolve(8915, None, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("without a token"), "got: {err}");
    }

    #[test]
    fn a_blank_token_does_not_count_as_a_token() {
        // A settings field holding only whitespace must not be mistaken for a
        // credential — that would silently open 0.0.0.0 with no auth.
        assert!(ServerConfig::resolve(8915, Some("   "), true).is_err());
        let c = ServerConfig::resolve(8915, Some("  \t "), false).unwrap();
        assert!(c.token.is_none());
    }

    #[test]
    fn lan_with_a_token_binds_all_interfaces() {
        let c = ServerConfig::resolve(8915, Some("swordfish"), true).unwrap();
        assert_eq!(c.addr.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(c.token.as_deref(), Some("swordfish"));
    }

    #[test]
    fn a_token_is_trimmed_before_use() {
        let c = ServerConfig::resolve(8915, Some(" swordfish "), false).unwrap();
        assert_eq!(c.token.as_deref(), Some("swordfish"));
    }

    #[test]
    fn port_zero_is_refused() {
        assert!(ServerConfig::resolve(0, None, false).is_err());
    }
}

#[cfg(test)]
mod http_tests;
