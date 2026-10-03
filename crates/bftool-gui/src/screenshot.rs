//! Pure synthetic fixtures and explicitly requested screenshot artifacts.
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::app::{App, ArchivePlanInputs, View};
use crate::backend::{synthetic_config, Backend};
use crate::views::verify::VerifyStats;
use bftool_core::engine::verify::{ExtraFile, VerifyReport};
use eframe::egui;

pub fn inject_demo_data(app: &mut App) {
    app.backend = Backend::Demo;
    app.cfg = synthetic_config();
    app.config_source = bftool_core::config::ConfigSource::Default;
    app.status_cache = Some((
        app.backend.status(&app.cfg).expect("pure fixture"),
        Instant::now(),
    ));
    let opts = app.archive_ui.to_options().unwrap_or_default();
    app.archive_plan = Some(crate::backend::demo_plan(&app.cfg, &opts));
    app.archive_plan_inputs = Some(ArchivePlanInputs {
        cfg: app.cfg.clone(),
        opts,
    });
    app.drives_cache = Some(app.backend.drives().expect("pure fixture"));
    app.verify_ui.drives = app.drives_cache.clone();
    app.init_ui.cache = Some(app.backend.init_candidates(&app.cfg).expect("pure fixture"));
    let report = VerifyReport {
        checked: 152,
        extra: 1,
        extras: vec![ExtraFile {
            project: "演示纪录片".into(),
            rel: "演示额外文件.txt".into(),
        }],
        ..Default::default()
    };
    app.verify_ui.last_stats = Some(VerifyStats::from_report(&report));
    app.verify_ui.last_report = Some(report);
    app.verify_ui.summary = Some("【演示合成】示例统计，尚未运行生产复查".into());
    app.find_ui.keyword = "纪录片".into();
    app.find_ui.result = Some(app.backend.find(&app.cfg, "纪录片").expect("pure fixture"));
    app.watch_ui.folder = "【演示】/增量输入投放区".into();
    app.watch_ui.ext_text = "mov, mp4, prproj, zip, 7z".into();
    app.watch_ui.recursive = true;
    app.watch_ui.poll_secs_text = "60".into();
    app.watch_ui.last_msg = Some("【演示】尚未启动，模拟任务不读写任何素材".into());
    app.settings_ui.loaded = false;
    app.settings_ui.ready_root = app.cfg.ready_root.display().to_string();
    app.settings_ui.archived_root = app.cfg.archived_root.display().to_string();
    app.settings_ui.system_root = app.cfg.system_root.display().to_string();
    app.last_summary = Some("【演示合成】示例数据；无生产任务执行记录".into());
}

pub type CaptureCompletion = Arc<Mutex<Option<Result<usize, String>>>>;

pub struct ScreenshotRunner {
    pub output_dir: PathBuf,
    pub requested_screenshot: bool,
    pub settle_frames: usize,
    pub saved_count: usize,
    pub completion: CaptureCompletion,
    waiting_since: Option<Instant>,
}

impl ScreenshotRunner {
    pub fn new(output_dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&output_dir)?;
        Ok(Self {
            output_dir,
            requested_screenshot: false,
            settle_frames: 5,
            saved_count: 0,
            completion: Arc::new(Mutex::new(None)),
            waiting_since: None,
        })
    }

    fn finish(&mut self, result: Result<usize, String>, ctx: &egui::Context) {
        match &result {
            Ok(n) => eprintln!("[screenshot] {n} pages saved"),
            Err(e) => eprintln!("[screenshot] FAILED: {e}"),
        }
        if let Ok(mut completion) = self.completion.lock() {
            *completion = Some(result);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    pub fn step(&mut self, app: &mut App, ctx: &egui::Context) {
        const VIEWS: [(View, &str); 8] = [
            (View::Dashboard, "01_dashboard"),
            (View::Archive, "02_archive"),
            (View::Verify, "03_verify"),
            (View::Init, "04_init"),
            (View::Find, "05_find"),
            (View::Drives, "06_drives"),
            (View::Settings, "07_settings"),
            (View::Watch, "08_watch"),
        ];
        if self.completion.lock().map(|c| c.is_some()).unwrap_or(true) {
            return;
        }
        if !app.backend.is_demo() {
            self.finish(
                Err("screenshot runner requires pure demo backend".into()),
                ctx,
            );
            return;
        }
        let events = ctx.input(|i| i.raw.events.clone());
        for event in events {
            if let egui::Event::Screenshot {
                image, user_data, ..
            } = event
            {
                let page = user_data
                    .data
                    .as_ref()
                    .and_then(|d| d.downcast_ref::<usize>())
                    .copied();
                if !self.requested_screenshot || page != Some(self.saved_count) {
                    continue;
                }
                let path = self
                    .output_dir
                    .join(format!("{}.bmp", VIEWS[self.saved_count].1));
                if let Err(e) = save_image_bmp(&image, &path) {
                    self.finish(Err(format!("{}: {e}", path.display())), ctx);
                    return;
                }
                self.saved_count += 1;
                self.requested_screenshot = false;
                self.waiting_since = None;
                self.settle_frames = 4;
            }
        }
        if self.saved_count == VIEWS.len() {
            self.finish(Ok(self.saved_count), ctx);
            return;
        }
        if self
            .waiting_since
            .is_some_and(|t| t.elapsed() > Duration::from_secs(15))
        {
            self.finish(Err("screenshot response timed out".into()), ctx);
            return;
        }
        app.view = VIEWS[self.saved_count].0;
        if self.settle_frames > 0 {
            self.settle_frames -= 1;
        } else if !self.requested_screenshot {
            self.requested_screenshot = true;
            self.waiting_since = Some(Instant::now());
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                self.saved_count,
            )));
        }
        ctx.request_repaint();
    }
}

/// 将 egui ColorImage 保存为无压缩 32 位 BMP 图片文件
pub fn save_image_bmp(image: &egui::ColorImage, path: &Path) -> std::io::Result<()> {
    let width = image.size[0] as u32;
    let height = image.size[1] as u32;
    let row_size = width * 4;
    let image_size = row_size * height;
    let file_size = 54 + image_size;

    let f = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut writer = BufWriter::new(f);

    // 14 字节 BMP 文件头
    writer.write_all(b"BM")?;
    writer.write_all(&file_size.to_le_bytes())?;
    writer.write_all(&[0u8; 4])?; // 保留
    writer.write_all(&54u32.to_le_bytes())?; // 像素偏移量

    // 40 字节 DIB 头 (BITMAPINFOHEADER)
    writer.write_all(&40u32.to_le_bytes())?;
    writer.write_all(&(width as i32).to_le_bytes())?;
    writer.write_all(&(-(height as i32)).to_le_bytes())?; // 负高表示从上到下顺序
    writer.write_all(&1u16.to_le_bytes())?; // planes
    writer.write_all(&32u16.to_le_bytes())?; // bpp
    writer.write_all(&0u32.to_le_bytes())?; // BI_RGB 无压缩
    writer.write_all(&image_size.to_le_bytes())?;
    writer.write_all(&2835u32.to_le_bytes())?; // 水平分辨率 (~72 dpi)
    writer.write_all(&2835u32.to_le_bytes())?; // 垂直分辨率
    writer.write_all(&0u32.to_le_bytes())?;
    writer.write_all(&0u32.to_le_bytes())?;

    // 像素数据: egui 为 RGBA，BMP 32位为 BGRA
    for pixel in &image.pixels {
        writer.write_all(&[pixel.b(), pixel.g(), pixel.r(), pixel.a()])?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regression_screenshot_preserves_existing_output() {
        let root = std::env::temp_dir().join(format!(
            "bftool-shot-collision-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let file = root.join("existing.bmp");
        std::fs::write(&file, b"existing evidence").unwrap();
        let result = save_image_bmp(&egui::ColorImage::new([2, 2], egui::Color32::WHITE), &file);
        let preserved = std::fs::read(&file).unwrap() == b"existing evidence";
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(root).unwrap();
        assert!(
            result.is_err(),
            "existing output must fail rather than be replaced"
        );
        assert!(preserved);
    }

    #[test]
    fn regression_failed_screenshot_never_advances_success_count() {
        let root = std::env::temp_dir().join(format!(
            "bftool-shot-red-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let out = root.join("output");
        let mut runner = ScreenshotRunner::new(out.clone()).unwrap();
        runner.requested_screenshot = true;
        std::fs::remove_dir(&out).unwrap();
        std::fs::write(&out, b"not a directory").unwrap();
        let mut app = crate::app::tests::fixture();
        inject_demo_data(&mut app);
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            events: vec![egui::Event::Screenshot {
                viewport_id: egui::ViewportId::ROOT,
                user_data: egui::UserData::new(0usize),
                image: std::sync::Arc::new(egui::ColorImage::new([2, 2], egui::Color32::WHITE)),
            }],
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| runner.step(&mut app, ctx));
        std::fs::remove_file(out).unwrap();
        std::fs::remove_dir(root).unwrap();
        assert_eq!(
            runner.saved_count, 0,
            "failed output is not a completed page"
        );
        let result = runner.completion.lock().unwrap();
        assert!(result
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .contains("output"));
    }
}
