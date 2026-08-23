use std::collections::{BTreeSet, VecDeque};

use jcode_operator_backend::OperatorOverview;
use jcode_operator_backend::platform_contract::{
    ActionReceipt, Attachment, ControlLease, IdempotencyKey, PlatformCursor, PlatformEvent,
    ResourceAuthority, ResourceCoordinate, ResourceKind, ResourceRecord, SessionRecord,
    Subscription,
};

pub const MAX_PANES: usize = 12;
pub const MAX_TIMELINE_EVENTS: usize = 256;
pub const MAX_RECEIPTS: usize = 64;
pub const MAX_PENDING_RECEIPTS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum View {
    Overview,
    Runs,
    Sessions,
    Approvals,
    Models,
    Failures,
    Receipts,
}

impl View {
    pub const ALL: [Self; 7] = [
        Self::Overview,
        Self::Runs,
        Self::Sessions,
        Self::Approvals,
        Self::Models,
        Self::Failures,
        Self::Receipts,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Runs => "runs",
            Self::Sessions => "sessions",
            Self::Approvals => "approvals",
            Self::Models => "models",
            Self::Failures => "failures",
            Self::Receipts => "receipts",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneLayout {
    Grid,
    Rows,
    Columns,
    Tabs,
    Focused,
}

impl PaneLayout {
    pub const fn next(self) -> Self {
        match self {
            Self::Grid => Self::Rows,
            Self::Rows => Self::Columns,
            Self::Columns => Self::Tabs,
            Self::Tabs => Self::Focused,
            Self::Focused => Self::Grid,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::Rows => "rows",
            Self::Columns => "columns",
            Self::Tabs => "tabs",
            Self::Focused => "focused",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "grid" => Some(Self::Grid),
            "rows" => Some(Self::Rows),
            "columns" => Some(Self::Columns),
            "tabs" => Some(Self::Tabs),
            "focused" => Some(Self::Focused),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Live,
    Stale,
    Reconnecting,
    UpgradeRequired,
}

impl ConnectionState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "LIVE",
            Self::Stale => "STALE / READ-ONLY",
            Self::Reconnecting => "RECONNECTING / READ-ONLY",
            Self::UpgradeRequired => "UPGRADE REQUIRED / READ-ONLY",
        }
    }

    pub const fn mutations_allowed(self) -> bool {
        matches!(self, Self::Live)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyEvents {
    Applied,
    DuplicateOnly,
    ResyncRequired,
}

#[derive(Clone, Debug)]
pub struct SessionPane {
    pub record: SessionRecord,
    pub attachment: Attachment,
    pub control: Option<ControlLease>,
    pub timeline: VecDeque<ResourceRecord>,
    pub unread: usize,
    pub pinned: bool,
}

impl SessionPane {
    pub fn key(&self) -> String {
        coordinate_key(&self.record.session.resource)
    }
}

#[derive(Clone, Debug)]
pub struct CockpitState {
    pub overview: OperatorOverview,
    pub view: View,
    pub connection: ConnectionState,
    pub status: String,
    pub selected_key: Option<String>,
    pub selected_index: usize,
    pub panes: Vec<SessionPane>,
    pub focused_pane: usize,
    pub pane_layout: PaneLayout,
    pub receipts: VecDeque<ActionReceipt>,
    pub pending_receipts: VecDeque<IdempotencyKey>,
    pub resource_cursor: PlatformCursor,
    pub show_help: bool,
    pub high_contrast: bool,
}

impl CockpitState {
    pub fn new(overview: OperatorOverview) -> Self {
        let resource_cursor = overview.cursor.clone();
        let mut state = Self {
            overview,
            view: View::Overview,
            connection: ConnectionState::Live,
            status: String::from("connected"),
            selected_key: None,
            selected_index: 0,
            panes: Vec::new(),
            focused_pane: 0,
            pane_layout: PaneLayout::Grid,
            receipts: VecDeque::new(),
            pending_receipts: VecDeque::new(),
            resource_cursor,
            show_help: false,
            high_contrast: false,
        };
        state.stabilize_selection();
        state
    }

    pub fn replace_overview(&mut self, overview: OperatorOverview, reconnected: bool) {
        self.resource_cursor = overview.cursor.clone();
        self.overview = overview;
        for pane in &mut self.panes {
            if let Some(record) = self
                .overview
                .sessions
                .iter()
                .find(|candidate| candidate.session.resource == pane.record.session.resource)
            {
                pane.record = record.clone();
            }
            // A reconnect never proves that an old exclusive lease survived.
            // Ordinary snapshot refreshes retain an unexpired local lease.
            if reconnected {
                pane.control = None;
            }
        }
        self.connection = ConnectionState::Live;
        self.status = String::from("reconciled with authoritative snapshot");
        self.stabilize_selection();
    }

    pub fn expire_controls(&mut self, now_ms: i64) {
        for pane in &mut self.panes {
            if pane
                .control
                .as_ref()
                .is_some_and(|lease| lease.expires_at.as_millis() <= now_ms)
            {
                pane.control = None;
            }
        }
    }

    pub fn track_pending_receipt(&mut self, key: IdempotencyKey) {
        if self.pending_receipts.contains(&key) {
            return;
        }
        if self.pending_receipts.len() == MAX_PENDING_RECEIPTS {
            self.pending_receipts.pop_front();
        }
        self.pending_receipts.push_back(key);
    }

    pub fn resolve_pending_receipt(&mut self, key: &IdempotencyKey) {
        self.pending_receipts.retain(|candidate| candidate != key);
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
        self.selected_index = 0;
        self.selected_key = None;
        self.stabilize_selection();
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.visible_resources().len();
        if len == 0 {
            self.selected_index = 0;
            self.selected_key = None;
            return;
        }
        self.selected_index = self
            .selected_index
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
        self.selected_key = self
            .visible_resources()
            .get(self.selected_index)
            .map(|record| coordinate_key(&record.resource));
    }

    pub fn stabilize_selection(&mut self) {
        let keys = self
            .visible_resources()
            .iter()
            .map(|record| coordinate_key(&record.resource))
            .collect::<Vec<_>>();
        if keys.is_empty() {
            self.selected_index = 0;
            self.selected_key = None;
            return;
        }
        if let Some(key) = self.selected_key.as_deref()
            && let Some(index) = keys.iter().position(|candidate| candidate == key)
        {
            self.selected_index = index;
            return;
        }
        self.selected_index = self.selected_index.min(keys.len() - 1);
        self.selected_key = Some(keys[self.selected_index].clone());
    }

    pub fn selected_resource(&self) -> Option<&ResourceRecord> {
        self.visible_resources().get(self.selected_index).copied()
    }

    pub fn selected_session(&self) -> Option<&SessionRecord> {
        let selected = self.selected_resource()?;
        self.overview
            .sessions
            .iter()
            .find(|session| session.session.resource == selected.resource)
    }

    pub fn visible_resources(&self) -> Vec<&ResourceRecord> {
        let mut seen = BTreeSet::new();
        self.overview
            .resources
            .iter()
            .chain(
                self.overview
                    .sessions
                    .iter()
                    .map(|session| &session.session),
            )
            .filter(|record| {
                !(record.resource.authority == ResourceAuthority::Automonique
                    && record.resource.kind == ResourceKind::Client
                    && record.resource.id.as_str().starts_with("platform-action-"))
            })
            .filter(|record| seen.insert(coordinate_key(&record.resource)))
            .filter(|record| match self.view {
                View::Overview => {
                    record.resource.kind != ResourceKind::Node
                        || record.freshness.state.as_str() == "fresh"
                }
                View::Runs => record.resource.kind == ResourceKind::Run,
                View::Sessions => record.resource.kind == ResourceKind::Session,
                View::Approvals => record.resource.kind == ResourceKind::Approval,
                View::Models => record.resource.kind == ResourceKind::Model,
                View::Failures => is_failure(record),
                View::Receipts => record.resource.kind == ResourceKind::Receipt,
            })
            .collect()
    }

    pub fn toggle_focused_pin(&mut self) {
        let Some(pane) = self.panes.get_mut(self.focused_pane) else {
            self.status = String::from("no focused session pane");
            return;
        };
        pane.pinned = !pane.pinned;
        self.status = if pane.pinned {
            String::from("focused pane pinned in saved workspace")
        } else {
            String::from("focused pane unpinned")
        };
    }

    pub fn attach(&mut self, record: SessionRecord, attachment: Attachment) -> bool {
        let key = coordinate_key(&record.session.resource);
        if let Some(index) = self.panes.iter().position(|pane| pane.key() == key) {
            self.focused_pane = index;
            return false;
        }
        if self.panes.len() >= MAX_PANES {
            self.status = format!("pane limit reached ({MAX_PANES})");
            return false;
        }
        self.panes.push(SessionPane {
            record,
            attachment,
            control: None,
            timeline: VecDeque::new(),
            unread: 0,
            pinned: false,
        });
        self.focused_pane = self.panes.len() - 1;
        true
    }

    pub fn remove_focused_pane(&mut self) -> Option<SessionPane> {
        if self.panes.is_empty() {
            return None;
        }
        let pane = self
            .panes
            .remove(self.focused_pane.min(self.panes.len() - 1));
        self.focused_pane = self.focused_pane.min(self.panes.len().saturating_sub(1));
        Some(pane)
    }

    pub fn focus_next_pane(&mut self, delta: isize) {
        if self.panes.is_empty() {
            self.focused_pane = 0;
            return;
        }
        self.focused_pane = self
            .focused_pane
            .saturating_add_signed(delta)
            .min(self.panes.len() - 1);
        if let Some(pane) = self.panes.get_mut(self.focused_pane) {
            pane.unread = 0;
        }
    }

    pub fn reorder_focused_pane(&mut self, delta: isize) {
        if self.panes.len() < 2 {
            return;
        }
        let next = self
            .focused_pane
            .saturating_add_signed(delta)
            .min(self.panes.len() - 1);
        if next != self.focused_pane {
            self.panes.swap(self.focused_pane, next);
            self.focused_pane = next;
        }
    }

    pub fn push_receipt(&mut self, receipt: ActionReceipt) {
        if self.receipts.len() == MAX_RECEIPTS {
            self.receipts.pop_front();
        }
        self.status = format!(
            "{} {} for {}",
            receipt.action.as_str(),
            receipt.outcome.as_str(),
            receipt.target.id.as_str()
        );
        self.receipts.push_back(receipt);
    }

    pub fn apply_subscription(&mut self, subscription: Subscription) -> ApplyEvents {
        let current = self.resource_cursor.sequence.get();
        let mut next = current;
        let mut applied = 0_usize;
        for event in subscription.events {
            let sequence = event.cursor.sequence.get();
            if sequence <= next {
                continue;
            }
            if sequence != next.saturating_add(1) {
                self.connection = ConnectionState::Stale;
                self.status = String::from("event gap detected; requesting fresh snapshot");
                return ApplyEvents::ResyncRequired;
            }
            self.apply_event(event);
            next = sequence;
            applied += 1;
        }
        if subscription.cursor.sequence.get() < next {
            self.connection = ConnectionState::Stale;
            self.status = String::from("subscription cursor moved backwards");
            return ApplyEvents::ResyncRequired;
        }
        self.resource_cursor = subscription.cursor;
        self.connection = ConnectionState::Live;
        self.stabilize_selection();
        if applied == 0 {
            ApplyEvents::DuplicateOnly
        } else {
            ApplyEvents::Applied
        }
    }

    fn apply_event(&mut self, event: PlatformEvent) {
        if let Some(existing) = self
            .overview
            .resources
            .iter_mut()
            .find(|record| record.resource == event.resource.resource)
        {
            if event.resource.freshness.revision >= existing.freshness.revision {
                *existing = event.resource.clone();
            }
        } else {
            self.overview.resources.push(event.resource.clone());
        }
        for (index, pane) in self.panes.iter_mut().enumerate() {
            let belongs = event.resource.resource == pane.record.session.resource
                || pane
                    .record
                    .run
                    .as_ref()
                    .is_some_and(|run| *run == event.resource.resource);
            if belongs {
                if pane.timeline.len() == MAX_TIMELINE_EVENTS {
                    pane.timeline.pop_front();
                }
                pane.timeline.push_back(event.resource.clone());
                if index != self.focused_pane {
                    pane.unread = pane.unread.saturating_add(1);
                }
            }
        }
    }
}

pub fn coordinate_key(coordinate: &ResourceCoordinate) -> String {
    format!(
        "{}/{}/{}",
        coordinate.authority.as_str(),
        coordinate.kind.as_str(),
        coordinate.id.as_str()
    )
}

pub fn is_failure(record: &ResourceRecord) -> bool {
    let summary = record.summary.as_str().to_ascii_lowercase();
    record.freshness.state.as_str() != "fresh"
        || [
            "failed",
            "failure",
            "error",
            "rejected",
            "lost",
            "quarantine",
            "denied",
        ]
        .iter()
        .any(|needle| summary.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_operator_backend::platform_contract::{
        Attachment, Capabilities, ClientId, ControlLease, ControlLeaseId, CursorTopic, Freshness,
        FreshnessState, PlatformText, ResourceAuthority, ResourceId, SessionRecord,
    };
    use jcode_operator_backend::platform_primitives::{EpochMillis, Revision};

    fn cursor(sequence: u64) -> PlatformCursor {
        PlatformCursor {
            authority: ResourceAuthority::Automonique,
            topic: CursorTopic::new("resources").expect("topic"),
            sequence: Revision::new(sequence).expect("sequence"),
        }
    }

    fn record(id: &str, revision: u64, summary: &str) -> ResourceRecord {
        ResourceRecord {
            resource: ResourceCoordinate::new(
                ResourceAuthority::Automonique,
                ResourceKind::Run,
                ResourceId::new(id).expect("id"),
            ),
            freshness: Freshness {
                state: FreshnessState::Fresh,
                observed_at: EpochMillis::from_millis(1),
                revision: Revision::new(revision).expect("revision"),
            },
            summary: PlatformText::new(summary).expect("summary"),
        }
    }

    fn state() -> CockpitState {
        CockpitState::new(OperatorOverview {
            capabilities: Capabilities::platform_v1(),
            actions: jcode_operator_backend::platform_contract::PlatformAction::ALL.to_vec(),
            resources: vec![record("run-a", 1, "running")],
            sessions: Vec::new(),
            cursor: cursor(1),
        })
    }

    fn session_record(index: usize) -> (SessionRecord, Attachment) {
        let session_coordinate = ResourceCoordinate::new(
            ResourceAuthority::Automonique,
            ResourceKind::Session,
            ResourceId::new(format!("session-{index}")).expect("session id"),
        );
        let run = ResourceCoordinate::new(
            ResourceAuthority::Automonique,
            ResourceKind::Run,
            ResourceId::new(format!("run-{index}")).expect("run id"),
        );
        let client = ClientId::new("client-a").expect("client");
        (
            SessionRecord {
                session: ResourceRecord {
                    resource: session_coordinate.clone(),
                    freshness: Freshness {
                        state: FreshnessState::Fresh,
                        observed_at: EpochMillis::from_millis(1),
                        revision: Revision::FIRST,
                    },
                    summary: PlatformText::new("open").expect("summary"),
                },
                run: Some(run),
                attachable: true,
                controllable: true,
            },
            Attachment {
                session: session_coordinate,
                client,
                cursor: cursor(1),
            },
        )
    }

    #[test]
    fn duplicate_events_are_idempotent() {
        let mut state = state();
        let result = state.apply_subscription(
            Subscription::new(
                vec![PlatformEvent {
                    cursor: cursor(1),
                    resource: record("run-a", 1, "running"),
                }],
                cursor(1),
            )
            .expect("subscription"),
        );
        assert_eq!(result, ApplyEvents::DuplicateOnly);
        assert_eq!(state.overview.resources.len(), 1);
    }

    #[test]
    fn gaps_force_resync_without_applying_later_state() {
        let mut state = state();
        let result = state.apply_subscription(
            Subscription::new(
                vec![PlatformEvent {
                    cursor: cursor(3),
                    resource: record("run-a", 2, "completed"),
                }],
                cursor(3),
            )
            .expect("subscription"),
        );
        assert_eq!(result, ApplyEvents::ResyncRequired);
        assert_eq!(state.overview.resources[0].summary.as_str(), "running");
        assert!(!state.connection.mutations_allowed());
    }

    #[test]
    fn selection_survives_snapshot_reordering_by_durable_identity() {
        let mut state = state();
        state.overview.resources.push(record("run-b", 1, "ready"));
        state.set_view(View::Runs);
        state.move_selection(1);
        assert!(
            state
                .selected_key
                .as_deref()
                .is_some_and(|key| key.ends_with("run-b"))
        );

        state.replace_overview(
            OperatorOverview {
                capabilities: Capabilities::platform_v1(),
                actions: jcode_operator_backend::platform_contract::PlatformAction::ALL.to_vec(),
                resources: vec![record("run-b", 2, "running"), record("run-a", 1, "running")],
                sessions: Vec::new(),
                cursor: cursor(2),
            },
            false,
        );
        assert_eq!(state.selected_index, 0);
        assert_eq!(
            state
                .selected_resource()
                .expect("selected")
                .resource
                .id
                .as_str(),
            "run-b"
        );
    }

    #[test]
    fn receipt_history_is_bounded() {
        assert_eq!(MAX_RECEIPTS, 64);
    }

    #[test]
    fn session_projection_is_not_rendered_twice_when_snapshot_and_session_list_overlap() {
        let mut state = state();
        let (session, _) = session_record(0);
        state.overview.resources.push(session.session.clone());
        state.overview.sessions.push(session);
        state.set_view(View::Sessions);
        assert_eq!(state.visible_resources().len(), 1);
    }

    #[test]
    fn dynamic_panes_are_bounded_reorderable_and_independently_buffered() {
        let mut state = state();
        for index in 0..MAX_PANES {
            let (session, attachment) = session_record(index);
            assert!(state.attach(session, attachment));
        }
        let (overflow, overflow_attachment) = session_record(MAX_PANES);
        assert!(!state.attach(overflow.clone(), overflow_attachment.clone()));
        assert_eq!(state.panes.len(), MAX_PANES);

        state.focused_pane = 0;
        let first = state.panes[0].key();
        state.reorder_focused_pane(1);
        assert_eq!(state.focused_pane, 1);
        assert_eq!(state.panes[1].key(), first);

        for sequence in 2..=(MAX_TIMELINE_EVENTS as u64 + 20) {
            let update = record("run-0", sequence, "streaming");
            assert_ne!(
                state.apply_subscription(
                    Subscription::new(
                        vec![PlatformEvent {
                            cursor: cursor(sequence),
                            resource: update,
                        }],
                        cursor(sequence),
                    )
                    .expect("subscription"),
                ),
                ApplyEvents::ResyncRequired
            );
        }
        let pane = state
            .panes
            .iter()
            .find(|pane| {
                pane.record
                    .run
                    .as_ref()
                    .is_some_and(|run| run.id.as_str() == "run-0")
            })
            .expect("run pane");
        assert_eq!(pane.timeline.len(), MAX_TIMELINE_EVENTS);

        state.focused_pane = 0;
        let removed = state.remove_focused_pane().expect("remove pane");
        assert_eq!(state.panes.len(), MAX_PANES - 1);
        assert!(state.attach(overflow, overflow_attachment));
        assert_eq!(state.panes.len(), MAX_PANES);
        assert!(state.panes.iter().all(|pane| pane.key() != removed.key()));
    }

    #[test]
    fn pinning_is_explicit_and_never_changes_control() {
        let mut state = state();
        let (session, attachment) = session_record(0);
        state.attach(session, attachment);
        state.toggle_focused_pin();
        assert!(state.panes[0].pinned);
        assert!(state.panes[0].control.is_none());
        state.toggle_focused_pin();
        assert!(!state.panes[0].pinned);
    }

    #[test]
    fn reconnect_drops_control_but_regular_refresh_preserves_it() {
        let mut state = state();
        let (session, attachment) = session_record(0);
        state.attach(session, attachment);
        state.panes[0].control = Some(ControlLease {
            id: ControlLeaseId::new("lease-a").expect("lease"),
            session: state.panes[0].record.session.resource.clone(),
            client: ClientId::new("client-a").expect("client"),
            expires_at: EpochMillis::from_millis(i64::MAX),
            revision: Revision::FIRST,
        });
        let overview = state.overview.clone();
        state.replace_overview(overview.clone(), false);
        assert!(state.panes[0].control.is_some());
        state.replace_overview(overview, true);
        assert!(state.panes[0].control.is_none());
    }
}
