//! 设置视图: 基础设置优先，高级选项折叠；目录点选、输入校验、保存成功和失败反馈完整。

use std::path::PathBuf;

use eframe::egui;

use bftool_core::config::{Config, ConfigSource, LoadedConfig, SaveTarget};

use crate::app::App;
use crate::views::{theme, util};

/// 保存位置(GUI 暴露 3 种)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveChoice {
    CurrentSource,
    #[default]
    AppData,
    CurrentDir,
}

/// 设置页跨帧状态。
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
    pub extra_catalogs: Vec<String>,
    pub save_choice: SaveChoice,
    pub result: Option<(bool, String)>,
}

impl SettingsUiState {
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
        self.extra_catalogs = cfg
            .extra_catalogs
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        self.loaded = true;
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    theme::page_header(
        ui,
        "系统配置与目录设置",
        "管理待归档源目录、归档移入目录、系统元数据存储位置及核心安全策略。",
    );

    if !app.settings_ui.loaded {
        let cfg = app.cfg.clone();
        app.settings_ui.load_from(&cfg);
    }

    egui::ScrollArea::vertical()
        .id_salt("settings_scroll")
        .show(ui, |ui| {
            // 当前来源卡片
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::badge(ui, "生效中", theme::PRIMARY_SOFT, theme::PRIMARY);
                    ui.label(
                        egui::RichText::new(util::source_hint(&app.config_source))
                            .color(theme::TEXT_BODY),
                    );
                });
            });

            ui.add_space(theme::GAP);

            // ── 基础设置（优先呈现） ──
            theme::card(ui, |ui| {
                theme::section_title(ui, "核心归档目录设置 (基础)");
                ui.add_space(6.0);

                ui.vertical(|ui| {
                        ui.add_enabled_ui(!app.backend.is_demo(), |ui| dir_field(ui, "待备份 (源) 目录", "准备归档的项目存放于此", &mut app.settings_ui.ready_root));
                        ui.add_enabled_ui(!app.backend.is_demo(), |ui| dir_field(ui, "已备份目录", "归档成功后源文件安全移入此处", &mut app.settings_ui.archived_root));
                        ui.add_enabled_ui(!app.backend.is_demo(), |ui| dir_field(ui, "备份系统元数据目录", "存储全局索引与复查记录", &mut app.settings_ui.system_root));
                    });

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);

                theme::section_title(ui, "核心安全参数 (基础)");
                ui.add_space(6.0);

                egui::Grid::new("settings_params_grid")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        field_with_desc(ui, "预留磁盘余量 (GB)：", "备份盘剩余空间低于此值时自动封盘防写满", &mut app.settings_ui.reserve_gb);
                        field_with_desc(ui, "文件稳定期 (分钟)：", "源文件在此时间内未修改才允许归档，防半写数据", &mut app.settings_ui.stable_minutes);

                    });
                ui.checkbox(&mut app.settings_ui.test_archives, "开启压缩包内部解压测试 (强烈推荐)");
            });

            ui.add_space(theme::GAP);

            // ── 高级选项（默认折叠） ──
            theme::card(ui, |ui| {
                ui.collapsing("高级与扩展配置 (选填)", |ui| {
                    ui.add_space(4.0);
                    egui::Grid::new("settings_advanced_grid")
                        .num_columns(2)
                        .spacing([14.0, 10.0])
                        .show(ui, |ui| {
                            field_with_desc(ui, "认盘最小容量 (GB)：", "过滤较小的移动 U 盘，仅认正规大容量备份盘", &mut app.settings_ui.min_drive_gb);
                            field_with_desc(ui, "备份盘命名前缀：", "初始化盘默认编号前缀（如「备份」对应「备份1」）", &mut app.settings_ui.name_prefix);
                        });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);
                    theme::section_title(ui, "第三方解压测试器路径 (可选外部工具)");
                    ui.add_space(4.0);

                    ui.vertical(|ui| {
                            ui.add_enabled_ui(!app.backend.is_demo(), |ui| file_field(ui, "WinRAR (WinRAR.exe)", &mut app.settings_ui.winrar));
                            ui.add_enabled_ui(!app.backend.is_demo(), |ui| file_field(ui, "Bandizip (Bandizip.exe)", &mut app.settings_ui.bandizip));
                            ui.add_enabled_ui(!app.backend.is_demo(), |ui| file_field(ui, "7-Zip (7z.exe)", &mut app.settings_ui.seven_zip));
                        });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(4.0);

                    ui.add_enabled_ui(!app.backend.is_demo(), |ui| extra_catalogs_section(ui, &mut app.settings_ui.extra_catalogs));
                });
            });

            ui.add_space(theme::GAP);

            // ── 保存配置卡片 ──
            theme::card(ui, |ui| {
                theme::section_title(ui, "保存配置位置");
                ui.add_space(4.0);

                let is_default = matches!(app.config_source, ConfigSource::Default);
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(
                        &mut app.settings_ui.save_choice,
                        SaveChoice::AppData,
                        "%APPDATA% (推荐，跟随当前登录用户)",
                    );
                    ui.add_enabled_ui(!is_default, |ui| {
                        ui.selectable_value(
                            &mut app.settings_ui.save_choice,
                            SaveChoice::CurrentSource,
                            "写回当前来源文件",
                        );
                    });
                    ui.selectable_value(
                        &mut app.settings_ui.save_choice,
                        SaveChoice::CurrentDir,
                        "程序所在当前目录",
                    );
                });

                if is_default && app.settings_ui.save_choice == SaveChoice::CurrentSource {
                    app.settings_ui.save_choice = SaveChoice::AppData;
                }

                if app.settings_ui.save_choice == SaveChoice::CurrentDir {
                    ui.add_space(4.0);
                    theme::callout_with_tag(
                        ui,
                        theme::WARN,
                        theme::WARN_SOFT,
                        "位置提示",
                        "GUI 双击启动时，当前目录为程序 .exe 所在目录，移动程序可能导致配置丢失。建议优先选用 %APPDATA%。",
                    );
                }

                ui.add_space(8.0);
                if ui.add_enabled(!app.is_busy(), theme::btn_primary(&app.operation_label("保存并应用设置"))).clicked() {
                    do_save(app);
                }

                // 反馈提示
                if let Some((ok, msg)) = &app.settings_ui.result {
                    ui.add_space(6.0);
                    if *ok {
                        theme::callout_with_tag(ui, theme::OK, theme::OK_SOFT, "保存成功", msg);
                    } else {
                        util::error_banner(
                            ui,
                            "配置校验未通过，保存已中止",
                            msg,
                            "请检查标红或上述提示字段，修正后再次点击保存。",
                        );
                    }
                }
            });
        });
}

fn dir_field(ui: &mut egui::Ui, label: &str, desc: &str, value: &mut String) {
    ui.vertical(|ui| {
        ui.label(
            egui::RichText::new(label)
                .font(theme::subtitle_font())
                .color(theme::TEXT_TITLE),
        );
        ui.label(
            egui::RichText::new(desc)
                .size(11.0)
                .color(theme::TEXT_MUTED),
        );
        ui.horizontal_wrapped(|ui| {
            path_display(ui, value);

            if ui.add(theme::btn_secondary("📂 选择目录…")).clicked() {
                let mut dlg = rfd::FileDialog::new().set_title(format!("选择 {label}"));
                let cur = value.trim();
                if !cur.is_empty() {
                    dlg = dlg.set_directory(cur);
                }
                if let Some(p) = dlg.pick_folder() {
                    *value = p.display().to_string();
                }
            }
        });
        ui.add_space(6.0);
    });
}

fn field_with_desc(ui: &mut egui::Ui, label: &str, desc: &str, value: &mut String) {
    ui.vertical(|ui| {
        ui.set_max_width(170.0);
        ui.label(label);
        ui.label(
            egui::RichText::new(desc)
                .size(11.0)
                .color(theme::TEXT_MUTED),
        );
    });
    ui.add(egui::TextEdit::singleline(value).desired_width(140.0));
    ui.end_row();
}

fn file_field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.vertical(|ui| {
        ui.label(label);
        path_display(ui, value);
        ui.horizontal(|ui| {
            if ui.button("选择…").clicked() {
                let mut dlg = rfd::FileDialog::new()
                    .set_title(format!("选择 {label} 可执行文件"))
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
        });
        ui.add_space(6.0);
    });
}

fn path_display(ui: &mut egui::Ui, value: &str) {
    let v = value.trim();
    if v.is_empty() {
        ui.colored_label(theme::TEXT_MUTED, "(未指定路径)");
    } else {
        util::copyable_path(ui, v, 32);
    }
}

pub fn elide(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let tail: String = chars[chars.len() - (max - 1)..].iter().collect();
    format!("…{tail}")
}

fn extra_catalogs_section(ui: &mut egui::Ui, items: &mut Vec<String>) {
    theme::section_title(ui, "多机汇总查询：额外索引来源 (跨机检索)");
    ui.label(
        egui::RichText::new("把其它电脑的「备份索引名单.csv」拷到本机后添加至此，即可在「查找」页跨机检索全部备份。")
            .size(11.5)
            .color(theme::TEXT_MUTED),
    );
    ui.add_space(4.0);
    if ui.button("➕ 添加外部索引文件…").clicked() {
        let picked = rfd::FileDialog::new()
            .set_title("选择其它电脑的 备份索引名单.csv（支持多选）")
            .add_filter("CSV 索引文件", &["csv"])
            .pick_files();
        if let Some(paths) = picked {
            for p in paths {
                let s = p.display().to_string();
                if !items.iter().any(|e| e == &s) {
                    items.push(s);
                }
            }
        }
    }
    if items.is_empty() {
        ui.colored_label(theme::TEXT_MUTED, "(未添加额外来源，当前仅检索本机总索引)");
    } else {
        let mut remove: Option<usize> = None;
        for (i, path) in items.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.button("移除").clicked() {
                    remove = Some(i);
                }
                util::copyable_path(ui, path, 40);
            });
        }
        if let Some(i) = remove {
            items.remove(i);
        }
    }
}

fn do_save(app: &mut App) {
    if !app.ensure_idle() {
        return;
    }
    match parse_form(&app.settings_ui) {
        Err(msg) => app.settings_ui.result = Some((false, msg)),
        Ok(cfg) => {
            let target = save_choice_to_target(app.settings_ui.save_choice);
            let loaded = LoadedConfig {
                config: cfg.clone(),
                source: app.config_source.clone(),
            };
            match app.backend.save(&loaded, &target) {
                Ok(path) => {
                    app.cfg = cfg;
                    app.archive_plan = None;
                    app.archive_plan_inputs = None;
                    app.status_cache = None;
                    app.drives_cache = None;
                    app.verify_ui.drives = None;
                    if let Some(path) = path {
                        app.config_source = ConfigSource::Candidate(path.clone());
                        app.settings_ui.result =
                            Some((true, format!("配置已成功保存至 {}", path.display())));
                    } else {
                        app.settings_ui.result =
                            Some((true, "【演示】仅在内存中应用，未写入配置文件。".into()));
                    }
                }
                Err(e) => app.settings_ui.result = Some((false, format!("保存失败：{:#}", e))),
            }
        }
    }
}

pub fn parse_form(s: &SettingsUiState) -> Result<Config, String> {
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
        extra_catalogs: s
            .extra_catalogs
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect(),
        watch_source: None,
        enable_watch: false,
        watch_poll_secs: 60,
        incremental_verify: Default::default(),
    })
}

fn parse_u64(text: &str, name: &str) -> Result<u64, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err(format!("「{}」不能为空。", name));
    }
    t.parse::<u64>()
        .map_err(|_| format!("「{}」必须是非负整数(当前:「{}」)。", name, t))
}

pub fn save_choice_to_target(c: SaveChoice) -> SaveTarget {
    match c {
        SaveChoice::CurrentSource => SaveTarget::CurrentSource,
        SaveChoice::AppData => SaveTarget::AppData,
        SaveChoice::CurrentDir => SaveTarget::CurrentDir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regression_busy_settings_returns_before_form_validation_or_save() {
        let mut app = crate::app::tests::fixture();
        crate::app::tests::occupy_find(&mut app);
        do_save(&mut app);
        assert!(
            app.settings_ui.result.is_none(),
            "busy action must not validate or save the form"
        );
    }

    fn valid_state() -> SettingsUiState {
        SettingsUiState {
            loaded: true,
            ready_root: r"C:\data\ready".into(),
            archived_root: r"C:\data\archived".into(),
            system_root: r"C:\data\system".into(),
            reserve_gb: "50".into(),
            stable_minutes: "30".into(),
            min_drive_gb: "64".into(),
            name_prefix: "备份".into(),
            test_archives: true,
            winrar: String::new(),
            bandizip: String::new(),
            seven_zip: String::new(),
            extra_catalogs: Vec::new(),
            save_choice: SaveChoice::AppData,
            result: None,
        }
    }

    #[test]
    fn parse_form_valid() {
        let s = valid_state();
        let cfg = parse_form(&s).expect("valid");
        assert_eq!(cfg.reserve_gb, 50);
        assert_eq!(cfg.stable_minutes, 30);
        assert_eq!(cfg.min_drive_gb, 64);
        assert_eq!(cfg.name_prefix, "备份");
        assert!(cfg.test_archives);
    }

    #[test]
    fn parse_form_empty_root_errs() {
        let mut s = valid_state();
        s.ready_root = "   ".into();
        let err = parse_form(&s).unwrap_err();
        assert!(err.contains("「待备份」目录不能为空"));
    }

    #[test]
    fn parse_form_bad_number_errs() {
        let mut s = valid_state();
        s.reserve_gb = "-1".into();
        assert!(parse_form(&s).unwrap_err().contains("预留余量(GB)"));
        s.reserve_gb = "abc".into();
        assert!(parse_form(&s).unwrap_err().contains("预留余量(GB)"));
    }

    #[test]
    fn parse_form_preserves_relative_root_for_save() {
        let mut s = valid_state();
        s.ready_root = "待备份".into();
        let cfg = parse_form(&s).expect("parse preserves relative");
        assert_eq!(cfg.ready_root, PathBuf::from("待备份"));
    }

    #[test]
    fn parse_form_collects_extra_catalogs_dropping_blanks() {
        let mut s = valid_state();
        s.extra_catalogs = vec![
            r"C:\cat1.csv".into(),
            "   ".into(),
            r"D:\cat2.csv".into(),
            "".into(),
        ];
        let cfg = parse_form(&s).expect("parse extra_catalogs");
        assert_eq!(
            cfg.extra_catalogs,
            vec![PathBuf::from(r"C:\cat1.csv"), PathBuf::from(r"D:\cat2.csv")]
        );
    }

    #[test]
    fn save_choice_maps_to_target() {
        assert!(matches!(
            save_choice_to_target(SaveChoice::CurrentSource),
            SaveTarget::CurrentSource
        ));
        assert!(matches!(
            save_choice_to_target(SaveChoice::AppData),
            SaveTarget::AppData
        ));
        assert!(matches!(
            save_choice_to_target(SaveChoice::CurrentDir),
            SaveTarget::CurrentDir
        ));
    }

    #[test]
    fn elide_keeps_short_and_truncates_long_with_tail() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("exact-10ch", 10), "exact-10ch");
        assert_eq!(elide("12345678901", 10), "…345678901");
    }
}
