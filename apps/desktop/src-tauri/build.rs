fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "choose_paths",
            "input_revision",
            "invalidate_plan",
            "preview_backup",
            "plan_entries",
            "start_backup",
            "resume_backup",
            "job_snapshot",
            "cancel_job",
            "backup_history",
            "verify_backup",
        ]),
    ))
    .expect("build explicit command ACL");
}
