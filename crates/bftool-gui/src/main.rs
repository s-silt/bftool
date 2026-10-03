//! Native entry. Select the dependency mode before App/config initialization.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use bftool_gui::app::App;
use bftool_gui::screenshot::ScreenshotRunner;
use std::path::PathBuf;

#[derive(Debug)]
struct Startup {
    screenshot_dir: Option<PathBuf>,
    demo: bool,
    narrow: bool,
    zoom: f32,
}

impl Startup {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut result = Self {
            screenshot_dir: None,
            demo: false,
            narrow: false,
            zoom: 1.0,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--demo" => result.demo = true,
                "--demo-screenshot" => {
                    let output = args.next().ok_or("missing screenshot output directory")?;
                    if output.trim().is_empty() || output.starts_with("--") {
                        return Err("missing or invalid screenshot output directory".into());
                    }
                    result.screenshot_dir = Some(PathBuf::from(output));
                    result.demo = true;
                }
                "--narrow" => result.narrow = true,
                "--demo-zoom" => {
                    result.zoom = args
                        .next()
                        .ok_or("missing application zoom")?
                        .parse()
                        .map_err(|e| format!("invalid application zoom: {e}"))?;
                }
                _ => return Err(format!("unknown argument: {arg}")),
            }
        }
        if !result.zoom.is_finite()
            || !(0.5..=2.0).contains(&result.zoom)
            || (!result.demo && result.zoom != 1.0)
        {
            return Err("application zoom requires demo and a finite scale in 0.5..=2.0".into());
        }
        Ok(result)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Startup {
        screenshot_dir,
        demo,
        narrow,
        zoom,
    } = Startup::parse(std::env::args().skip(1))?;
    // Validate output before creating any App. Failures are process failures.
    let runner = screenshot_dir.map(ScreenshotRunner::new).transpose()?;
    let completion = runner.as_ref().map(|r| r.completion.clone());
    let size = if narrow {
        [880.0, 600.0]
    } else {
        [1200.0, 820.0]
    };
    let title = if demo {
        format!("bftool 演示纯合成 pid {}", std::process::id())
    } else {
        "归档备份工具 bftool".into()
    };
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size([560.0, 400.0])
            .with_title(title),
        ..Default::default()
    };
    eframe::run_native(
        "bftool",
        opts,
        Box::new(move |cc| {
            cc.egui_ctx.set_zoom_factor(zoom);
            let mut app = if demo {
                App::new_demo(cc)
            } else {
                App::new(cc)
            };
            app.screenshot_runner = runner;
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| format!("GUI startup/runtime failed: {e}"))?;
    if let Some(completion) = completion {
        match completion
            .lock()
            .map_err(|_| "screenshot result lock poisoned")?
            .as_ref()
        {
            Some(Ok(8)) => {}
            Some(Ok(n)) => return Err(format!("incomplete screenshot set: {n}/8").into()),
            Some(Err(e)) => return Err(e.clone().into()),
            None => return Err("GUI closed before all screenshots completed".into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Startup, String> {
        Startup::parse(args.iter().map(|s| s.to_string()))
    }
    #[test]
    fn demo_flags_select_demo_before_any_app_or_config_is_created() {
        assert!(!parse(&[]).unwrap().demo);
        assert!(parse(&["--demo"]).unwrap().demo);
        let capture = parse(&[
            "--demo-screenshot",
            "candidate-only",
            "--narrow",
            "--demo-zoom",
            "1.5",
        ])
        .unwrap();
        assert!(capture.demo && capture.narrow);
        assert_eq!(capture.zoom, 1.5);
        assert_eq!(
            capture.screenshot_dir,
            Some(PathBuf::from("candidate-only"))
        );
    }
    #[test]
    fn invalid_startup_arguments_fail_before_app_initialization() {
        for args in [
            vec!["--demo-screenshot"],
            vec!["--demo-screenshot", "--narrow"],
            vec!["--demo-screenshot", "   "],
            vec!["--demo-zoom"],
            vec!["--demo", "--demo-zoom", "NaN"],
            vec!["--demo", "--demo-zoom", "3.0"],
            vec!["--demo-zoom", "1.25"],
            vec!["--unknown"],
        ] {
            assert!(parse(&args).is_err(), "should reject {args:?}");
        }
    }
}
