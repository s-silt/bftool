//! 设置视图:显示当前生效配置 + 来源 → 编辑三根目录与参数 → validate → save(SaveTarget)。
//! 默认存 `%APPDATA%`(与 cwd 无关,CLI/GUI 双击都查到)。(Spec D §4.3/§5)

use std::path::PathBuf;

use eframe::egui;

use bftool_core::config::{Config, ConfigSource, LoadedConfig, SaveTarget};

use crate::app::App;
use crate::views::util;

/// 保存位置(GUI 暴露 3 种;Custom 需文件对话框,暂不在 GUI 提供)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveChoice {
    /// 写回当前来源(source 为 Default 时禁用)
    CurrentSource,
    #[default]
    AppData,
    CurrentDir,
}

/// 设置页跨帧状态:表单字段(数字用文本框,保存时解析)+ 保存位置 + 上次结果。
#[derive(Debug, Clone, Default)]
pub struct SettingsUiState {
    pub loaded: bool,
    pub ready_root: String,
    pub archived_root: String,
    pub system_root: String,
    pub reserve_gb: String,
    pub stable_minutes: String,
    pub min_drive_gb: String,
    pub name_prefix: String,
    pub test_archives: bool,
    pub winrar: String,
    pub bandizip: String,
    pub seven_zip: String,
    pub save_choice: SaveChoice,
    /// (成功?, 文案)
    pub result: Option<(bool, String)>,
}

impl SettingsUiState {
    /// 从 cfg 填充表单(首次进入 / 保存后)。
    fn load_from(&mut self, cfg: &Config) {
        self.ready_root = cfg.ready_root.display().to_string();
        self.archived_root = cfg.archived_root.display().to_string();
        self.system_root = cfg.system_root.display().to_string();
        self.reserve_gb = cfg.reserve_gb.to_string();
        self.stable_minutes = cfg.stable_minutes.to_string();
        self.min_drive_gb = cfg.min_drive_gb.to_string();
        self.name_prefix = cfg.name_prefix.clone();
        self.test_archives = cfg.test_archives;
        self.winrar = cfg.winrar_path.display().to_string();
        self.bandizip = cfg.bandizip_path.display().to_string();
        self.seven_zip = cfg.seven_zip_path.display().to_string();
        self.loaded = true;
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    ui.heading("设置");
    if !app.settings_ui.loaded {
        let cfg = app.cfg.clone();
        app.settings_ui.load_from(&cfg);
    }
    ui.weak(util::source_hint(&app.config_source));
    ui.separator();

    egui::Grid::new("settings_form")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            // 三个目录:点击选择(原生对话框),不手输路径。
            dir_field(ui, "待备份(源)目录", &mut app.settings_ui.ready_root);
            dir_field(ui, "已备份目录", &mut app.settings_ui.archived_root);
            dir_field(ui, "备份系统目录", &mut app.settings_ui.system_root);
            field(ui, "预留余量(GB)", &mut app.settings_ui.reserve_gb);
            field(ui, "稳定期(分钟)", &mut app.settings_ui.stable_minutes);
            field(ui, "认盘最小容量(GB)", &mut app.settings_ui.min_drive_gb);
            field(ui, "盘命名前缀", &mut app.settings_ui.name_prefix);
            ui.label("压缩包内部测试");
            ui.checkbox(&mut app.settings_ui.test_archives, "开启(推荐)");
            ui.end_row();
        });

    ui.collapsing("压缩包测试器路径(高级)", |ui| {
        egui::Grid::new("tester_paths")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                // 测试器是 .exe 文件:点击选择文件(可清除)。
                file_field(ui, "WinRAR", &mut app.settings_ui.winrar);
                file_field(ui, "Bandizip", &mut app.settings_ui.bandizip);
                file_field(ui, "7-Zip", &mut app.settings_ui.seven_zip);
            });
    });

    ui.separator();
    ui.label("保存位置：");
    let is_default = matches!(app.config_source, ConfigSource::Default);
    ui.horizontal(|ui| {
        ui.selectable_value(
            &mut app.settings_ui.save_choice,
            SaveChoice::AppData,
            "%APPDATA%(推荐)",
        );
        ui.add_enabled_ui(!is_default, |ui| {
            ui.selectable_value(
                &mut app.settings_ui.save_choice,
                SaveChoice::CurrentSource,
                "写回当前来源",
            );
        });
        ui.selectable_value(
            &mut app.settings_ui.save_choice,
            SaveChoice::CurrentDir,
            "当前目录(仅 CLI 也在此目录运行才生效)",
        );
    });
    if is_default && app.settings_ui.save_choice == SaveChoice::CurrentSource {
        // Default 时该选项禁用;若残留选中,纠回 AppData
        app.settings_ui.save_choice = SaveChoice::AppData;
    }

    ui.add_space(4.0);
    if ui.button("💾 保存").clicked() {
        do_save(app);
    }
    if let Some((ok, msg)) = &app.settings_ui.result {
        let color = if *ok {
            egui::Color32::from_rgb(0x2e, 0x7d, 0x32)
        } else {
            egui::Color32::from_rgb(0xCC, 0x33, 0x33)
        };
        ui.colored_label(color, msg);
    }
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.text_edit_singleline(value);
    ui.end_row();
}

/// 目录行:点击「选择目录…」弹原生对话框设置路径(不手输)。当前值只读显示(过长省略,hover 看全)。
fn dir_field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.horizontal(|ui| {
        if ui.button("选择目录…").clicked() {
            let mut dlg = rfd::FileDialog::new().set_title(format!("选择{}", label));
            let cur = value.trim();
            if !cur.is_empty() {
                dlg = dlg.set_directory(cur);
            }
            if let Some(p) = dlg.pick_folder() {
                *value = p.display().to_string();
            }
        }
        path_display(ui, value);
    });
    ui.end_row();
}

/// 文件行:点击「选择…」选 .exe(可「清除」)。当前值只读显示。
fn file_field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.horizontal(|ui| {
        if ui.button("选择…").clicked() {
            let mut dlg = rfd::FileDialog::new()
                .set_title(format!("选择 {} 可执行文件", label))
                .add_filter("可执行文件", &["exe"]);
            let cur = value.trim();
            if !cur.is_empty() {
                if let Some(parent) = std::path::Path::new(cur).parent() {
                    dlg = dlg.set_directory(parent);
                }
            }
            if let Some(p) = dlg.pick_file() {
                *value = p.display().to_string();
            }
        }
        if !value.trim().is_empty() && ui.button("清除").clicked() {
            value.clear();
        }
        path_display(ui, value);
    });
    ui.end_row();
}

/// 路径只读显示:空 → "(未选择)";过长 → 省略中间,hover 看全。
fn path_display(ui: &mut egui::Ui, value: &str) {
    let v = value.trim();
    if v.is_empty() {
        ui.weak("(未选择)");
    } else {
        ui.monospace(elide(v, 44)).on_hover_text(v);
    }
}

/// 过长字符串保留尾部(路径末段更有信息量),前面用 … 省略。
fn elide(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let tail: String = chars[chars.len() - (max - 1)..].iter().collect();
    format!("…{tail}")
}

fn do_save(app: &mut App) {
    match parse_form(&app.settings_ui) {
        Err(msg) => app.settings_ui.result = Some((false, msg)),
        Ok(cfg) => {
            let target = save_choice_to_target(app.settings_ui.save_choice);
            let loaded = LoadedConfig {
                config: cfg.clone(),
                source: app.config_source.clone(),
            };
            match loaded.save(&target) {
                Ok(path) => {
                    app.cfg = cfg;
                    // 保存后,配置现落在 path —— 后续视为可自动发现的来源。
                    app.config_source = ConfigSource::Candidate(path.clone());
                    app.settings_ui.result = Some((true, format!("已保存到 {}", path.display())));
                }
                Err(e) => app.settings_ui.result = Some((false, format!("保存失败：{:#}", e))),
            }
        }
    }
}

/// 表单 → Config:解析数字 + 校验三根目录非空(name_prefix 留给 core validate)。纯函数,可测。
fn parse_form(s: &SettingsUiState) -> Result<Config, String> {
    for (val, name) in [
        (&s.ready_root, "待备份"),
        (&s.archived_root, "已备份"),
        (&s.system_root, "备份系统"),
    ] {
        if val.trim().is_empty() {
            return Err(format!("「{}」目录不能为空。", name));
        }
    }
    let reserve_gb = parse_u64(&s.reserve_gb, "预留余量(GB)")?;
    let stable_minutes = parse_u64(&s.stable_minutes, "稳定期(分钟)")?;
    let min_drive_gb = parse_u64(&s.min_drive_gb, "认盘最小容量(GB)")?;
    Ok(Config {
        ready_root: PathBuf::from(s.ready_root.trim()),
        archived_root: PathBuf::from(s.archived_root.trim()),
        system_root: PathBuf::from(s.system_root.trim()),
        reserve_gb,
        stable_minutes,
        min_drive_gb,
        name_prefix: s.name_prefix.trim().to_string(),
        test_archives: s.test_archives,
        winrar_path: PathBuf::from(s.winrar.trim()),
        bandizip_path: PathBuf::from(s.bandizip.trim()),
        seven_zip_path: PathBuf::from(s.seven_zip.trim()),
    })
}

fn parse_u64(s: &str, field: &str) -> Result<u64, String> {
    s.trim()
        .parse::<u64>()
        .map_err(|_| format!("「{}」必须是非负整数(当前:「{}」)。", field, s.trim()))
}

fn save_choice_to_target(c: SaveChoice) -> SaveTarget {
    match c {
        SaveChoice::CurrentSource => SaveTarget::CurrentSource,
        SaveChoice::AppData => SaveTarget::AppData,
        SaveChoice::CurrentDir => SaveTarget::CurrentDir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> SettingsUiState {
        SettingsUiState {
            ready_root: "D:/r".into(),
            archived_root: "D:/a".into(),
            system_root: "D:/s".into(),
            reserve_gb: "30".into(),
            stable_minutes: "30".into(),
            min_drive_gb: "200".into(),
            name_prefix: "备份".into(),
            test_archives: true,
            ..Default::default()
        }
    }

    #[test]
    fn elide_keeps_short_and_truncates_long_with_tail() {
        assert_eq!(elide("D:/r", 44), "D:/r");
        let long = "C:/Users/somebody/Desktop/资料库/待备份/项目目录abcdefghij";
        let e = elide(long, 20);
        assert!(e.chars().count() <= 20);
        assert!(e.starts_with('…'));
        assert!(e.ends_with("abcdefghij"), "应保留尾部:{e}");
    }

    #[test]
    fn parse_form_valid() {
        let c = parse_form(&filled()).unwrap();
        assert_eq!(c.reserve_gb, 30);
        assert_eq!(c.min_drive_gb, 200);
        assert_eq!(c.ready_root, PathBuf::from("D:/r"));
        assert_eq!(c.name_prefix, "备份");
    }

    #[test]
    fn parse_form_empty_root_errs() {
        let mut s = filled();
        s.system_root = "   ".into();
        assert!(parse_form(&s).unwrap_err().contains("备份系统"));
    }

    #[test]
    fn parse_form_bad_number_errs() {
        let mut s = filled();
        s.reserve_gb = "abc".into();
        let e = parse_form(&s).unwrap_err();
        assert!(e.contains("预留余量"), "应指明字段;得到 {}", e);
    }

    #[test]
    fn save_choice_maps_to_target() {
        assert!(matches!(
            save_choice_to_target(SaveChoice::AppData),
            SaveTarget::AppData
        ));
        assert!(matches!(
            save_choice_to_target(SaveChoice::CurrentDir),
            SaveTarget::CurrentDir
        ));
        assert!(matches!(
            save_choice_to_target(SaveChoice::CurrentSource),
            SaveTarget::CurrentSource
        ));
    }
}
