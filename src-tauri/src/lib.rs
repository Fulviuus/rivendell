pub mod auth;
pub mod commands;
pub mod db;
pub mod error;
pub mod export;
pub mod fsjail;
pub mod git;
pub mod mcp;
pub mod models;
pub mod store;

use commands::AppState;
use std::sync::Arc;
use store::Store;
use tauri::{Emitter, Manager};

/// Preferred first, so an MCP config written today still works tomorrow.
const PREFERRED_PORTS: &[u16] = &[8787, 8788, 8789, 8790, 8791];

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rivendell_lib=info,warn".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let store = Arc::new(Store::open(&dir.join("rivendell.db"))?);

            let mcp_url = Arc::new(std::sync::RwLock::new(String::new()));
            app.manage(AppState {
                store: store.clone(),
                mcp_url: mcp_url.clone(),
            });

            // Hand stalled threads back to the coder without waiting for
            // someone to poke the app.
            let sweeper = store.clone();
            tauri::async_runtime::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    tick.tick().await;
                    match sweeper.sweep_stalled_threads() {
                        Ok(n) if n > 0 => tracing::info!("{n} thread(s) stopped waiting"),
                        Err(e) => tracing::warn!("sweep failed: {e}"),
                        _ => {}
                    }
                }
            });

            // Bridge the event log into the webview.
            let handle = app.handle().clone();
            let mut rx = store.events.subscribe();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(notice) => {
                            let _ = handle.emit("rivendell://event", notice);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("ui event stream lagged by {n}");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = Arc::new(mcp::server::McpState { store: store.clone() });

                let mut running = None;
                for port in PREFERRED_PORTS {
                    match mcp::server::serve(state.clone(), *port).await {
                        Ok(r) => {
                            running = Some(r);
                            break;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                        Err(e) => {
                            tracing::error!("could not bind {port}: {e}");
                            continue;
                        }
                    }
                }
                // Every preferred port was taken — let the OS choose.
                let running = match running {
                    Some(r) => Some(r),
                    None => mcp::server::serve(state.clone(), 0).await.ok(),
                };

                match running {
                    Some(r) => {
                        tracing::info!("mcp server listening on {}", r.url);
                        if let Ok(mut u) = mcp_url.write() {
                            *u = r.url.clone();
                        }
                        let _ = handle.emit("rivendell://server", r.url);
                    }
                    None => tracing::error!("could not start the MCP server on any port"),
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::server_info,
            commands::list_projects,
            commands::create_project,
            commands::delete_project,
            commands::list_rooms,
            commands::create_room,
            commands::update_room,
            commands::delete_room,
            commands::list_profiles,
            commands::upsert_profile,
            commands::list_agents,
            commands::create_agent,
            commands::update_agent,
            commands::rotate_agent_key,
            commands::set_agent_revoked,
            commands::delete_agent,
            commands::list_tags,
            commands::list_threads,
            commands::get_thread,
            commands::create_thread,
            commands::reply,
            commands::edit_message,
            commands::update_thread,
            commands::resolve_thread,
            commands::set_thread_status,
            commands::claim_thread,
            commands::search,
            commands::events_since,
            commands::file_preview,
            commands::list_project_files,
            commands::git_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Rivendell");
}
