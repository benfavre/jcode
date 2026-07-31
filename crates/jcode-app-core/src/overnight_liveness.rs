//! Liveness reconciliation for persisted overnight manifests.
//!
//! An overnight run records the pid that owns it (`OvernightManifest::process_id`)
//! but nothing ever read it back, so a run whose process died left a manifest
//! sitting in `Running` forever: `/overnight status` reported a live run and
//! `/overnight cancel` wrote `CancelRequested` into a run with no supervisor left
//! to observe it.
//!
//! Reconciliation is deliberately read-only and side-effect free. `record_event`
//! loads the manifest while writing, so persisting from inside `load_manifest`
//! would re-enter this path; and `maybe_refresh_overnight_display_card` polls
//! `latest_manifest` every 5s, so logging here would repeat forever once a run
//! goes stale. Reporting the corrected status and writing nothing keeps both
//! paths clean, and leaves persistence to the code that already owns run
//! lifecycle transitions.

use jcode_overnight_core::{OvernightManifest, reconciled_status};

/// Return `manifest` with its status corrected if the process that owns the run
/// is no longer alive. A live (or terminal) run is returned untouched.
pub(crate) fn reconcile(manifest: OvernightManifest) -> OvernightManifest {
    let alive = crate::platform::is_process_running(manifest.process_id);
    reconcile_with(manifest, alive)
}

/// Liveness-injectable core of [`reconcile`], so the decision can be tested
/// without spawning or killing real processes.
fn reconcile_with(mut manifest: OvernightManifest, process_alive: bool) -> OvernightManifest {
    if let Some(status) = reconciled_status(&manifest.status, process_alive) {
        manifest.status = status;
    }
    manifest
}

#[cfg(test)]
#[path = "overnight_liveness_tests.rs"]
mod overnight_liveness_tests;
