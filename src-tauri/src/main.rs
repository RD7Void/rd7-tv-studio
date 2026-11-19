// Rust / main.rs / Tauri v2 / Windows x64
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod tv;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tauri::Manager;

#[derive(Clone)]
struct State(Arc<Mutex<tv::Backend>>);
#[tauri::command]
async fn rpc(
    state: tauri::State<'_, State>,
    action: String,
    payload: Option<Value>,
) -> Result<Value, String> {
    let backend = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        backend
            .lock()
            .map_err(|_| "Conexão ocupada. Reinicie o painel.".to_string())?
            .dispatch(&action, payload.unwrap_or(Value::Null))
    })
    .await
    .map_err(|e| e.to_string())?
}
fn main() {
    if let Some(arg) = std::env::args().nth(1) {
        if arg == "--smoke-test" {
            let dir = std::path::PathBuf::from(std::env::var("LOCALAPPDATA").unwrap())
                .join("com.rd7.tvstudio");
            let mut backend = tv::Backend::new(dir);
            let result = backend.smoke();
            let out =
                std::env::var("RD7_TEST_OUTPUT").unwrap_or_else(|_| "rd7-diagnostic.json".into());
            std::fs::write(out, serde_json::to_vec_pretty(&result).unwrap()).ok();
            return;
        }
    }
    let mut context = tauri::generate_context!();
    if std::env::var("RD7_STUDIO_QA").as_deref() == Ok("1") {
        if let Some(window) = context.config_mut().app.windows.first_mut() {
            window.additional_browser_args =
                Some("--remote-debugging-port=9224 --remote-allow-origins=*".into());
        }
    }
    tauri::Builder::default()
        .setup(|app| {
            let dir = app.path().app_local_data_dir()?;
            app.manage(State(Arc::new(Mutex::new(tv::Backend::new(dir)))));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![rpc])
        .run(context)
        .expect("Falha ao abrir RD7 TV Studio");
}
