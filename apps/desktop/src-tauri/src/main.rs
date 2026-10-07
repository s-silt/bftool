#![windows_subsystem = "windows"]
use bftool_desktop_bridge::*;
use tauri::{Manager, State};

fn worker_error(message: impl ToString) -> ErrorDto {
    ErrorDto {
        code: "WORKER_FAILED".into(),
        message: message.to_string(),
        operation: "worker".into(),
        retry: "检查任务记录后重试".into(),
    }
}
#[tauri::command]
async fn choose_paths(window: tauri::WebviewWindow, kind: String) -> Result<Vec<String>, ErrorDto> {
    tauri::async_runtime::spawn_blocking(move || {
        let dialog = rfd::FileDialog::new().set_parent(&window);
        let paths = match kind.as_str() {
            "file" => dialog.pick_files().unwrap_or_default(),
            "directory" => dialog.pick_folders().unwrap_or_default(),
            "target" => dialog.pick_folder().into_iter().collect(),
            _ => return Err(worker_error("未知选择器类型")),
        };
        paths
            .into_iter()
            .map(|p| {
                p.into_os_string()
                    .into_string()
                    .map_err(|_| worker_error("路径不能转换为 UTF-8"))
            })
            .collect()
    })
    .await
    .map_err(worker_error)?
}
#[tauri::command]
fn input_revision(engine: State<'_, Engine>) -> String {
    engine.input_revision()
}
#[tauri::command]
fn invalidate_plan(engine: State<'_, Engine>, revision: String) -> Result<(), ErrorDto> {
    engine.invalidate(&revision)
}
#[tauri::command]
async fn preview_backup(
    engine: State<'_, Engine>,
    request: PreviewRequest,
) -> Result<PlanDto, ErrorDto> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.preview(request))
        .await
        .map_err(worker_error)?
}
#[tauri::command]
fn plan_entries(
    engine: State<'_, Engine>,
    plan_id: String,
    offset: usize,
    limit: usize,
) -> Result<Vec<EntryDto>, ErrorDto> {
    engine.entries(&plan_id, offset, limit)
}
#[tauri::command]
fn start_backup(
    engine: State<'_, Engine>,
    revision: String,
    plan_id: String,
) -> Result<String, ErrorDto> {
    engine.start(&revision, &plan_id)
}
#[tauri::command]
fn resume_backup(
    engine: State<'_, Engine>,
    revision: String,
    recovery_id: String,
) -> Result<String, ErrorDto> {
    engine.resume(&revision, &recovery_id)
}
#[tauri::command]
fn job_snapshot(engine: State<'_, Engine>) -> Result<Option<Snapshot>, ErrorDto> {
    engine.snapshot()
}
#[tauri::command]
fn cancel_job(engine: State<'_, Engine>, job_id: String) -> Result<(), ErrorDto> {
    engine.cancel(&job_id)
}
#[tauri::command]
async fn backup_history(
    engine: State<'_, Engine>,
    target: String,
) -> Result<Vec<HistoryDto>, ErrorDto> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.history(&target))
        .await
        .map_err(worker_error)?
}
#[tauri::command]
async fn verify_backup(
    engine: State<'_, Engine>,
    target: String,
) -> Result<VerificationDto, ErrorDto> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.verify(&target))
        .await
        .map_err(worker_error)?
}
fn main() {
    tauri::Builder::default()
        .manage(Engine::default())
        .setup(|app| {
            let config = app
                .config()
                .app
                .windows
                .first()
                .ok_or("missing main window config")?;
            let expected = app
                .config()
                .build
                .dev_url
                .as_ref()
                .map(|u| u.origin().ascii_serialization());
            let window = tauri::WebviewWindowBuilder::from_config(app, config)?;
            #[cfg(debug_assertions)]
            let mut window = window;
            #[cfg(debug_assertions)]
            {
                if let Ok(profile) = std::env::var("BFTOOL_TEST_PROFILE") {
                    window = window.data_directory(std::path::PathBuf::from(profile));
                }
                if let Ok(port) = std::env::var("BFTOOL_TEST_CDP_PORT") {
                    let port: u16 = port.parse()?;
                    window = window.additional_browser_args(&format!(
                        "--remote-debugging-port={port} --remote-debugging-address=127.0.0.1"
                    ));
                }
            }
            window
                .on_navigation(move |url| {
                    #[cfg(feature = "custom-protocol")]
                    {
                        let _ = &expected;
                        url.scheme() == "http"
                            && url.host_str() == Some("tauri.localhost")
                            && url.port().is_none()
                    }
                    #[cfg(not(feature = "custom-protocol"))]
                    {
                        expected
                            .as_ref()
                            .is_some_and(|origin| &url.origin().ascii_serialization() == origin)
                    }
                })
                .build()?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let engine = window.state::<Engine>().inner().clone();
                if engine.request_shutdown() {
                    let app = window.app_handle().clone();
                    std::thread::spawn(move || {
                        loop {
                            if !engine.is_busy() {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        app.exit(0);
                    });
                } else {
                    window.app_handle().exit(0);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            choose_paths,
            input_revision,
            invalidate_plan,
            preview_backup,
            plan_entries,
            start_backup,
            resume_backup,
            job_snapshot,
            cancel_job,
            backup_history,
            verify_backup
        ])
        .run(tauri::generate_context!())
        .expect("bftool desktop startup");
}
