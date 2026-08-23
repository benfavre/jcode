mod lifecycle;
mod render;
mod state;
mod workspace;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use self::render::{ComposerView, ConfirmationView, PaletteView};
use self::state::{ApplyEvents, CockpitState, ConnectionState, View, coordinate_key};
use self::workspace::{restore_workspace, save_workspace};
use super::terminal::{init_tui_runtime, spawn_session_signal_watchers};
use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use jcode_operator_backend::platform_contract::{
    ClientId, ExecuteRequest, GetReceiptRequest, IdempotencyKey, PlatformAction, PlatformMethod,
    PlatformText, ResourceAuthority, ResourceCoordinate, ResourceKind, ResourceRecord,
};
use jcode_operator_backend::{AutomoniqueBackend, BackendError, OperatorBackend};
use lifecycle::{spawn_suspend_watcher, suspend_process};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SUBSCRIBE_INTERVAL: Duration = Duration::from_millis(250);
const MIN_RECONNECT_BACKOFF: Duration = Duration::from_millis(250);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

pub async fn run_platform_command(socket: Option<&str>, json: bool) -> Result<()> {
    let socket = platform_socket(socket)?;
    let backend = AutomoniqueBackend::local(&socket);
    let overview = backend
        .overview(ResourceAuthority::Automonique)
        .await
        .with_context(|| format!("platform endpoint unavailable at {}", socket.display()))?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&render::overview_json(&overview))?
        );
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("platform cockpit needs a terminal; use --json for a one-shot snapshot");
    }

    let (mut terminal, guard) = init_tui_runtime()?;
    let mut guard = Some(guard);
    spawn_session_signal_watchers();
    let mut suspend_requests = spawn_suspend_watcher();
    let client = ClientId::new(format!("jcode-platform-{}", std::process::id()))
        .map_err(|_| anyhow::anyhow!("could not construct platform client identity"))?;
    let mut state = CockpitState::new(overview);
    if restore_workspace(&backend, &client, &mut state)
        .await
        .is_err()
    {
        state.status = String::from("saved pane layout was ignored because it was invalid");
    }
    let mut palette: Option<PaletteState> = None;
    let mut confirmation: Option<PendingAction> = None;
    let mut composer: Option<RequestComposer> = None;
    let mut last_refresh = Instant::now();
    let mut last_subscribe = Instant::now();
    let mut retry_at = Instant::now();
    let mut backoff = MIN_RECONNECT_BACKOFF;

    loop {
        let confirmation_view = confirmation.as_ref().map(PendingAction::view);
        let palette_names = palette.as_ref().map(|value| {
            value
                .commands
                .iter()
                .map(|command| command.label())
                .collect::<Vec<_>>()
        });
        let palette_view = palette
            .as_ref()
            .zip(palette_names.as_ref())
            .map(|(value, names)| PaletteView {
                commands: names.as_slice(),
                selected: value.selected,
            });
        let composer_view = composer.as_ref().map(RequestComposer::view);
        terminal.draw(|frame| {
            render::render(
                frame,
                &state,
                confirmation_view,
                palette_view,
                composer_view,
            )
        })?;
        state.expire_controls(now_millis());

        let mut suspend = suspend_requests.try_recv().is_ok();
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            if key.code == KeyCode::Char('z') && key.modifiers.contains(KeyModifiers::CONTROL) {
                suspend = true;
            } else if handle_key(
                key,
                &backend,
                &client,
                &mut state,
                &mut palette,
                &mut confirmation,
                &mut composer,
            )
            .await?
            {
                break;
            }
        }
        if suspend {
            if let Some(active_guard) = guard.take() {
                active_guard.finish(true);
            }
            suspend_process()?;
            let resumed = init_tui_runtime()?;
            terminal = resumed.0;
            guard = Some(resumed.1);
            state.status = String::from("terminal resumed; authoritative resync required");
            state.connection = ConnectionState::Reconnecting;
            retry_at = Instant::now();
            continue;
        }

        if state.connection.mutations_allowed() && last_subscribe.elapsed() >= SUBSCRIBE_INTERVAL {
            match backend.subscribe(Some(state.resource_cursor.clone())).await {
                Ok(subscription) => {
                    if state.apply_subscription(subscription) == ApplyEvents::ResyncRequired {
                        state.connection = ConnectionState::Reconnecting;
                        retry_at = Instant::now();
                    }
                }
                Err(_) => {
                    state.connection = ConnectionState::Reconnecting;
                    state.status =
                        String::from("platform disconnected; retaining read-only snapshot");
                    retry_at = Instant::now() + backoff;
                    backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                }
            }
            last_subscribe = Instant::now();
        }

        if state.connection != ConnectionState::Live && Instant::now() >= retry_at {
            match refresh(&backend, &mut state).await {
                Ok(()) => {
                    for pane in &mut state.panes {
                        match backend
                            .attach(pane.record.session.resource.clone(), client.clone())
                            .await
                        {
                            Ok(attachment) => pane.attachment = attachment,
                            Err(_) => pane.record.attachable = false,
                        }
                    }
                    backoff = MIN_RECONNECT_BACKOFF;
                    last_refresh = Instant::now();
                    last_subscribe = Instant::now();
                }
                Err(_) => {
                    state.connection = ConnectionState::Reconnecting;
                    retry_at = Instant::now() + backoff;
                    backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                }
            }
        } else if state.connection == ConnectionState::Live
            && last_refresh.elapsed() >= REFRESH_INTERVAL
        {
            if refresh(&backend, &mut state).await.is_err() {
                state.connection = ConnectionState::Reconnecting;
                state.status = String::from("snapshot refresh failed; retaining read-only state");
                retry_at = Instant::now() + backoff;
            }
            last_refresh = Instant::now();
        }
    }

    let workspace_saved = save_workspace(&state).is_ok();
    cleanup_client(&backend, &client, &mut state).await;
    if let Some(active_guard) = guard.take() {
        active_guard.finish(true);
    }
    if !workspace_saved {
        eprintln!("jcode platform: local pane layout could not be saved");
    }
    Ok(())
}

async fn handle_key(
    key: KeyEvent,
    backend: &impl OperatorBackend,
    client: &ClientId,
    state: &mut CockpitState,
    palette: &mut Option<PaletteState>,
    confirmation: &mut Option<PendingAction>,
    composer: &mut Option<RequestComposer>,
) -> Result<bool> {
    if let Some(open) = composer.as_mut() {
        match key.code {
            KeyCode::Esc => {
                *composer = None;
                state.status = String::from("composition cancelled before submission");
            }
            KeyCode::Enter => {
                let Some(composed) = composer.take() else {
                    return Ok(false);
                };
                if let Some(pending) = composed.into_pending() {
                    *confirmation = Some(pending);
                } else {
                    state.status = String::from("request text is required");
                }
            }
            KeyCode::Backspace => {
                open.text.pop();
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                let mut candidate = open.text.clone();
                candidate.push(character);
                if PlatformText::new(candidate.clone()).is_ok() {
                    open.text = candidate;
                } else {
                    state.status = String::from("request text reached the platform bound");
                }
            }
            _ => {}
        }
        return Ok(false);
    }
    if confirmation.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(pending) = confirmation.take() {
                    execute_confirmed(backend, state, pending).await;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                *confirmation = None;
                state.status = String::from("mutation cancelled before submission");
            }
            _ => {}
        }
        return Ok(false);
    }
    if let Some(open) = palette.as_mut() {
        match key.code {
            KeyCode::Up => open.selected = open.selected.saturating_sub(1),
            KeyCode::Down => {
                open.selected = (open.selected + 1).min(open.commands.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(command) = open.commands.get(open.selected).copied() {
                    *palette = None;
                    apply_command(command, backend, client, state, confirmation, composer).await;
                }
            }
            KeyCode::Esc | KeyCode::Char('p') => *palette = None,
            _ => {}
        }
        return Ok(false);
    }
    if state.show_help {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
            state.show_help = false;
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => state.show_help = true,
        KeyCode::Char('h') => state.high_contrast = !state.high_contrast,
        KeyCode::Char('i') => open_composer_for_selection(state, composer),
        KeyCode::Char('p') => {
            let commands = available_commands(state);
            if commands.is_empty() {
                state.status = String::from("no authorized action for the selected target");
            } else {
                *palette = Some(PaletteState {
                    commands,
                    selected: 0,
                });
            }
        }
        KeyCode::Char('r') => {
            state.connection = ConnectionState::Reconnecting;
            state.status = String::from("manual authoritative resync requested");
        }
        KeyCode::Char('l') => state.pane_layout = state.pane_layout.next(),
        KeyCode::Char('P') => state.toggle_focused_pin(),
        KeyCode::Char('a') => {
            apply_command(
                AvailableCommand::Attach,
                backend,
                client,
                state,
                confirmation,
                composer,
            )
            .await;
        }
        KeyCode::Char('d') => {
            apply_command(
                AvailableCommand::Detach,
                backend,
                client,
                state,
                confirmation,
                composer,
            )
            .await;
        }
        KeyCode::Char('c') => {
            apply_command(
                AvailableCommand::ClaimControl,
                backend,
                client,
                state,
                confirmation,
                composer,
            )
            .await;
        }
        KeyCode::Char('x') => {
            apply_command(
                AvailableCommand::ReleaseControl,
                backend,
                client,
                state,
                confirmation,
                composer,
            )
            .await;
        }
        KeyCode::Up => state.move_selection(-1),
        KeyCode::Down => state.move_selection(1),
        KeyCode::Tab | KeyCode::Char(']') => state.focus_next_pane(1),
        KeyCode::BackTab | KeyCode::Char('[') => state.focus_next_pane(-1),
        KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.reorder_focused_pane(-1);
        }
        KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.reorder_focused_pane(1);
        }
        KeyCode::Char(value @ '1'..='7') => {
            let index = usize::from(value as u8 - b'1');
            state.set_view(View::ALL[index]);
        }
        _ => {}
    }
    Ok(false)
}

async fn refresh(backend: &impl OperatorBackend, state: &mut CockpitState) -> Result<()> {
    let reconnected = state.connection != ConnectionState::Live;
    let overview = backend.overview(ResourceAuthority::Automonique).await?;
    if overview.capabilities.protocol != "automonique.platform"
        || overview.capabilities.schema != "automonique.platform/v1"
    {
        state.connection = ConnectionState::UpgradeRequired;
        state.status = String::from("server protocol is incompatible; mutation controls disabled");
        bail!("platform protocol upgrade required");
    }
    let pending = state.pending_receipts.iter().cloned().collect::<Vec<_>>();
    for key in pending {
        match backend
            .receipt(GetReceiptRequest::by_idempotency_key(key.clone()))
            .await
        {
            Ok(receipt) => {
                state.resolve_pending_receipt(&key);
                state.push_receipt(receipt);
            }
            Err(error) => {
                state.connection = ConnectionState::Stale;
                state.status = String::from("unknown receipt; controls remain disabled");
                return Err(anyhow::anyhow!(error));
            }
        }
    }
    state.replace_overview(overview, reconnected);
    Ok(())
}

async fn apply_command(
    command: AvailableCommand,
    backend: &impl OperatorBackend,
    client: &ClientId,
    state: &mut CockpitState,
    confirmation: &mut Option<PendingAction>,
    composer: &mut Option<RequestComposer>,
) {
    match command {
        AvailableCommand::Attach => {
            if !state.connection.mutations_allowed() {
                state.status = String::from("attach disabled while snapshot is stale");
                return;
            }
            let Some(session) = state.selected_session().cloned() else {
                state.status = String::from("select an attachable session first");
                return;
            };
            if !session.attachable {
                state.status = String::from("selected session is read-only and not attachable");
                return;
            }
            match backend
                .attach(session.session.resource.clone(), client.clone())
                .await
            {
                Ok(attachment) => {
                    let id = session.session.resource.id.as_str().to_owned();
                    state.attach(session, attachment);
                    state.status = format!("attached to {id} as observer");
                }
                Err(error) => state.status = format!("attach refused: {error}"),
            }
        }
        AvailableCommand::Detach => {
            let Some(pane) = state.panes.get(state.focused_pane).cloned() else {
                state.status = String::from("no attached pane to detach");
                return;
            };
            if let Some(lease) = pane.control.clone() {
                let Ok(key) = interaction_key("release") else {
                    state.status = String::from("could not construct control release identity");
                    return;
                };
                if let Err(error) = backend.release_control(lease, key).await {
                    state.status = format!("control release refused; observer retained: {error}");
                    return;
                }
            }
            let id = pane.record.session.resource.id.as_str().to_owned();
            match backend
                .detach(pane.record.session.resource.clone(), client.clone())
                .await
            {
                Ok(()) => {
                    state.remove_focused_pane();
                    state.status = format!("detached observer from {id}; runner unchanged");
                }
                Err(error) => {
                    state.status = format!("detach refused; observer pane retained: {error}")
                }
            }
        }
        AvailableCommand::ClaimControl => {
            if !state.connection.mutations_allowed() {
                state.status = String::from("control disabled while snapshot is stale");
                return;
            }
            let target = state
                .panes
                .get(state.focused_pane)
                .map(|pane| pane.record.clone())
                .or_else(|| state.selected_session().cloned());
            let Some(session) = target else {
                state.status = String::from("select or attach a controllable session first");
                return;
            };
            if !session.controllable {
                state.status = String::from("server marks the selected session non-controllable");
                return;
            }
            let Ok(key) = interaction_key("claim") else {
                state.status = String::from("could not construct control claim identity");
                return;
            };
            match backend
                .claim_control(session.session.resource.clone(), client.clone(), key)
                .await
            {
                Ok(lease) => {
                    if let Some(pane) = state
                        .panes
                        .iter_mut()
                        .find(|pane| pane.record.session.resource == session.session.resource)
                    {
                        pane.control = Some(lease);
                    }
                    state.status = String::from("exclusive control lease claimed");
                }
                Err(error) => state.status = format!("control refused or held elsewhere: {error}"),
            }
        }
        AvailableCommand::ReleaseControl => {
            let Some(pane) = state.panes.get_mut(state.focused_pane) else {
                state.status = String::from("no focused session pane");
                return;
            };
            let Some(lease) = pane.control.take() else {
                state.status = String::from("focused pane holds no control lease");
                return;
            };
            let Ok(key) = interaction_key("release") else {
                pane.control = Some(lease);
                state.status = String::from("could not construct control release identity");
                return;
            };
            match backend.release_control(lease.clone(), key).await {
                Ok(()) => state.status = String::from("control lease released; observer retained"),
                Err(error) => {
                    pane.control = Some(lease);
                    state.status = format!("control release refused: {error}");
                }
            }
        }
        AvailableCommand::StartRun
        | AvailableCommand::StopRun
        | AvailableCommand::GrantApproval
        | AvailableCommand::DenyApproval => {
            if !state.connection.mutations_allowed() {
                state.status = String::from("mutation disabled while snapshot is stale");
                return;
            }
            let Some(target) = state.selected_resource().cloned() else {
                state.status = String::from("select an exact target first");
                return;
            };
            *confirmation = PendingAction::for_command(command, target);
        }
        AvailableCommand::SubmitRequest | AvailableCommand::FollowUp => {
            open_composer(command, state, composer);
        }
        AvailableCommand::CycleLayout => state.pane_layout = state.pane_layout.next(),
        AvailableCommand::Refresh => {
            state.connection = ConnectionState::Reconnecting;
            state.status = String::from("authoritative resync requested");
        }
    }
}

async fn execute_confirmed(
    backend: &impl OperatorBackend,
    state: &mut CockpitState,
    pending: PendingAction,
) {
    if !state.connection.mutations_allowed() {
        state.status = String::from("confirmation invalidated because the client is stale");
        return;
    }
    let current_revision = state
        .overview
        .resources
        .iter()
        .find(|record| record.resource == pending.target)
        .map(|record| record.freshness.revision)
        .or_else(|| {
            state
                .overview
                .sessions
                .iter()
                .find(|record| record.session.resource == pending.target)
                .map(|record| record.session.freshness.revision)
        });
    if current_revision != Some(pending.expected_revision) {
        state.status = String::from("confirmation invalidated by a target revision change");
        return;
    }
    let idempotency_key = pending.idempotency_key.clone();
    let request = match ExecuteRequest::new(
        pending.action,
        pending.target,
        idempotency_key.clone(),
        Some(pending.expected_revision),
        pending.parameter,
    ) {
        Ok(request) => request,
        Err(_) => {
            state.status = String::from("typed mutation failed local validation");
            return;
        }
    };
    match backend.execute(request).await {
        Ok(receipt) => state.push_receipt(receipt),
        Err(error @ BackendError::Refused { .. }) => {
            state.status = format!("mutation refused without effect: {error}");
        }
        Err(_) => match backend
            .receipt(GetReceiptRequest::by_idempotency_key(
                idempotency_key.clone(),
            ))
            .await
        {
            Ok(receipt) => state.push_receipt(receipt),
            Err(_) => {
                state.track_pending_receipt(idempotency_key);
                state.connection = ConnectionState::Stale;
                state.status = String::from(
                    "submission outcome unknown; controls disabled until receipt reconciliation",
                );
            }
        },
    }
}

async fn cleanup_client(
    backend: &impl OperatorBackend,
    client: &ClientId,
    state: &mut CockpitState,
) {
    for pane in state.panes.drain(..) {
        if let Some(lease) = pane.control
            && let Ok(key) = interaction_key("exit-release")
            && let Err(error) = backend.release_control(lease, key).await
        {
            eprintln!("jcode platform: control release during exit failed: {error}");
        }
        if let Err(error) = backend
            .detach(pane.record.session.resource, client.clone())
            .await
        {
            eprintln!("jcode platform: observer detach during exit failed: {error}");
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AvailableCommand {
    Attach,
    Detach,
    ClaimControl,
    ReleaseControl,
    StartRun,
    StopRun,
    GrantApproval,
    DenyApproval,
    SubmitRequest,
    FollowUp,
    CycleLayout,
    Refresh,
}

impl AvailableCommand {
    const fn label(self) -> &'static str {
        match self {
            Self::Attach => "Attach selected session as observer",
            Self::Detach => "Detach focused observer pane",
            Self::ClaimControl => "Claim exclusive short control lease",
            Self::ReleaseControl => "Release focused control lease",
            Self::StartRun => "Start selected ready run",
            Self::StopRun => "Cancel selected active run",
            Self::GrantApproval => "Grant selected approval revision",
            Self::DenyApproval => "Deny selected approval revision",
            Self::SubmitRequest => "Compose new durable request",
            Self::FollowUp => "Compose follow-up for selected session",
            Self::CycleLayout => "Cycle pane layout",
            Self::Refresh => "Reconcile from authoritative snapshot",
        }
    }
}

struct PaletteState {
    commands: Vec<AvailableCommand>,
    selected: usize,
}

fn available_commands(state: &CockpitState) -> Vec<AvailableCommand> {
    let mut commands = vec![AvailableCommand::Refresh];
    let supports = |method: PlatformMethod| state.overview.capabilities.methods.contains(&method);
    let supports_action = |action: PlatformAction| {
        supports(PlatformMethod::Execute) && state.overview.actions.contains(&action)
    };
    if let Some(record) = state.selected_resource() {
        if record.freshness.state.as_str() != "fresh" {
            return commands;
        }
        match record.resource.kind {
            ResourceKind::Session => {
                if supports(PlatformMethod::Attach) {
                    commands.push(AvailableCommand::Attach);
                }
                if supports(PlatformMethod::ClaimControl) {
                    commands.push(AvailableCommand::ClaimControl);
                }
                if supports_action(PlatformAction::FollowUp) {
                    commands.push(AvailableCommand::FollowUp);
                }
            }
            ResourceKind::Run => {
                if supports_action(PlatformAction::StartRun) {
                    commands.push(AvailableCommand::StartRun);
                }
                if supports_action(PlatformAction::StopRun) {
                    commands.push(AvailableCommand::StopRun);
                }
            }
            ResourceKind::Approval if supports_action(PlatformAction::DecideApproval) => {
                commands.extend([
                    AvailableCommand::GrantApproval,
                    AvailableCommand::DenyApproval,
                ]);
            }
            ResourceKind::Node if supports_action(PlatformAction::SubmitRequest) => {
                commands.push(AvailableCommand::SubmitRequest);
            }
            _ => {}
        }
    }
    if !state.panes.is_empty() {
        if supports(PlatformMethod::Detach) {
            commands.push(AvailableCommand::Detach);
        }
        if supports(PlatformMethod::ReleaseControl)
            && state
                .panes
                .get(state.focused_pane)
                .is_some_and(|pane| pane.control.is_some())
        {
            commands.push(AvailableCommand::ReleaseControl);
        }
        commands.push(AvailableCommand::CycleLayout);
    }
    commands
}

struct RequestComposer {
    action: PlatformAction,
    target: ResourceRecord,
    target_label: String,
    text: String,
}

impl RequestComposer {
    fn view(&self) -> ComposerView<'_> {
        ComposerView {
            operation: self.action.as_str(),
            target: &self.target_label,
            text: &self.text,
        }
    }

    fn into_pending(self) -> Option<PendingAction> {
        let parameter = match PlatformText::new(self.text) {
            Ok(parameter) => parameter,
            Err(_) => return None,
        };
        let (title, consequence) = match self.action {
            PlatformAction::SubmitRequest => (
                "Submit durable request",
                "The text enters node intake once and is reconciled by idempotency key.",
            ),
            PlatformAction::FollowUp => (
                "Submit session follow-up",
                "The text is durably queued against this exact provider session revision.",
            ),
            _ => return None,
        };
        Some(PendingAction {
            action: self.action,
            target: self.target.resource,
            expected_revision: self.target.freshness.revision,
            parameter: Some(parameter),
            idempotency_key: match interaction_key(self.action.as_str()) {
                Ok(key) => key,
                Err(_) => return None,
            },
            title,
            consequence,
            target_label: self.target_label,
        })
    }
}

fn open_composer_for_selection(state: &mut CockpitState, composer: &mut Option<RequestComposer>) {
    let action = match state.selected_resource().map(|record| record.resource.kind) {
        Some(ResourceKind::Session) => AvailableCommand::FollowUp,
        Some(ResourceKind::Node) => AvailableCommand::SubmitRequest,
        _ => {
            state.status = String::from("select a node or session before composing");
            return;
        }
    };
    open_composer(action, state, composer);
}

fn open_composer(
    command: AvailableCommand,
    state: &mut CockpitState,
    composer: &mut Option<RequestComposer>,
) {
    if !state.connection.mutations_allowed() {
        state.status = String::from("composition disabled while snapshot is stale");
        return;
    }
    let Some(target) = state.selected_resource().cloned() else {
        state.status = String::from("select an exact target before composing");
        return;
    };
    if target.freshness.state.as_str() != "fresh" {
        state.status = String::from("selected target is not fresh; composition disabled");
        return;
    }
    let action = match command {
        AvailableCommand::SubmitRequest
            if target.resource.kind == ResourceKind::Node
                && state
                    .overview
                    .actions
                    .contains(&PlatformAction::SubmitRequest) =>
        {
            PlatformAction::SubmitRequest
        }
        AvailableCommand::FollowUp
            if target.resource.kind == ResourceKind::Session
                && state.overview.actions.contains(&PlatformAction::FollowUp) =>
        {
            PlatformAction::FollowUp
        }
        _ => {
            state.status = String::from("server does not advertise this composition action");
            return;
        }
    };
    let target_label = coordinate_key(&target.resource);
    *composer = Some(RequestComposer {
        action,
        target,
        target_label,
        text: String::new(),
    });
}

struct PendingAction {
    action: PlatformAction,
    target: ResourceCoordinate,
    expected_revision: jcode_operator_backend::platform_primitives::Revision,
    parameter: Option<PlatformText>,
    idempotency_key: IdempotencyKey,
    title: &'static str,
    consequence: &'static str,
    target_label: String,
}

impl PendingAction {
    fn for_command(command: AvailableCommand, target: ResourceRecord) -> Option<Self> {
        let (action, parameter, title, consequence) = match command {
            AvailableCommand::StartRun if target.resource.kind == ResourceKind::Run => (
                PlatformAction::StartRun,
                None,
                "Start run",
                "The exact ready run is admitted once; retries reconcile by idempotency key.",
            ),
            AvailableCommand::StopRun if target.resource.kind == ResourceKind::Run => (
                PlatformAction::StopRun,
                None,
                "Cancel run",
                "Cancellation targets the exact run; terminal state is reconciled separately.",
            ),
            AvailableCommand::GrantApproval if target.resource.kind == ResourceKind::Approval => (
                PlatformAction::DecideApproval,
                Some(match PlatformText::new("grant") {
                    Ok(parameter) => parameter,
                    Err(_) => return None,
                }),
                "Grant approval",
                "Only this approval revision is granted; provider permission remains separate.",
            ),
            AvailableCommand::DenyApproval if target.resource.kind == ResourceKind::Approval => (
                PlatformAction::DecideApproval,
                Some(match PlatformText::new("deny") {
                    Ok(parameter) => parameter,
                    Err(_) => return None,
                }),
                "Deny approval",
                "Only this approval revision is denied and the decision is durably audited.",
            ),
            _ => return None,
        };
        let target_label = coordinate_key(&target.resource);
        Some(Self {
            action,
            target: target.resource,
            expected_revision: target.freshness.revision,
            parameter,
            idempotency_key: match interaction_key(action.as_str()) {
                Ok(key) => key,
                Err(_) => return None,
            },
            title,
            consequence,
            target_label,
        })
    }

    fn view(&self) -> ConfirmationView<'_> {
        ConfirmationView {
            title: self.title,
            target: &self.target_label,
            revision: self.expected_revision.get(),
            consequence: self.consequence,
        }
    }
}

fn interaction_key(operation: &str) -> Result<IdempotencyKey, &'static str> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    IdempotencyKey::new(format!(
        "jcode-platform-{operation}-{}-{nanos}",
        std::process::id()
    ))
    .map_err(|_| "idempotency_key_invalid")
}

fn now_millis() -> i64 {
    let duration = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration,
        Err(_) => return i64::MAX,
    };
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn platform_socket(explicit: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = explicit.map(str::trim).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) =
        std::env::var_os("AUTOMONIQUE_PLATFORM_SOCKET").filter(|path| !path.is_empty())
    {
        return Ok(PathBuf::from(path));
    }
    dirs::runtime_dir()
        .map(|runtime| runtime.join("automonique/admin.sock"))
        .context("no runtime directory; pass --socket or set AUTOMONIQUE_PLATFORM_SOCKET")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_operator_backend::platform_contract::{
        ActionReceipt, Attachment, Capabilities, ControlLease, CursorTopic, Freshness,
        FreshnessState, PlatformCursor, PlatformResponse, PlatformText, ReceiptId, ReceiptOutcome,
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
    async fn composed_follow_up_reconciles_an_ambiguous_submission_by_key() {
        let mut state = session_state();
        let target = state.overview.sessions[0].session.clone();
        let pending = RequestComposer {
            action: PlatformAction::FollowUp,
            target: target.clone(),
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
            target: target.clone(),
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
            target: target.clone(),
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
}
