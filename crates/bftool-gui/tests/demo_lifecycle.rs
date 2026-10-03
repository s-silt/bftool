//! Actual egui button events and synthetic asynchronous workers. No OS window/volume.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bftool_core::config::ConfigSource;
use bftool_core::reporter::LogLevel;
use bftool_gui::app::{App, TaskKind, View};
use bftool_gui::backend::{synthetic_config, Backend};
use bftool_gui::reporter::ProgressState;
use eframe::egui;

fn fixture() -> (App, egui::Context) {
    let mut app = App {
        backend: Backend::Demo,
        task_kind: None,
        view: View::Dashboard,
        cfg: synthetic_config(),
        config_source: ConfigSource::Default,
        logs: Vec::new(),
        progress: Arc::new(Mutex::new(ProgressState::default())),
        rx: None,
        task: None,
        plan_task: None,
        file_backup_plan_task: None,
        find_task: None,
        task_started: None,
        last_summary: None,
        archive_plan: None,
        archive_plan_inputs: None,
        archive_ui: Default::default(),
        drives_cache: None,
        drives_result: None,
        find_ui: Default::default(),
        init_ui: Default::default(),
        verify_ui: Default::default(),
        settings_ui: Default::default(),
        watch_ui: Default::default(),
        backup_ui: Default::default(),
        tasks_ui: Default::default(),
        status_cache: None,
        screenshot_runner: None,
    };
    bftool_gui::screenshot::inject_demo_data(&mut app);
    let ctx = egui::Context::default();
    let mut fonts = egui::FontDefinitions::default();
    let family = fonts.families[&egui::FontFamily::Proportional].clone();
    fonts
        .families
        .insert(egui::FontFamily::Name("semibold".into()), family);
    ctx.set_fonts(fonts);
    (app, ctx)
}

fn frame(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
    assert_eq!(
        app.backend,
        Backend::Demo,
        "never permit Live services in the event harness"
    );
    ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 820.0),
            )),
            events,
            ..Default::default()
        },
        |ctx| app.render(ctx),
    )
}

fn click(app: &mut App, ctx: &egui::Context, label: &str) {
    let output = frame(app, ctx, Vec::new());
    let pos = output
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) if t.galley.text().contains(label) => {
                let center = t.pos + t.galley.size() * 0.5;
                s.clip_rect.contains(center).then_some(center)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("button is not visible: {label}"));
    for pressed in [true, false] {
        frame(
            app,
            ctx,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
}

fn idle(app: &mut App, ctx: &egui::Context) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.is_busy() && Instant::now() < deadline {
        frame(app, ctx, Vec::new());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!app.is_busy(), "synthetic task did not settle within 5s");
    assert!(app.rx.is_none());
}

fn prepare(app: &mut App, ctx: &egui::Context, view: View) {
    app.view = view;
    if view == View::Archive {
        click(app, ctx, "演练 / 刷新计划");
        idle(app, ctx);
        assert!(app.archive_plan.is_some());
    }
}

fn lifecycle(view: View, kind: TaskKind, label: &str) {
    let (mut app, ctx) = fixture();
    prepare(&mut app, &ctx, view);
    click(&mut app, &ctx, label);
    assert_eq!(app.task_kind, Some(kind));
    assert!(app.task.is_some());
    let started = app.task_started;
    app.logs
        .push((LogLevel::Info, "first-task-log-owner".into()));
    click(&mut app, &ctx, label); // Disabled/reentry attempt must preserve original ownership.
    assert_eq!(app.task_started, started);
    assert!(app.logs.iter().any(|(_, s)| s == "first-task-log-owner"));
    app.view = if view == View::Watch {
        View::Archive
    } else {
        View::Watch
    };
    frame(&mut app, &ctx, Vec::new());
    assert_eq!(app.task_kind, Some(kind));
    click(&mut app, &ctx, "停止任务");
    idle(&mut app, &ctx);
    assert!(app
        .last_summary
        .as_ref()
        .unwrap()
        .starts_with("【演示合成结果】"));
    if kind == TaskKind::Verify {
        let report = app.verify_ui.last_report.as_ref().unwrap();
        assert!(report.cancelled && report.checked == 0);
    }
    if kind == TaskKind::Watch {
        assert!(app.watch_ui.last_msg.as_ref().unwrap().contains("已取消"));
    }
    prepare(&mut app, &ctx, view);
    click(&mut app, &ctx, label);
    idle(&mut app, &ctx);
    if kind == TaskKind::Verify {
        let report = app.verify_ui.last_report.as_ref().unwrap();
        assert!(!report.cancelled);
        assert_eq!((report.checked, report.extra), (152, 1));
    }
    if kind == TaskKind::Watch {
        assert!(app.watch_ui.last_msg.as_ref().unwrap().contains("监视结束"));
    }
    prepare(&mut app, &ctx, view);
    click(&mut app, &ctx, label);
    assert!(app.task.is_some());
    let closing = Instant::now();
    drop(app); // Same App/BackgroundTask Drop path as closing a native window.
    assert!(
        closing.elapsed() < Duration::from_secs(2),
        "synthetic close did not cancel/join promptly"
    );
}

#[test]
fn archive_buttons_reentry_cancel_complete_and_close_with_synthetic_backend() {
    lifecycle(View::Archive, TaskKind::Archive, "正式备份");
}
#[test]
fn verify_buttons_reentry_cancel_report_and_close_with_synthetic_backend() {
    lifecycle(View::Verify, TaskKind::Verify, "开始整盘复查");
}
#[test]
fn watch_buttons_reentry_cancel_complete_and_close_with_synthetic_backend() {
    lifecycle(View::Watch, TaskKind::Watch, "开始监视归档");
}
