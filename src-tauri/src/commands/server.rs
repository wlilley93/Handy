//! Commands the settings UI uses to drive the local API server.
//!
//! The settings are persisted by these commands rather than by the frontend
//! store alone, because starting the server has to happen on the same
//! transition that writes the setting — otherwise the toggle and the socket
//! disagree until the next app launch.

use std::sync::Arc;
use tauri::{AppHandle, Manager, State};

use crate::server::{self, ServerHandle};
use crate::settings::{get_settings, write_settings};

#[derive(serde::Serialize, serde::Deserialize, specta::Type)]
pub struct ServerStatus {
    pub enabled: bool,
    pub running: bool,
    /// The address actually bound, once running. Not derived from the settings:
    /// a port collision means the setting and the socket differ, and the UI
    /// should show what is true.
    pub address: Option<String>,
    pub port: u16,
    pub has_token: bool,
    pub allow_lan: bool,
}

#[tauri::command]
#[specta::specta]
pub fn get_server_status(app: AppHandle, handle: State<'_, Arc<ServerHandle>>) -> ServerStatus {
    let settings = get_settings(&app);
    ServerStatus {
        enabled: settings.server_enabled,
        running: handle.is_running(),
        address: handle.bound_addr().map(|a| a.to_string()),
        port: settings.server_port,
        has_token: settings
            .server_token
            .as_deref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false),
        allow_lan: settings.server_allow_lan,
    }
}

/// Write the server settings and bring the listener into line with them.
///
/// Returns the bound address on success. An error leaves the settings written
/// but the server down, and `get_server_status` will report exactly that.
#[tauri::command]
#[specta::specta]
pub async fn set_server_settings(
    app: AppHandle,
    enabled: bool,
    port: u16,
    token: Option<String>,
    allow_lan: bool,
) -> Result<Option<String>, String> {
    let mut settings = get_settings(&app);
    settings.server_enabled = enabled;
    settings.server_port = port;
    settings.server_token = token.filter(|t| !t.trim().is_empty());
    settings.server_allow_lan = allow_lan;
    write_settings(&app, settings);

    let handle = {
        let state: State<'_, Arc<ServerHandle>> = app.state();
        state.inner().clone()
    };

    if !enabled {
        handle.stop();
        return Ok(None);
    }

    let config = server::ServerConfig::from_settings(&app, None).map_err(|e| e.to_string())?;
    let addr = server::start(&app, config, true)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(addr.to_string()))
}
