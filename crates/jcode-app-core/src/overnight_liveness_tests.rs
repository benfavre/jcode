use super::{reconcile, reconcile_with};
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

// The tests above inject liveness directly. These drive `reconcile` itself, so
// that the pid -> bool wiring is covered too: without them, inverting the check
// inside `reconcile` would leave every other test in this file passing.

/// pid 0 must never read as alive. `kill(0, 0)` signals the caller's own process
/// group and succeeds, so delegating straight to `platform::is_process_running`
/// would report a corrupted manifest as permanently live.
#[test]
fn zero_pid_is_treated_as_dead() {
    let manifest = manifest_with(OvernightRunStatus::Running, 0);
    assert_eq!(reconcile(manifest).status, OvernightRunStatus::Failed);
}

#[test]
fn reconcile_keeps_a_run_owned_by_this_process_running() {
    let manifest = manifest_with(OvernightRunStatus::Running, std::process::id());
    assert_eq!(reconcile(manifest).status, OvernightRunStatus::Running);
}

#[cfg(unix)]
#[test]
fn reconcile_reports_a_reaped_child_as_dead() {
    let manifest = manifest_with(OvernightRunStatus::Running, spawn_and_reap_dead_pid());
    assert_eq!(reconcile(manifest).status, OvernightRunStatus::Failed);
}

/// Spawn a child, reap it, and confirm the pid reads as dead before handing it
/// back. Retried because the OS may recycle a pid between exit and the check.
#[cfg(unix)]
fn spawn_and_reap_dead_pid() -> u32 {
    for _ in 0..16 {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id();
        child.wait().expect("reap short-lived child");
        if !crate::platform::is_process_running(pid) {
            return pid;
        }
    }
    panic!("could not obtain a reliably-dead pid");
}
