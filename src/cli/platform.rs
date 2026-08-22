use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use jcode_operator_backend::platform_contract::{
    Attachment, ClientId, ControlLease, IdempotencyKey, ResourceAuthority, ResourceRecord,
    SessionRecord,
};
use jcode_operator_backend::{AutomoniqueBackend, OperatorBackend, OperatorOverview};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::terminal::init_tui_runtime;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub async fn run_platform_command(socket: Option<&str>, json: bool) -> Result<()> {
    let socket = platform_socket(socket)?;
    let backend = AutomoniqueBackend::local(&socket);
    let mut overview = backend
        .overview(ResourceAuthority::Automonique)
        .await
        .with_context(|| format!("platform endpoint unavailable at {}", socket.display()))?;
    if json {
        println!("{}", serde_json::to_string(&overview_json(&overview))?);
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("platform cockpit needs a terminal; use --json for a one-shot snapshot");
    }

    let (mut terminal, guard) = init_tui_runtime()?;
    let mut last_refresh = Instant::now();
    let mut status = String::from("live");
    let client = ClientId::new(format!("jcode-{}", std::process::id()))
        .map_err(|_| anyhow::anyhow!("could not construct platform client identity"))?;
    let mut selected = 0_usize;
    let mut attachment: Option<Attachment> = None;
    let mut control: Option<ControlLease> = None;
    loop {
        terminal.draw(|frame| {
            render(
                frame,
                &overview,
                &status,
                &socket,
                selected,
                attachment.as_ref(),
                control.as_ref(),
            )
        })?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('r') => last_refresh = Instant::now() - REFRESH_INTERVAL,
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    selected = (selected + 1).min(overview.sessions.len().saturating_sub(1));
                }
                KeyCode::Char('a') => {
                    if let Some(session) = overview.sessions.get(selected) {
                        match backend
                            .attach(session.session.resource.clone(), client.clone())
                            .await
                        {
                            Ok(value) => {
                                attachment = Some(value);
                                status = String::from("attached as observer");
                            }
                            Err(error) => status = format!("attach refused: {error}"),
                        }
                    }
                }
                KeyCode::Char('c') => {
                    if let Some(session) = overview.sessions.get(selected) {
                        match backend
                            .claim_control(
                                session.session.resource.clone(),
                                client.clone(),
                                interaction_key("claim")?,
                            )
                            .await
                        {
                            Ok(value) => {
                                control = Some(value);
                                status = String::from("exclusive control claimed");
                            }
                            Err(error) => status = format!("control refused: {error}"),
                        }
                    }
                }
                _ => {}
            }
        }
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            match backend.overview(ResourceAuthority::Automonique).await {
                Ok(next) => {
                    overview = next;
                    selected = selected.min(overview.sessions.len().saturating_sub(1));
                    status = String::from("live");
                }
                Err(_) => status = String::from("stale — reconnecting"),
            }
            last_refresh = Instant::now();
        }
    }
    guard.finish(true);
    Ok(())
}

fn interaction_key(operation: &str) -> Result<IdempotencyKey> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    IdempotencyKey::new(format!("jcode-{operation}-{}-{nanos}", std::process::id()))
        .map_err(|_| anyhow::anyhow!("could not construct platform idempotency key"))
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

fn overview_json(overview: &OperatorOverview) -> serde_json::Value {
    serde_json::json!({
        "protocol": overview.capabilities.protocol,
        "schema": overview.capabilities.schema,
        "cursor": overview.cursor.sequence.get(),
        "resources": overview.resources.iter().map(resource_json).collect::<Vec<_>>(),
        "sessions": overview.sessions.iter().map(session_json).collect::<Vec<_>>(),
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

fn session_json(record: &SessionRecord) -> serde_json::Value {
    serde_json::json!({
        "session": resource_json(&record.session),
        "run": record.run.as_ref().map(|run| serde_json::json!({
            "authority": run.authority.as_str(),
            "kind": run.kind.as_str(),
            "id": run.id.as_str(),
        })),
        "attachable": record.attachable,
        "controllable": record.controllable,
    })
}

fn render(
    frame: &mut ratatui::Frame<'_>,
    overview: &OperatorOverview,
    status: &str,
    socket: &Path,
    selected: usize,
    attachment: Option<&Attachment>,
    control: Option<&ControlLease>,
) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Percentage(48),
            Constraint::Percentage(48),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "Automonique platform v1",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {status}  cursor {}",
            overview.cursor.sequence.get()
        )),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, layout[0]);

    let resources = overview
        .resources
        .iter()
        .map(|record| {
            Line::from(format!(
                "{} / {} / {}  [{}]  {}",
                record.resource.authority.as_str(),
                record.resource.kind.as_str(),
                record.resource.id.as_str(),
                record.freshness.state.as_str(),
                record.summary.as_str(),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(resources).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(format!(" Resources ({}) ", overview.resources.len()))
                .borders(Borders::ALL),
        ),
        layout[1],
    );

    let sessions = overview
        .sessions
        .iter()
        .enumerate()
        .map(|(index, record)| {
            let marker = if index == selected { ">" } else { " " };
            Line::from(format!(
                "{marker} {}  [{}]  attach={} control={}  {}",
                record.session.resource.id.as_str(),
                record.session.freshness.state.as_str(),
                record.attachable,
                record.controllable,
                record.session.summary.as_str(),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(sessions).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(format!(" Sessions ({}) ", overview.sessions.len()))
                .borders(Borders::ALL),
        ),
        layout[2],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "↑↓ select · a observe · c control · q quit · r refresh · observed={} controlled={} · {}",
            attachment.is_some(),
            control.is_some(),
            socket.display(),
        ))
        .style(Style::default().fg(Color::DarkGray)),
        layout[3],
    );
}

#[cfg(test)]
mod tests {
    use super::platform_socket;

    #[test]
    fn explicit_socket_wins_without_touching_provider_state() {
        assert_eq!(
            platform_socket(Some("/tmp/operator.sock")).expect("socket"),
            std::path::PathBuf::from("/tmp/operator.sock")
        );
    }
}
