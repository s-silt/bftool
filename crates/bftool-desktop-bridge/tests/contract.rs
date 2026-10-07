use bftool_desktop_bridge::*;
use std::{fs, sync::Arc, thread, time::Duration};

fn fixture() -> (tempfile::TempDir, PreviewRequest) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("中文源目录");
    let target = dir.path().join("备份目标");
    fs::create_dir_all(source.join("子目录")).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("甲.ZIP"), b"safe payload").unwrap();
    fs::write(source.join("skip.txt"), b"skip").unwrap();
    fs::write(source.join("子目录/乙.zip"), b"nested").unwrap();
    let request = PreviewRequest {
        revision: "1".into(),
        target: target.to_string_lossy().into(),
        sources: vec![SourceInput {
            path: source.to_string_lossy().into(),
            kind: SourceKind::Directory,
            recursive: false,
            suffixes: "zip".into(),
            include_extensionless: false,
        }],
    };
    (dir, request)
}

#[test]
fn preview_is_readonly_shallow_and_token_is_invalidated() {
    let (_dir, request) = fixture();
    let engine = Engine::default();
    engine.invalidate("1").unwrap();
    let view = engine.preview(request.clone()).unwrap();
    assert_eq!(view.files, "1");
    assert!(!std::path::Path::new(&request.target)
        .join(".bftool-backup")
        .exists());
    let entries = engine.entries(&view.plan_id, 0, 100).unwrap();
    assert_eq!(entries.iter().filter(|e| e.kind == "File").count(), 1);
    assert!(entries.iter().any(|e| e.relative_path == "甲.ZIP"));
    assert!(entries
        .iter()
        .any(|e| e.kind == "Directory" && e.relative_path.is_empty()));
    engine.invalidate("2").unwrap();
    let error = engine.start("2", &view.plan_id).unwrap_err();
    assert_eq!(error.code, "STALE_PLAN");
    assert_eq!(engine.preview(request).unwrap_err().code, "STALE_REQUEST");
}

#[test]
fn copy_preserves_source_consumes_token_and_retains_terminal_result() {
    let (_dir, request) = fixture();
    let source = request.sources[0].path.clone();
    let engine = Arc::new(Engine::default());
    engine.invalidate("1").unwrap();
    let view = engine.preview(request.clone()).unwrap();
    let job = engine.start("1", &view.plan_id).unwrap();
    assert!(engine.start("1", &view.plan_id).is_err());
    for _ in 0..400 {
        if engine.snapshot().unwrap().is_some_and(|s| s.terminal) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let result = engine.snapshot().unwrap().unwrap();
    assert_eq!(result.job_id, job);
    assert_eq!(result.phase, "completed");
    assert_eq!(result.results.len(), 1);
    assert!(result.results[0].published);
    assert_eq!(
        fs::read(std::path::Path::new(&source).join("甲.ZIP")).unwrap(),
        b"safe payload"
    );
    assert_eq!(engine.history(&request.target).unwrap().len(), 1);
    assert!(engine.snapshot().unwrap().unwrap().terminal);
}

#[test]
fn overlap_and_unknown_json_fields_are_rejected() {
    let (_dir, mut request) = fixture();
    request.sources.push(request.sources[0].clone());
    let engine = Engine::default();
    engine.invalidate("1").unwrap();
    assert_eq!(engine.preview(request).unwrap_err().code, "INVALID_INPUT");
    assert!(serde_json::from_str::<PreviewRequest>(
        r#"{"revision":"1","target":"C:/tmp","sources":[],"entries":[]}"#
    )
    .is_err());
}

fn await_idle(engine: &Engine) -> Snapshot {
    for _ in 0..1000 {
        if !engine.is_busy() {
            return engine.snapshot().unwrap().unwrap();
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("synthetic operation did not terminate");
}

#[cfg(windows)]
#[test]
fn long_target_publication_preserves_keep_both_without_core_changes() {
    let (dir, mut request) = fixture();
    let source = dir.path().join(format!("long_{}.bin", "s".repeat(110)));
    fs::write(&source, b"synthetic long-path payload").unwrap();
    let mut target = std::path::PathBuf::from(&request.target);
    while target
        .join(source.file_name().unwrap())
        .to_string_lossy()
        .encode_utf16()
        .count()
        < 320
    {
        target.push("deep_target_segment".repeat(4));
    }
    fs::create_dir_all(&target).unwrap();
    let occupied = target.join(source.file_name().unwrap());
    fs::write(&occupied, b"keep preexisting payload").unwrap();
    request.target = target.to_string_lossy().into();
    request.sources = vec![SourceInput {
        path: source.to_string_lossy().into(),
        kind: SourceKind::File,
        recursive: false,
        suffixes: String::new(),
        include_extensionless: true,
    }];
    let engine = Engine::default();
    engine.invalidate("1").unwrap();
    let plan = engine.preview(request.clone()).unwrap();
    let published = target.join(&plan.destinations[0]);
    assert!(published.to_string_lossy().encode_utf16().count() >= 300);
    engine.start("1", &plan.plan_id).unwrap();
    let result = await_idle(&engine);
    assert_eq!(result.phase, "completed", "long-path result: {result:?}");
    assert!(result.results[0].published);
    assert_eq!(fs::read(&occupied).unwrap(), b"keep preexisting payload");
    assert_eq!(fs::read(&source).unwrap(), b"synthetic long-path payload");
    assert_eq!(
        fs::read(&published).unwrap(),
        b"synthetic long-path payload"
    );
    let verified = engine.verify(&request.target).unwrap();
    assert_eq!(verified.checked, "1");
    assert_eq!(verified.bad, "0");
}

#[test]
fn changed_source_and_occupied_destination_fail_closed() {
    let (_dir, request) = fixture();
    let engine = Engine::default();
    engine.invalidate("1").unwrap();
    let view = engine.preview(request.clone()).unwrap();
    fs::write(
        std::path::Path::new(&request.sources[0].path).join("甲.ZIP"),
        b"new content",
    )
    .unwrap();
    engine.start("1", &view.plan_id).unwrap();
    let snapshot = await_idle(&engine);
    assert_eq!(snapshot.phase, "failed");
    assert_eq!(
        fs::read(std::path::Path::new(&request.sources[0].path).join("甲.ZIP")).unwrap(),
        b"new content"
    );
    engine.invalidate("2").unwrap();
    let mut request = request;
    request.revision = "2".into();
    let view = engine.preview(request.clone()).unwrap();
    let occupied = std::path::Path::new(&request.target).join(&view.destinations[0]);
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("unknown"), b"do not overwrite").unwrap();
    engine.start("2", &view.plan_id).unwrap();
    assert_eq!(await_idle(&engine).phase, "failed");
    assert_eq!(
        fs::read(occupied.join("unknown")).unwrap(),
        b"do not overwrite"
    );
}

#[test]
fn cancellation_is_real_terminal_and_does_not_erase_source() {
    let (_dir, request) = fixture();
    let source = std::path::Path::new(&request.sources[0].path).join("large.zip");
    let file = fs::File::create(&source).unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    drop(file);
    let engine = Engine::default();
    engine.invalidate("1").unwrap();
    let view = engine.preview(request).unwrap();
    let job = engine.start("1", &view.plan_id).unwrap();
    engine.cancel(&job).unwrap();
    let final_state = await_idle(&engine);
    assert!(final_state.terminal);
    assert_eq!(final_state.phase, "cancelled");
    assert!(final_state.results.iter().all(|r| !r.published));
    assert_eq!(fs::metadata(source).unwrap().len(), 64 * 1024 * 1024);
    assert_eq!(engine.cancel(&job).unwrap_err().code, "STALE_JOB");
}

#[test]
fn successful_backup_has_real_sha256_verification_and_detects_tamper() {
    let (_dir, request) = fixture();
    let engine = Engine::default();
    engine.invalidate("1").unwrap();
    let view = engine.preview(request.clone()).unwrap();
    engine.start("1", &view.plan_id).unwrap();
    assert_eq!(await_idle(&engine).phase, "completed");
    let verified = engine.verify(&request.target).unwrap();
    assert_eq!(verified.bad, "0");
    assert_eq!(verified.size_only, "0");
    assert!(verified.checked.parse::<u64>().unwrap() > 0);
    let payload = std::path::Path::new(&request.target)
        .join(&view.destinations[0])
        .join("甲.ZIP");
    fs::write(payload, b"tampered").unwrap();
    assert!(
        engine
            .verify(&request.target)
            .unwrap()
            .bad
            .parse::<u64>()
            .unwrap()
            > 0
    );
}

#[test]
fn single_file_keep_both_preserves_preexisting_destination() {
    let (dir, mut request) = fixture();
    let source = dir.path().join("单文件.bin");
    fs::write(&source, b"selected single file").unwrap();
    let existing = std::path::Path::new(&request.target).join("单文件.bin");
    fs::write(&existing, b"preexisting destination").unwrap();
    request.sources = vec![SourceInput {
        path: source.to_string_lossy().into(),
        kind: SourceKind::File,
        recursive: false,
        suffixes: String::new(),
        include_extensionless: true,
    }];
    let engine = Engine::default();
    engine.invalidate("1").unwrap();
    let plan = engine.preview(request.clone()).unwrap();
    assert_eq!(plan.files, "1");
    assert_ne!(plan.destinations[0], "单文件.bin");
    engine.start("1", &plan.plan_id).unwrap();
    assert_eq!(await_idle(&engine).phase, "completed");
    assert_eq!(fs::read(&source).unwrap(), b"selected single file");
    assert_eq!(fs::read(&existing).unwrap(), b"preexisting destination");
    assert_eq!(
        fs::read(std::path::Path::new(&request.target).join(&plan.destinations[0])).unwrap(),
        b"selected single file"
    );
    assert_eq!(engine.verify(&request.target).unwrap().bad, "0");
}
