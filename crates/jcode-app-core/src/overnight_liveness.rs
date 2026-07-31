//! Liveness reconciliation for persisted overnight manifests.
//!
//! # Why a manifest can lie
//!
//! A non-terminal `status` is not a fact about the run, it is a *claim that some
//! process is actively driving it*. Only the owning process advances that field,
//! so when it dies the manifest freezes mid-claim and nobody is left to correct
//! it: `/overnight status` reports a live run forever, and `/overnight cancel`
//! writes `CancelRequested` into a run with no supervisor left to observe it.
//!
//! The manifest already carries the witness that settles this. `process_id` is
//! recorded at launch and, before this module existed, never read back. So the
//! contradiction ("status says running, the process is gone") was always
//! resolvable from data already on disk.
//!
//! # Why correcting the read is enough
//!
//! `overnight::load_manifest` and `overnight::latest_manifest` are the only
//! paths to an overnight manifest. Deriving the status at that chokepoint fixes
//! every consumer at once, with no coordination and no call-site audit: the
//! progress card already gates on `matches!(status, Running | CancelRequested)`,
//! and `cancel_latest_run` already early-returns on terminal states. Both were
//! written correctly; they were being fed a stale claim.
//!
//! # Why nothing is written back
//!
//! `record_event` loads the manifest while writing, so persisting from inside
//! `load_manifest` would re-enter this path. `maybe_refresh_overnight_display_card`
//! polls `latest_manifest` every 5s, so logging here would repeat forever once a
//! run goes stale. Reporting the corrected status and writing nothing keeps both
//! paths free of side effects; convergence on disk belongs in an explicit
//! lifecycle sweep, not in a getter.

use jcode_overnight_core::{OvernightManifest, reconciled_status};

/// Return `manifest` with its status corrected if the process that owns the run
/// is no longer alive. A live (or terminal) run is returned untouched.
pub(crate) fn reconcile(manifest: OvernightManifest) -> OvernightManifest {
    let alive = manifest_process_is_alive(manifest.process_id);
    reconcile_with(manifest, alive)
}

/// Whether the pid recorded in a manifest names a live process.
///
/// pid 0 is rejected before delegating: `kill(0, 0)` targets the caller's own
/// process group and succeeds, so `platform::is_process_running(0)` answers
/// true. `server::reload_state::reload_process_alive` guards the same case for
/// the same reason. A manifest should never carry pid 0, since it is written
/// from `std::process::id()`, so this only matters for a corrupted or
/// hand-edited manifest, where "alive forever" is exactly the state this module
/// exists to prevent.
fn manifest_process_is_alive(process_id: u32) -> bool {
    process_id != 0 && crate::platform::is_process_running(process_id)
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
