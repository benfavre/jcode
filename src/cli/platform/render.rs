use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use jcode_operator_backend::OperatorOverview;
use jcode_operator_backend::platform_contract::ResourceRecord;

use super::state::{CockpitState, ConnectionState, PaneLayout, SessionPane, View, coordinate_key};

pub(super) fn overview_json(overview: &OperatorOverview) -> serde_json::Value {
    serde_json::json!({
        "protocol": overview.capabilities.protocol,
        "schema": overview.capabilities.schema,
        "methods": overview.capabilities.methods.iter().map(|method| method.as_str()).collect::<Vec<_>>(),
        "actions": overview.actions.iter().map(|action| action.as_str()).collect::<Vec<_>>(),
        "cursor": overview.cursor.sequence.get(),
        "resources": overview.resources.iter().map(resource_json).collect::<Vec<_>>(),
        "sessions": overview.sessions.iter().map(|record| serde_json::json!({
            "session": resource_json(&record.session),
            "run": record.run.as_ref().map(coordinate_key),
            "attachable": record.attachable,
            "controllable": record.controllable,
        })).collect::<Vec<_>>(),
    })
}

fn resource_json(record: &ResourceRecord) -> serde_json::Value {
    serde_json::json!({
        "authority": record.resource.authority.as_str(),
        "kind": record.resource.kind.as_str(),
        "id": record.resource.id.as_str(),
        "freshness": record.freshness.state.as_str(),
        "observed_at": record.freshness.observed_at.as_millis(),
        "revision": record.freshness.revision.get(),
        "summary": record.summary.as_str(),
    })
}

pub struct ConfirmationView<'a> {
    pub title: &'a str,
    pub target: &'a str,
    pub revision: u64,
    pub consequence: &'a str,
}

pub struct PaletteView<'a> {
    pub commands: &'a [&'a str],
    pub selected: usize,
}

pub struct ComposerView<'a> {
    pub operation: &'a str,
    pub target: &'a str,
    pub text: &'a str,
}

pub fn render(
    frame: &mut ratatui::Frame<'_>,
    state: &CockpitState,
    confirmation: Option<ConfirmationView<'_>>,
    palette: Option<PaletteView<'_>>,
    composer: Option<ComposerView<'_>>,
) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let compact = area.width < 80 || area.height < 24;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(if compact { 2 } else { 3 }),
            Constraint::Min(6),
            Constraint::Length(if compact { 2 } else { 3 }),
        ])
        .split(area);

    render_header(frame, state, rows[0]);
    render_tabs(frame, state, rows[1], compact);
    render_body(frame, state, rows[2], compact);
    render_footer(frame, state, rows[3], compact);

    if state.show_help {
        render_help(frame, centered(area, 76, 24));
    }
    if let Some(palette) = palette {
        render_palette(frame, palette, centered(area, 68, 18));
    }
    if let Some(confirmation) = confirmation {
        render_confirmation(frame, confirmation, centered(area, 72, 13));
    }
    if let Some(composer) = composer {
        render_composer(frame, composer, centered(area, 76, 14));
    }
}

fn render_header(frame: &mut ratatui::Frame<'_>, state: &CockpitState, area: Rect) {
    let status_style = match state.connection {
        ConnectionState::Live => Style::default().fg(Color::Green),
        ConnectionState::Stale | ConnectionState::Reconnecting => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        ConnectionState::UpgradeRequired => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
    };
    let header = Line::from(vec![
        Span::styled(
            "AUTOMONIQUE OPERATOR",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(state.connection.label(), status_style),
        Span::raw(format!(
            "  cursor={}  panes={}  layout={}",
            state.resource_cursor.sequence.get(),
            state.panes.len(),
            state.pane_layout.label()
        )),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_tabs(frame: &mut ratatui::Frame<'_>, state: &CockpitState, area: Rect, compact: bool) {
    let mut spans = Vec::new();
    for (index, view) in View::ALL.iter().enumerate() {
        if compact && index > 3 && *view != state.view {
            continue;
        }
        let label = format!(" {}:{} ", index + 1, view.label());
        let style = if *view == state.view {
            Style::default()
                .fg(Color::Black)
                .bg(if state.high_contrast {
                    Color::White
                } else {
                    Color::Cyan
                })
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn render_body(frame: &mut ratatui::Frame<'_>, state: &CockpitState, area: Rect, compact: bool) {
    if !state.panes.is_empty() && state.view == View::Sessions {
        render_panes(frame, state, area, compact);
        return;
    }
    if compact {
        render_resource_list(frame, state, area);
        return;
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(area);
    render_resource_list(frame, state, columns[0]);
    render_detail(frame, state, columns[1]);
}

fn render_resource_list(frame: &mut ratatui::Frame<'_>, state: &CockpitState, area: Rect) {
    let records = state.visible_resources();
    let lines = records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            let selected = index == state.selected_index;
            let marker = if selected { ">" } else { " " };
            let freshness = record.freshness.state.as_str().to_uppercase();
            let text = format!(
                "{marker} {:10} {:22} [{freshness}] {}",
                record.resource.kind.as_str(),
                record.resource.id.as_str(),
                record.summary.as_str()
            );
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::styled(text, style)
        })
        .collect::<Vec<_>>();
    let empty = if records.is_empty() {
        vec![Line::styled(
            "No authorized records in this view.",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        lines
    };
    frame.render_widget(
        Paragraph::new(empty).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(format!(" {} ({}) ", state.view.label(), records.len()))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_detail(frame: &mut ratatui::Frame<'_>, state: &CockpitState, area: Rect) {
    let lines = if let Some(record) = state.selected_resource() {
        vec![
            Line::from(vec![
                Span::styled("Authority: ", label()),
                Span::raw(record.resource.authority.as_str()),
            ]),
            Line::from(vec![
                Span::styled("Kind:      ", label()),
                Span::raw(record.resource.kind.as_str()),
            ]),
            Line::from(vec![
                Span::styled("ID:        ", label()),
                Span::raw(record.resource.id.as_str()),
            ]),
            Line::from(vec![
                Span::styled("Freshness: ", label()),
                Span::raw(record.freshness.state.as_str()),
            ]),
            Line::from(vec![
                Span::styled("Revision:  ", label()),
                Span::raw(record.freshness.revision.get().to_string()),
            ]),
            Line::from(""),
            Line::from(record.summary.as_str().to_owned()),
        ]
    } else {
        vec![Line::from(
            "Select a record to inspect its authoritative projection.",
        )]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(" Detail ").borders(Borders::ALL)),
        area,
    );
}

fn render_panes(frame: &mut ratatui::Frame<'_>, state: &CockpitState, area: Rect, compact: bool) {
    let areas = pane_areas(
        area,
        state.panes.len(),
        state.focused_pane,
        state.pane_layout,
        compact,
    );
    for (slot, pane_index) in areas.iter().enumerate() {
        let Some((area, index)) = pane_index else {
            continue;
        };
        let Some(pane) = state.panes.get(*index) else {
            continue;
        };
        render_pane(frame, pane, *area, *index == state.focused_pane, slot);
    }
}

fn pane_areas(
    area: Rect,
    count: usize,
    focused: usize,
    layout: PaneLayout,
    compact: bool,
) -> Vec<Option<(Rect, usize)>> {
    if count == 0 {
        return Vec::new();
    }
    if compact || matches!(layout, PaneLayout::Tabs | PaneLayout::Focused) {
        return vec![Some((area, focused.min(count - 1)))];
    }
    match layout {
        PaneLayout::Rows => Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![Constraint::Ratio(1, count as u32); count])
            .split(area)
            .iter()
            .copied()
            .enumerate()
            .map(|(index, area)| Some((area, index)))
            .collect(),
        PaneLayout::Columns => Layout::default()
            .direction(Direction::Horizontal)
            .constraints(vec![Constraint::Ratio(1, count as u32); count])
            .split(area)
            .iter()
            .copied()
            .enumerate()
            .map(|(index, area)| Some((area, index)))
            .collect(),
        PaneLayout::Grid => {
            let columns = (count as f64).sqrt().ceil() as usize;
            let rows = count.div_ceil(columns);
            let row_areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints(vec![Constraint::Ratio(1, rows as u32); rows])
                .split(area);
            let mut result = Vec::new();
            for row in row_areas.iter() {
                let cells = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(vec![Constraint::Ratio(1, columns as u32); columns])
                    .split(*row);
                for cell in cells.iter() {
                    let index = result.len();
                    result.push((index < count).then_some((*cell, index)));
                }
            }
            result
        }
        PaneLayout::Tabs | PaneLayout::Focused => unreachable!(),
    }
}

fn render_pane(
    frame: &mut ratatui::Frame<'_>,
    pane: &SessionPane,
    area: Rect,
    focused: bool,
    slot: usize,
) {
    let control = if pane.control.is_some() {
        "CONTROL"
    } else {
        "OBSERVER"
    };
    let title = format!(
        " {} {} [{}] unread={}{} ",
        slot + 1,
        pane.record.session.resource.id.as_str(),
        control,
        pane.unread,
        if pane.pinned { " PINNED" } else { "" }
    );
    let border_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mut lines = vec![Line::from(format!(
        "state={} attach={} control={}",
        pane.record.session.summary.as_str(),
        pane.record.attachable,
        pane.record.controllable
    ))];
    for event in pane
        .timeline
        .iter()
        .rev()
        .take(area.height.saturating_sub(4) as usize)
        .rev()
    {
        lines.push(Line::from(format!(
            "r{} {}: {}",
            event.freshness.revision.get(),
            event.resource.kind.as_str(),
            event.summary.as_str()
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(border_style),
        ),
        area,
    );
}

fn render_footer(frame: &mut ratatui::Frame<'_>, state: &CockpitState, area: Rect, compact: bool) {
    let keys = if compact {
        "? help · p commands · q quit"
    } else {
        "↑↓ select · i compose · a attach · d detach · c control · x release · p commands · [ ] focus · P pin · l layout · ? help · q quit"
    };
    frame.render_widget(
        Paragraph::new(vec![Line::from(keys), Line::from(state.status.clone())])
            .style(Style::default().fg(Color::Gray))
            .block(Block::default().borders(Borders::TOP)),
        area,
    );
}

fn render_help(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let lines = vec![
        Line::styled(
            "Keyboard-only operator help",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from("1–7 switch authoritative views; arrows select by durable identity"),
        Line::from("a attach as observer; d detach without affecting the runner"),
        Line::from("c claim an exclusive short control lease; x release it"),
        Line::from("i composes an explicit new request or session follow-up"),
        Line::from("[ / ] focus panes; Shift+Left/Right reorder; P pins; l changes layout"),
        Line::from("p opens capability-driven actions; Enter previews; y executes; n cancels"),
        Line::from("r forces snapshot reconciliation; h toggles high contrast"),
        Line::from(""),
        Line::from("A stale or disconnected client is read-only. Focus never authorizes mutation."),
        Line::from("Closing this client detaches observers and releases its control leases."),
        Line::from(""),
        Line::from("Press ? or Esc to close help."),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(" Help ").borders(Borders::ALL)),
        area,
    );
}

fn render_composer(frame: &mut ratatui::Frame<'_>, composer: ComposerView<'_>, area: Rect) {
    let lines = vec![
        Line::from(vec![
            Span::styled("Operation: ", label()),
            Span::raw(composer.operation),
        ]),
        Line::from(vec![
            Span::styled("Exact target: ", label()),
            Span::raw(composer.target),
        ]),
        Line::from(""),
        Line::from(composer.text.to_owned()),
        Line::from(""),
        Line::styled(
            "Enter preview · Esc cancel · Backspace edit",
            Style::default().fg(Color::Yellow),
        ),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Durable request composer ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        area,
    );
}

fn render_palette(frame: &mut ratatui::Frame<'_>, palette: PaletteView<'_>, area: Rect) {
    let lines = palette
        .commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let marker = if index == palette.selected { ">" } else { " " };
            let style = if index == palette.selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::styled(format!("{marker} {command}"), style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Server-capability actions ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_confirmation(
    frame: &mut ratatui::Frame<'_>,
    confirmation: ConfirmationView<'_>,
    area: Rect,
) {
    let lines = vec![
        Line::styled(
            confirmation.title,
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        Line::from(vec![
            Span::styled("Exact target: ", label()),
            Span::raw(confirmation.target),
        ]),
        Line::from(vec![
            Span::styled("Revision:     ", label()),
            Span::raw(confirmation.revision.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Consequence:  ", label()),
            Span::raw(confirmation.consequence),
        ]),
        Line::from(""),
        Line::styled(
            "y execute once · n/Esc cancel",
            Style::default().fg(Color::Yellow),
        ),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .title(" Confirm typed mutation ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
        area,
    );
}

fn centered(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = max_width.min(area.width.saturating_sub(2)).max(1);
    let height = max_height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn label() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_operator_backend::OperatorOverview;
    use jcode_operator_backend::platform_contract::{
        Capabilities, CursorTopic, Freshness, FreshnessState, PlatformCursor, PlatformText,
        ResourceAuthority, ResourceCoordinate, ResourceId, ResourceKind, ResourceRecord,
    };
    use jcode_operator_backend::platform_primitives::{EpochMillis, Revision};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn state() -> CockpitState {
        let resources = vec![
            record(
                ResourceKind::Node,
                "node-a",
                "daemon ready",
                FreshnessState::Fresh,
            ),
            record(ResourceKind::Run, "run-a", "running", FreshnessState::Fresh),
            record(
                ResourceKind::Approval,
                "approval-a",
                "state=pending",
                FreshnessState::Fresh,
            ),
            record(
                ResourceKind::Model,
                "gpt-test",
                "available=true; default=true",
                FreshnessState::Fresh,
            ),
            record(
                ResourceKind::Run,
                "run-failed",
                "failed",
                FreshnessState::Stale,
            ),
        ];
        CockpitState::new(OperatorOverview {
            capabilities: Capabilities::platform_v1(),
            actions: jcode_operator_backend::platform_contract::PlatformAction::ALL.to_vec(),
            resources,
            sessions: Vec::new(),
            cursor: PlatformCursor {
                authority: ResourceAuthority::Automonique,
                topic: CursorTopic::new("resources").expect("topic"),
                sequence: Revision::new(7).expect("cursor"),
            },
        })
    }

    fn record(
        kind: ResourceKind,
        id: &str,
        summary: &str,
        freshness: FreshnessState,
    ) -> ResourceRecord {
        ResourceRecord {
            resource: ResourceCoordinate::new(
                if kind == ResourceKind::Model {
                    ResourceAuthority::Provider
                } else {
                    ResourceAuthority::Automonique
                },
                kind,
                ResourceId::new(id).expect("id"),
            ),
            freshness: Freshness {
                state: freshness,
                observed_at: EpochMillis::from_millis(1),
                revision: Revision::FIRST,
            },
            summary: PlatformText::new(summary).expect("summary"),
        }
    }

    fn screen(width: u16, height: u16, mut state: CockpitState) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &state, None, None, None))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width {
                line.push_str(buffer[(x, y)].symbol());
            }
            output.push_str(line.trim_end());
            output.push('\n');
        }
        // Exercise the high-contrast branch without making the helper's caller
        // depend on ANSI colour values.
        state.high_contrast = !state.high_contrast;
        output
    }

    #[test]
    fn wide_golden_contains_navigation_identity_and_state_text() {
        let output = screen(120, 36, state());
        assert!(output.contains("AUTOMONIQUE OPERATOR"));
        assert!(output.contains("LIVE"));
        assert!(output.contains("approval-a"));
        assert!(!output.contains("Exact target"));
    }

    #[test]
    fn narrow_golden_keeps_identity_and_non_colour_status() {
        let output = screen(58, 22, state());
        assert!(output.contains("LIVE"));
        assert!(output.contains("node-a"));
        assert!(output.contains("? help"));
    }

    #[test]
    fn stale_state_is_visibly_read_only() {
        let mut value = state();
        value.connection = ConnectionState::Stale;
        let output = screen(100, 28, value);
        assert!(output.contains("STALE / READ-ONLY"));
    }

    #[test]
    fn high_contrast_draws_without_hiding_status_text() {
        let mut value = state();
        value.high_contrast = true;
        let output = screen(100, 28, value);
        assert!(output.contains("LIVE"));
        assert!(output.contains("overview"));
    }

    #[test]
    fn zero_sized_terminal_waits_for_a_resize_without_panicking() {
        assert!(screen(0, 0, state()).is_empty());
    }
}
