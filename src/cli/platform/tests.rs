
use super::*;
use jcode_operator_backend::platform_contract::{
    ActionReceipt, Attachment, Capabilities, ControlLease, CursorTopic, Freshness, FreshnessState,
    PlatformCursor, PlatformRequest, PlatformResponse, PlatformText, ReceiptId, ReceiptOutcome,
    ResourceId, SessionList, SessionRecord, Snapshot,
};
use jcode_operator_backend::platform_primitives::{EpochMillis, Revision};
use jcode_operator_backend::{BackendMode, FakeBackend, OperatorOverview};

#[test]
fn explicit_socket_wins_without_touching_provider_state() {
    assert_eq!(
        platform_socket(Some("/tmp/operator.sock")).expect("socket"),
        PathBuf::from("/tmp/operator.sock")
    );
}

#[test]
fn action_keys_are_unique_for_separate_interactions() {
    assert_ne!(interaction_key("cancel"), interaction_key("cancel"));
}

fn cursor(sequence: u64) -> PlatformCursor {
    PlatformCursor {
        authority: ResourceAuthority::Automonique,
        topic: CursorTopic::new("sessions").expect("topic"),
        sequence: Revision::new(sequence).expect("sequence"),
    }
}

fn session() -> SessionRecord {
    SessionRecord {
        session: ResourceRecord {
            resource: ResourceCoordinate::new(
                ResourceAuthority::Automonique,
                ResourceKind::Session,
                ResourceId::new("session-a").expect("id"),
            ),
            freshness: Freshness {
                state: FreshnessState::Fresh,
                observed_at: EpochMillis::from_millis(1),
                revision: Revision::FIRST,
            },
            summary: PlatformText::new("open").expect("summary"),
        },
        run: None,
        attachable: true,
        controllable: true,
    }
}

fn session_state() -> CockpitState {
    let session = session();
    let mut state = CockpitState::new(OperatorOverview {
        capabilities: Capabilities::platform_v1(),
        actions: PlatformAction::ALL.to_vec(),
        resources: Vec::new(),
        sessions: vec![session],
        cursor: PlatformCursor {
            authority: ResourceAuthority::Automonique,
            topic: CursorTopic::new("resources").expect("topic"),
            sequence: Revision::FIRST,
        },
    });
    state.set_view(View::Sessions);
    state
}

fn attach_controlled_session(state: &mut CockpitState, client: &ClientId) -> ControlLease {
    let session = session();
    let attachment = Attachment {
        session: session.session.resource.clone(),
        client: client.clone(),
        cursor: cursor(1),
    };
    state.attach(session.clone(), attachment);
    let lease = ControlLease {
        id: jcode_operator_backend::platform_contract::ControlLeaseId::new("lease-steer")
            .expect("lease"),
        session: session.session.resource,
        client: client.clone(),
        expires_at: EpochMillis::from_millis(9_000_000_000_000),
        revision: Revision::new(7).expect("lease revision"),
    };
    state.panes[0].control = Some(lease.clone());
    lease
}

#[tokio::test]
async fn fake_backend_drives_attach_control_release_and_detach() {
    let client = ClientId::new("client-a").expect("client");
    let session = session();
    let attachment = Attachment {
        session: session.session.resource.clone(),
        client: client.clone(),
        cursor: cursor(1),
    };
    let lease = ControlLease {
        id: jcode_operator_backend::platform_contract::ControlLeaseId::new("lease-a")
            .expect("lease"),
        session: session.session.resource.clone(),
        client: client.clone(),
        expires_at: EpochMillis::from_millis(30_000),
        revision: Revision::FIRST,
    };
    let backend = FakeBackend::new(
        BackendMode::Managed,
        [
            Ok(PlatformResponse::Attached(attachment)),
            Ok(PlatformResponse::ControlClaimed(lease.clone())),
            Ok(PlatformResponse::ControlReleased {
                session: lease.session.clone(),
                client: client.clone(),
                lease: lease.id.clone(),
            }),
            Ok(PlatformResponse::Detached {
                session: session.session.resource.clone(),
                client: client.clone(),
            }),
        ],
    );
    let mut state = session_state();
    let mut confirmation = None;
    let mut composer = None;
    apply_command(
        AvailableCommand::Attach,
        &backend,
        &client,
        &mut state,
        &mut confirmation,
        &mut composer,
    )
    .await;
    assert_eq!(state.panes.len(), 1);
    apply_command(
        AvailableCommand::ClaimControl,
        &backend,
        &client,
        &mut state,
        &mut confirmation,
        &mut composer,
    )
    .await;
    assert!(state.panes[0].control.is_some());
    apply_command(
        AvailableCommand::ReleaseControl,
        &backend,
        &client,
        &mut state,
        &mut confirmation,
        &mut composer,
    )
    .await;
    assert!(state.panes[0].control.is_none());
    apply_command(
        AvailableCommand::Detach,
        &backend,
        &client,
        &mut state,
        &mut confirmation,
        &mut composer,
    )
    .await;
    assert!(state.panes.is_empty());
    assert_eq!(backend.requests().expect("requests").len(), 4);
}

#[test]
fn palette_uses_exact_server_actions_instead_of_inferring_from_execute() {
    let mut state = session_state();
    state.overview.actions.clear();
    assert!(!available_commands(&state).contains(&AvailableCommand::FollowUp));
    state.overview.actions = vec![PlatformAction::FollowUp];
    assert!(available_commands(&state).contains(&AvailableCommand::FollowUp));
    assert!(!available_commands(&state).contains(&AvailableCommand::StopRun));
    state.overview.sessions[0].session.freshness.state = FreshnessState::Stale;
    assert_eq!(available_commands(&state), vec![AvailableCommand::Refresh]);
}

#[tokio::test]
async fn steering_composes_and_executes_against_the_exact_live_lease() {
    let client = ClientId::new("client-steer").expect("client");
    let mut state = session_state();
    let lease = attach_controlled_session(&mut state, &client);
    state.overview.actions = vec![PlatformAction::Steer];
    assert!(available_commands(&state).contains(&AvailableCommand::Steer));

    let mut composer = None;
    open_composer(AvailableCommand::Steer, &mut state, &mut composer);
    let mut composer = composer.expect("steer composer");
    assert_eq!(composer.action, PlatformAction::Steer);
    assert_eq!(composer.target.kind, ResourceKind::ControlLease);
    assert_eq!(composer.target.id.as_str(), lease.id.as_str());
    assert_eq!(composer.expected_revision, lease.revision);
    composer.text = String::from("use the corrected live input");
    let pending = composer.into_pending().expect("pending steer");
    let receipt = ActionReceipt {
        id: ReceiptId::new("receipt-steer").expect("receipt"),
        action: PlatformAction::Steer,
        target: pending.target.clone(),
        outcome: ReceiptOutcome::Completed,
        revision: Revision::new(8).expect("receipt revision"),
        recorded_at: EpochMillis::from_millis(2),
        explanation: None,
    };
    let backend = FakeBackend::new(
        BackendMode::Managed,
        [Ok(PlatformResponse::Receipt(receipt.clone()))],
    );
    execute_confirmed(&backend, &mut state, pending).await;
    assert_eq!(state.receipts.back(), Some(&receipt));
    let requests = backend.requests().expect("requests");
    let PlatformRequest::Execute(request) = &requests[0] else {
        panic!("steer must use the canonical execute method")
    };
    assert_eq!(request.action, PlatformAction::Steer);
    assert_eq!(request.target.kind, ResourceKind::ControlLease);
    assert_eq!(request.target.id.as_str(), lease.id.as_str());
    assert_eq!(request.expected_revision, Some(lease.revision));
    assert_eq!(
        request.parameter.as_ref().map(PlatformText::as_str),
        Some("use the corrected live input")
    );
}

#[tokio::test]
async fn lease_loss_invalidates_a_steer_confirmation_without_a_request() {
    let client = ClientId::new("client-steer-loss").expect("client");
    let mut state = session_state();
    attach_controlled_session(&mut state, &client);
    state.overview.actions = vec![PlatformAction::Steer];
    let mut composer = None;
    open_composer(AvailableCommand::Steer, &mut state, &mut composer);
    let mut composer = composer.expect("steer composer");
    composer.text = String::from("must not cross a lost lease");
    let pending = composer.into_pending().expect("pending steer");
    state.panes[0].control = None;
    let backend = FakeBackend::new(BackendMode::Managed, []);
    execute_confirmed(&backend, &mut state, pending).await;
    assert!(backend.requests().expect("requests").is_empty());
    assert!(state.status.contains("invalidated"));
}

#[tokio::test]
async fn composed_follow_up_reconciles_an_ambiguous_submission_by_key() {
    let mut state = session_state();
    let target = state.overview.sessions[0].session.clone();
    let pending = RequestComposer {
        action: PlatformAction::FollowUp,
        target: target.resource.clone(),
        expected_revision: target.freshness.revision,
        target_label: coordinate_key(&target.resource),
        text: String::from("continue with the verified next step"),
    }
    .into_pending()
    .expect("pending follow-up");
    let receipt = ActionReceipt {
        id: ReceiptId::new("receipt-follow-up").expect("receipt"),
        action: PlatformAction::FollowUp,
        target: target.resource,
        outcome: ReceiptOutcome::Accepted,
        revision: Revision::FIRST,
        recorded_at: EpochMillis::from_millis(2),
        explanation: None,
    };
    let backend = FakeBackend::new(
        BackendMode::Managed,
        [
            Err(BackendError::Unavailable),
            Ok(PlatformResponse::Receipt(receipt.clone())),
        ],
    );
    execute_confirmed(&backend, &mut state, pending).await;
    assert_eq!(state.receipts.back(), Some(&receipt));
    assert_eq!(backend.requests().expect("requests").len(), 2);
    assert_eq!(state.connection, ConnectionState::Live);
}

#[tokio::test]
async fn reconnect_reconciles_unknown_receipt_before_controls_reenable() {
    let mut state = session_state();
    let target = state.overview.sessions[0].session.clone();
    let pending = RequestComposer {
        action: PlatformAction::FollowUp,
        target: target.resource.clone(),
        expected_revision: target.freshness.revision,
        target_label: coordinate_key(&target.resource),
        text: String::from("reconcile me"),
    }
    .into_pending()
    .expect("pending follow-up");
    let receipt = ActionReceipt {
        id: ReceiptId::new("receipt-after-reconnect").expect("receipt"),
        action: PlatformAction::FollowUp,
        target: target.resource,
        outcome: ReceiptOutcome::Accepted,
        revision: Revision::FIRST,
        recorded_at: EpochMillis::from_millis(3),
        explanation: None,
    };
    let backend = FakeBackend::new(
        BackendMode::Managed,
        [
            Err(BackendError::Unavailable),
            Err(BackendError::Unavailable),
            Ok(PlatformResponse::Capabilities(Capabilities::platform_v1())),
            Ok(PlatformResponse::Snapshot(
                Snapshot::new(Vec::new(), cursor(1)).expect("snapshot"),
            )),
            Ok(PlatformResponse::Sessions(
                SessionList::new(vec![session()], cursor(1)).expect("sessions"),
            )),
            Ok(PlatformResponse::Receipt(receipt.clone())),
        ],
    );

    execute_confirmed(&backend, &mut state, pending).await;
    assert_eq!(state.connection, ConnectionState::Stale);
    assert_eq!(state.pending_receipts.len(), 1);
    refresh(&backend, &mut state)
        .await
        .expect("reconcile refresh");
    assert_eq!(state.connection, ConnectionState::Live);
    assert!(state.pending_receipts.is_empty());
    assert_eq!(state.receipts.back(), Some(&receipt));
}

#[tokio::test]
async fn target_revision_change_invalidates_composer_confirmation_without_a_request() {
    let mut state = session_state();
    let target = state.overview.sessions[0].session.clone();
    let pending = RequestComposer {
        action: PlatformAction::FollowUp,
        target: target.resource.clone(),
        expected_revision: target.freshness.revision,
        target_label: coordinate_key(&target.resource),
        text: String::from("continue"),
    }
    .into_pending()
    .expect("pending follow-up");
    state.overview.sessions[0].session.freshness.revision = Revision::new(2).expect("revision");
    let backend = FakeBackend::new(BackendMode::Managed, []);
    execute_confirmed(&backend, &mut state, pending).await;
    assert!(backend.requests().expect("requests").is_empty());
    assert!(state.status.contains("invalidated"));
}
