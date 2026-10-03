//! 交互与验收补测：
//! 1. 重复点击防护 (防重复启动与重入)
//! 2. 运行中切页保持与状态独立
//! 3. 更改来源、目标或设置使预览立即失效 (防执行旧计划)
//! 4. 停止任务二阶段过渡 (停止请求中 -> 已停止)
//! 5. 校验严格规则 (0 项不显示成功，仅大小不显示内容已校验)
//! 6. 盘型识别 (非机械盘不盲称机械盘)

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bftool_core::config::{Config, ConfigSource};
use bftool_gui::app::{App, View};
use bftool_gui::backend::Backend;
use bftool_gui::reporter::ProgressState;
use bftool_gui::views::backup::{apply_demo_state, DemoState, TargetDriveKind};
use eframe::egui;

fn headless_fixture() -> (App, egui::Context) {
    let mut app = App {
        backend: Backend::Demo,
        task_kind: None,
        view: View::Backup,
        cfg: Config::default(),
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

#[test]
fn test_modify_source_or_target_invalidates_plan() {
    let (mut app, _ctx) = headless_fixture();
    apply_demo_state(&mut app, DemoState::SingleFile);
    assert!(app.backup_ui.plan.is_some(), "初始应有计划预览");

    // 1. 修改目标路径 -> 旧计划必须失效！
    app.backup_ui.target_path = r"E:\新目标路径_2026".into();
    app.backup_ui.check_invalidate_plan();
    assert!(
        app.backup_ui.plan.is_none(),
        "更改目标路径后，旧计划必须立即失效"
    );

    // 2. 重新加载计划并修改来源项目列表 -> 旧计划必须失效！
    apply_demo_state(&mut app, DemoState::MultiFolder);
    assert!(app.backup_ui.plan.is_some());
    app.backup_ui.sources.pop();
    app.backup_ui.check_invalidate_plan();
    assert!(
        app.backup_ui.plan.is_none(),
        "移除来源项后，旧计划必须立即失效"
    );
}

#[test]
fn test_reentry_and_duplicate_click_prevention() {
    let (mut app, _ctx) = headless_fixture();
    apply_demo_state(&mut app, DemoState::SingleFile);

    // 首次启动备份
    bftool_gui::views::backup::start_backup_action(&mut app);
    assert!(app.backup_ui.is_running);
    assert!(app.task.is_some());
    let first_task_started = app.task_started;

    // 模拟用户在运行中再次快速点击“开始备份”
    bftool_gui::views::backup::start_backup_action(&mut app);
    // 任务指针与状态不得被破坏或重新初始化
    assert!(app.backup_ui.is_running);
    assert_eq!(app.task_started, first_task_started);
}

#[test]
fn test_tab_switching_during_task_execution() {
    let (mut app, ctx) = headless_fixture();
    apply_demo_state(&mut app, DemoState::SingleFile);

    // 启动一个稍长周期的后台备份任务
    app.backup_ui.is_running = true;
    app.task = Some(bftool_gui::task::BackgroundTask::spawn(|cancel| {
        while !cancel.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(bftool_gui::app::ExecutionResult::Summary("done".into()))
    }));

    // 自由切页到「任务与记录」
    app.view = View::Tasks;
    let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
    assert!(app.backup_ui.is_running, "切页至任务与记录不应中断后台备份");

    // 自由切页到「校验」
    app.view = View::Verify;
    let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
    assert!(app.backup_ui.is_running, "切页至校验不应中断后台备份");

    // 自由切页到「设置」
    app.view = View::Settings;
    let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
    assert!(app.backup_ui.is_running, "切页至设置不应中断后台备份");

    // 切回「备份」
    app.view = View::Backup;
    let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
    assert!(app.backup_ui.is_running);

    // 收尾取消任务
    if let Some(t) = &app.task {
        t.request_cancel();
    }
}

#[test]
fn test_stop_transition_two_phase() {
    let (mut app, ctx) = headless_fixture();
    apply_demo_state(&mut app, DemoState::SingleFile);

    bftool_gui::views::backup::start_backup_action(&mut app);
    assert!(app.backup_ui.is_running);
    assert!(!app.backup_ui.stopping_requested);

    // 第一阶段：用户发出停止请求
    bftool_gui::views::backup::request_stop_backup(&mut app);
    assert!(
        app.backup_ui.stopping_requested,
        "点击停止后应先进入「停止请求中」状态"
    );
    assert!(app.task.as_ref().unwrap().cancel_requested());

    // 第二阶段：后台任务真正退出并完成泵送清理
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.task.is_some() && Instant::now() < deadline {
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.render(ctx));
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        !app.backup_ui.is_running,
        "后台真正退出后 is_running 应为 false"
    );
    assert!(
        !app.backup_ui.stopping_requested,
        "停止完成后 stopping_requested 应复位"
    );
}

#[test]
fn test_verification_zero_and_size_only_rules() {
    // 1. 0 个检查项不能显示“校验成功”或“通过”
    let zero_summary = bftool_gui::views::verify::verify_summary(0, 0, 0, 0, false);
    assert!(
        !zero_summary.contains("通过")
            && (zero_summary.contains("0") || zero_summary.contains("无可校验项")),
        "0 项检查必须明确提示无可校验项，绝不能显示为通过：{zero_summary}"
    );

    // 2. 仅大小比对不能标为“内容已校验”
    let size_only_summary = bftool_gui::views::verify::verify_summary(10, 0, 0, 5, false);
    assert!(
        !size_only_summary.contains("内容已校验")
            && (size_only_summary.contains("大小") || size_only_summary.contains("未核对内容")),
        "仅大小比对不能标为内容已校验：{size_only_summary}"
    );
}

#[test]
fn test_drive_type_label_rule() {
    // 机械盘明确标为机械盘
    assert_eq!(TargetDriveKind::Hdd.label(), "机械盘目标");
    // 未知或固态盘不得盲目标为机械盘
    assert_eq!(TargetDriveKind::Unknown.label(), "目标目录");
    assert_eq!(TargetDriveKind::Ssd.label(), "目标目录 (固态盘)");
}

#[test]
fn test_drop_and_close_prompt_handling() {
    let (mut app, _ctx) = headless_fixture();
    apply_demo_state(&mut app, DemoState::SingleFile);
    bftool_gui::views::backup::start_backup_action(&mut app);
    assert!(app.task.is_some());

    // 模拟窗口关闭触发 App::drop
    let closing_start = Instant::now();
    drop(app);
    assert!(
        closing_start.elapsed() < Duration::from_secs(3),
        "关闭窗口时必须安全收尾并及时返回，不得假装取消"
    );
}
