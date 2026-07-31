use super::reconcile_with;
use chrono::Utc;
use jcode_overnight_core::{OVERNIGHT_VERSION, OvernightManifest, OvernightRunStatus};
use std::path::PathBuf;

fn manifest_with(status: OvernightRunStatus, process_id: u32) -> OvernightManifest {
    let now = Utc::now();
    let run_dir = PathBuf::from("/tmp/overnight-liveness-test");
    OvernightManifest {
        version: OVERNIGHT_VERSION,
        run_id: "run-liveness".to_string(),
        parent_session_id: "parent".to_string(),
        coordinator_session_id: "coord".to_string(),
        coordinator_session_name: "coordinator".to_string(),
        started_at: now - chrono::Duration::hours(1),
        target_wake_at: now + chrono::Duration::hours(7),
        handoff_ready_at: now + chrono::Duration::hours(6),
        post_wake_grace_until: now + chrono::Duration::hours(9),
        morning_report_posted_at: None,
        completed_at: None,
        cancel_requested_at: None,
        status,
        mission: None,
        working_dir: None,
        provider_name: "provider".to_string(),
        model: "model".to_string(),
        max_agents_guidance: 1,
        process_id,
        run_dir: run_dir.clone(),
        events_path: run_dir.join("events.jsonl"),
        human_log_path: run_dir.join("run.log"),
        review_path: run_dir.join("review.html"),
        review_notes_path: run_dir.join("notes.md"),
        preflight_path: run_dir.join("preflight.json"),
        task_cards_dir: run_dir.join("task-cards"),
        issue_drafts_dir: run_dir.join("issue-drafts"),
        validation_dir: run_dir.join("validation"),
        last_activity_at: now,
    }
}

#[test]
fn dead_process_downgrades_running_run() {
    let manifest = manifest_with(OvernightRunStatus::Running, 4242);
    let reconciled = reconcile_with(manifest, false);
    assert_eq!(reconciled.status, OvernightRunStatus::Failed);
}

#[test]
fn dead_process_resolves_stuck_cancel_request() {
    let manifest = manifest_with(OvernightRunStatus::CancelRequested, 4242);
    let reconciled = reconcile_with(manifest, false);
    assert_eq!(reconciled.status, OvernightRunStatus::Failed);
}

#[test]
fn live_process_keeps_running_status() {
    let manifest = manifest_with(OvernightRunStatus::Running, std::process::id());
    let reconciled = reconcile_with(manifest, true);
    assert_eq!(reconciled.status, OvernightRunStatus::Running);
}

#[test]
fn reconciliation_preserves_every_other_field() {
    let before = manifest_with(OvernightRunStatus::Running, 4242);
    let after = reconcile_with(before.clone(), false);

    assert_eq!(after.run_id, before.run_id);
    assert_eq!(after.process_id, before.process_id);
    assert_eq!(after.started_at, before.started_at);
    assert_eq!(after.target_wake_at, before.target_wake_at);
    assert_eq!(after.completed_at, before.completed_at);
    assert_eq!(after.cancel_requested_at, before.cancel_requested_at);
    assert_eq!(after.last_activity_at, before.last_activity_at);
    assert_eq!(after.run_dir, before.run_dir);
}

/// The reconciled status must itself be terminal, so a second pass is a no-op.
/// `record_event` re-reads the manifest while writing, so a reconciliation that
/// kept flipping would recurse.
#[test]
fn reconciliation_is_idempotent() {
    let once = reconcile_with(manifest_with(OvernightRunStatus::Running, 4242), false);
    let twice = reconcile_with(once.clone(), false);
    assert_eq!(once.status, twice.status);
    assert_eq!(twice.status, OvernightRunStatus::Failed);
}

/// The live process in this test is the test process itself, which is the one
/// pid we can assert is running without racing a spawn.
#[test]
fn current_process_is_reported_alive() {
    assert!(crate::platform::is_process_running(std::process::id()));
}
