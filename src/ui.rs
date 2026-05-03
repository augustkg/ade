use ansi_to_tui::IntoText;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::{
    App, AppState, CreateField, CreateForm, FocusArea, HostField, HostForm, Notice, NoticeKind,
    PendingConfirm, SessionAction,
};
use crate::claude_status::ClaudeState;
use crate::hosts::{Host, HostKind};
use crate::model::{Folder, Machine, Row, Session};
use crate::text_field::TextField;
use crate::theme;

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(3),
    ])
    .split(frame.area());

    render_title(frame, chunks[0], app);

    let in_hosts_view = matches!(
        app.state,
        AppState::HostsList { .. } | AppState::HostForm(_)
    );
    if in_hosts_view {
        render_hosts_list(frame, chunks[1], app);
    } else if app.preview_pane_enabled {
        // 40/60 split: tree on the left at 40%, preview pane on the
        // right at 60%. The preview gets the larger share because it's
        // either the live tmux snapshot (read-only) or — when the user
        // has Tab'd in — the interactive embedded terminal.
        let body = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(chunks[1]);
        render_tree(frame, body[0], app);
        render_preview_pane(frame, body[1], app);
    } else {
        render_tree(frame, chunks[1], app);
    }

    // Footer can stack up to three opt-in/warning nudges above the help
    // bar:
    //   1. Tmux clipboard config not installed (`should_show_tmux_nudge`)
    //   2. Desktop notifications first-run prompt (`should_show_notifications_nudge`)
    //   3. Notifications enabled but hooks stale (`should_show_hooks_stale_nudge`)
    //
    // Each nudge is a single row; we add a blank spacer between the
    // last nudge and the help bar so they don't visually run together.
    let nudges = [
        (
            app.should_show_tmux_nudge(),
            render_tmux_nudge as fn(&mut Frame, Rect),
        ),
        (
            app.should_show_notifications_nudge(),
            render_notifications_nudge as fn(&mut Frame, Rect),
        ),
        (
            app.should_show_hooks_stale_nudge(),
            render_hooks_stale_nudge as fn(&mut Frame, Rect),
        ),
    ];
    let active: Vec<&fn(&mut Frame, Rect)> = nudges
        .iter()
        .filter_map(|(show, f)| if *show { Some(f) } else { None })
        .collect();
    if active.is_empty() {
        render_help_bar(frame, chunks[2], app);
    } else {
        // One row per nudge + one blank spacer + one row for the help bar.
        let mut constraints: Vec<Constraint> = vec![Constraint::Length(1); active.len() + 1];
        constraints.push(Constraint::Length(1));
        let footer = Layout::vertical(constraints).split(chunks[2]);
        for (i, render) in active.iter().enumerate() {
            render(frame, footer[i]);
        }
        let help_idx = footer.len() - 1;
        render_help_bar(frame, footer[help_idx], app);
    }

    if let AppState::CreatingSession(ref form) = app.state {
        render_create_modal(frame, app, form);
    }

    if let AppState::HostForm(ref form) = app.state {
        render_host_form_modal(frame, form);
    }

    if let AppState::Confirming(ref c) = app.state {
        render_confirm_modal(frame, c);
    }

    if let Some(ref error) = app.error_message {
        render_error_popup(frame, error);
    }
}

fn render_title(frame: &mut Frame, area: Rect, app: &App) {
    let is_refresh_focused =
        app.focus_area == FocusArea::TitleBar && matches!(app.state, AppState::Tree);

    let refresh_style = if is_refresh_focused {
        Style::default()
            .fg(theme::BASE)
            .bg(theme::MAUVE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::OVERLAY1)
    };

    let title_line = Line::from(vec![
        Span::styled(
            "  ADE — Agentic Development Environment  ",
            Style::default().fg(theme::MAUVE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Refresh ", refresh_style),
    ]);

    let title = Paragraph::new(title_line).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme::SURFACE1)),
    );
    frame.render_widget(title, area);
}

fn render_tree(frame: &mut Frame, area: Rect, app: &App) {
    let visible = app.tree.visible_rows();
    let mut items: Vec<ListItem> = Vec::with_capacity(visible.len());

    for (i, row) in visible.iter().enumerate() {
        let is_selected = i == app.selected_index && app.focus_area == FocusArea::SessionList;
        let is_renaming = matches!(app.state, AppState::RenamingSession { .. }) && is_selected;
        let renaming_folder =
            matches!(app.state, AppState::RenamingFolder { .. }) && is_selected;

        let item = match *row {
            Row::Folder(folder_idx) => {
                let folder = &app.tree.folders[folder_idx];
                if renaming_folder {
                    let mut spans = vec![Span::styled(
                        " ▼ ",
                        Style::default().fg(theme::PEACH).add_modifier(Modifier::BOLD),
                    )];
                    spans.extend(text_field_spans(&app.input_buffer, true, theme::PEACH));
                    ListItem::new(Line::from(spans))
                } else {
                    render_folder_row(folder, is_selected, app.selected_action)
                }
            }
            Row::Session(session_idx) => {
                let session = &app.tree.sessions[session_idx];
                let in_folder = session.prefix.is_some();
                if is_renaming {
                    let mut spans = vec![Span::raw(indent_for(in_folder))];
                    spans.extend(text_field_spans(&app.input_buffer, true, theme::PEACH));
                    ListItem::new(Line::from(spans))
                } else {
                    let is_current = matches!(session.machine, Machine::Local)
                        && app
                            .tree
                            .current_session
                            .as_deref()
                            .map(|n| n == session.raw_name)
                            .unwrap_or(false);
                    render_session_row(
                        session,
                        in_folder,
                        is_selected,
                        app.selected_action,
                        is_current,
                    )
                }
            }
            Row::NewSession => ListItem::new(Line::from(vec![
                Span::styled("  + ", Style::default().fg(theme::GREEN)),
                Span::styled("New session", Style::default().fg(theme::GREEN)),
            ])),
        };
        items.push(item);
    }

    let title = if app.tree.errors.is_empty() {
        " Sessions ".to_string()
    } else {
        let names: Vec<String> = app
            .tree
            .errors
            .iter()
            .map(|(m, _)| m.label().to_string())
            .collect();
        format!(" Sessions  ·  unreachable: {} ", names.join(", "))
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    title,
                    Style::default().fg(theme::SUBTEXT1),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE1)),
        )
        .highlight_style(
            Style::default()
                .bg(theme::SURFACE0)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌");

    let mut state = ListState::default();
    if app.focus_area == FocusArea::SessionList {
        state.select(Some(app.selected_index));
    }

    frame.render_stateful_widget(list, area, &mut state);
}

fn render_folder_row(
    folder: &Folder,
    is_selected: bool,
    selected_action: SessionAction,
) -> ListItem<'static> {
    let chevron = if folder.expanded { "▼" } else { "▶" };
    let title_color = if is_selected {
        theme::TEXT
    } else {
        theme::SUBTEXT1
    };

    let count = folder.sessions.len();
    let count_label = format!("  {} session{}", count, if count == 1 { "" } else { "s" });
    let machines_label = folder.machines_label();

    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled(chevron.to_string(), Style::default().fg(theme::OVERLAY2)),
        Span::raw(" "),
        Span::styled(
            folder.prefix.clone(),
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(machines_label, Style::default().fg(theme::OVERLAY1)),
        Span::styled(count_label, Style::default().fg(theme::OVERLAY0)),
    ];

    if let Some(state) = folder.claude {
        spans.push(Span::raw("  "));
        spans.push(claude_chip(state));
    }

    if is_selected {
        // SessionAction::Enter on a folder is "Toggle"; the other variants
        // map directly. Keep the same enum so cursor + h/l cycling behave
        // uniformly across row types.
        let inactive_color = theme::SUBTEXT0;
        let toggle_style = if selected_action == SessionAction::Enter {
            Style::default().fg(theme::BASE).bg(theme::MAUVE)
        } else {
            Style::default().fg(inactive_color)
        };
        let rename_style = if selected_action == SessionAction::Rename {
            Style::default().fg(theme::BASE).bg(theme::PEACH)
        } else {
            Style::default().fg(inactive_color)
        };
        let delete_style = if selected_action == SessionAction::Delete {
            Style::default().fg(theme::BASE).bg(theme::RED)
        } else {
            Style::default().fg(inactive_color)
        };

        spans.push(Span::raw("  "));
        spans.push(Span::styled(" Toggle ", toggle_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(" Rename ", rename_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(" Delete ", delete_style));
    }

    ListItem::new(Line::from(spans))
}

fn render_session_row(
    session: &Session,
    in_folder: bool,
    is_selected: bool,
    selected_action: SessionAction,
    is_current: bool,
) -> ListItem<'static> {
    let dot = if session.attached { "●" } else { "○" };
    let dot_color = if session.attached {
        theme::GREEN
    } else {
        theme::OVERLAY1
    };
    let m_color = machine_color(&session.machine);

    let inactive_color = if is_selected {
        theme::SUBTEXT0
    } else {
        theme::OVERLAY0
    };
    let enter_style = if is_selected && selected_action == SessionAction::Enter {
        Style::default().fg(theme::BASE).bg(theme::MAUVE)
    } else {
        Style::default().fg(inactive_color)
    };
    let rename_style = if is_selected && selected_action == SessionAction::Rename {
        Style::default().fg(theme::BASE).bg(theme::PEACH)
    } else {
        Style::default().fg(inactive_color)
    };
    let delete_style = if is_selected && selected_action == SessionAction::Delete {
        Style::default().fg(theme::BASE).bg(theme::RED)
    } else {
        Style::default().fg(inactive_color)
    };

    let display_name = if in_folder {
        session.leaf.clone()
    } else {
        session.raw_name.clone()
    };

    let metadata = format!(
        "  [{} window{}]",
        session.windows,
        if session.windows == 1 { "" } else { "s" },
    );

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(indent_for(in_folder)));
    spans.push(Span::styled(dot.to_string(), Style::default().fg(dot_color)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        display_name,
        Style::default().fg(if is_selected { theme::TEXT } else { theme::SUBTEXT1 }),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        session.machine.label().to_string(),
        Style::default().fg(m_color),
    ));
    spans.push(Span::styled(metadata, Style::default().fg(theme::OVERLAY0)));

    if let Some(state) = session.claude {
        spans.push(Span::raw("  "));
        spans.push(claude_chip(state));
    }

    if is_current {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            " · here ",
            Style::default()
                .fg(theme::SUBTEXT0)
                .bg(theme::SURFACE1),
        ));
    }

    if is_selected {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(" Enter ", enter_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(" Rename ", rename_style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(" Delete ", delete_style));
    }

    ListItem::new(Line::from(spans))
}

/// Small ` claude ` chip used on session and folder rows.
///
/// Renders for `Working` (Claude is actively processing a turn) and for
/// `AwaitingApproval` (Claude has popped a permission prompt and is
/// blocked on the user). Idle Claude (loaded but waiting at the prompt)
/// does not render — `tmux::map_claude_states` already drops Idle
/// upstream, so this match never sees it in practice. The Idle arm
/// exists only as a typesafety barrier; if a future refactor leaks an
/// `Idle` here we render nothing — silent suppression beats a
/// false-positive chip.
fn claude_chip(state: ClaudeState) -> Span<'static> {
    match state {
        ClaudeState::Working => Span::styled(
            " claude ",
            Style::default()
                .fg(theme::BASE)
                .bg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
        ClaudeState::AwaitingApproval => Span::styled(
            " claude · approve ",
            Style::default()
                .fg(theme::BASE)
                .bg(theme::RED)
                .add_modifier(Modifier::BOLD),
        ),
        ClaudeState::Idle => Span::raw(""),
    }
}

fn machine_color(m: &Machine) -> Color {
    const PALETTE: &[Color] = &[
        theme::SAPPHIRE,
        theme::PEACH,
        theme::GREEN,
        theme::TEAL,
        theme::PINK,
        theme::YELLOW,
        theme::LAVENDER,
        theme::SKY,
    ];
    match m {
        Machine::Local => theme::MAUVE,
        Machine::Remote(name) => {
            let h: u32 = name.bytes().map(|b| b as u32).sum::<u32>();
            PALETTE[h as usize % PALETTE.len()]
        }
    }
}

fn indent_for(in_folder: bool) -> &'static str {
    if in_folder {
        "    "
    } else {
        " "
    }
}

fn render_preview_pane(frame: &mut Frame, area: Rect, app: &App) {
    let embedded = app.embedded_active();
    let chord_pending = app.embedded_chord_pending();
    // Reset every frame; render_embedded_grid sets it back when
    // embedded is active. Keeps the panel-rect tracker accurate per
    // frame without a separate clear-on-exit hook.
    if !embedded {
        app.embedded_panel_rect.set(None);
    }
    let title = match app.preview_target() {
        Some(ref key) if embedded => format!(" embedded · {} ", key.name),
        Some(ref key) => format!(" preview · {} ", key.name),
        None => " preview ".to_string(),
    };
    // Border colour signals what the pane is doing:
    //   peach   = embedded (user keystrokes flow here)
    //   mauve   = embedded + chord prefix armed (next key is the chord)
    //   surface = ambient snapshot
    let border_color = if embedded && chord_pending {
        theme::MAUVE
    } else if embedded {
        theme::PEACH
    } else {
        theme::SURFACE1
    };
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if embedded {
        render_embedded_grid(frame, inner, app);
        return;
    }

    // Each arm renders its own Paragraph so we don't have to unify the
    // `Text` type across `ansi_to_tui` (returns `ratatui_core::Text`) and
    // direct ratatui::text::Text construction. `Paragraph::new` accepts
    // `Into<Text>` for both flavours.
    let placeholder = |frame: &mut Frame, msg: &str| {
        frame.render_widget(
            Paragraph::new(msg.to_string()).style(Style::default().fg(theme::OVERLAY1)),
            inner,
        );
    };
    match app.preview_target() {
        None => placeholder(frame, "Move the cursor to a session row to preview it here."),
        Some(ref key) => match app.preview_pane.get(key) {
            None => placeholder(frame, "Loading preview…"),
            Some(capture) => match &capture.body {
                Ok(ansi) => match ansi.as_bytes().into_text() {
                    Ok(text) => frame.render_widget(Paragraph::new(text), inner),
                    Err(_) => frame.render_widget(
                        Paragraph::new(ansi.clone()).style(Style::default().fg(theme::TEXT)),
                        inner,
                    ),
                },
                Err(e) => placeholder(frame, &format!("preview unavailable: {}", e)),
            },
        },
    }
}

/// Render the live vt100 grid of the embedded PTY into `area`. Also
/// resizes the embedded term to match the visible area on the way out
/// — the placeholder dimensions we used at spawn time get refined to
/// the actual rect the layout produced — and records the rect on the
/// App so mouse events can be filtered to "inside the embedded pane
/// only" (Phase 7).
fn render_embedded_grid(frame: &mut Frame, area: Rect, app: &App) {
    // Record the rect for the mouse handler regardless of whether the
    // PTY is alive — if it ever races to None mid-frame, the mouse
    // path uses .embedded_term.is_some() before forwarding anyway.
    app.embedded_panel_rect
        .set(Some((area.x, area.y, area.width, area.height)));

    let Some(et) = app.embedded_term.as_ref() else {
        return;
    };
    let parser = et.parser();
    // Ensure the PTY's reported size matches the rendered area. resize()
    // is gated internally on size-change so the per-frame call here is
    // cheap when nothing changed.
    let (rows, cols) = (area.height, area.width);
    if rows > 0 && cols > 0 {
        let _ = et.resize(rows, cols);
    }
    // Hand-rendered grid → ratatui Lines. Built in Phase 1; no new code
    // here, just consume.
    let lines = match parser.lock() {
        Ok(p) => crate::embedded_term::screen_to_lines(p.screen()),
        Err(_) => Vec::new(),
    };
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_hosts_list(frame: &mut Frame, area: Rect, app: &App) {
    // Vertical split: optional 2-row banner (hosts_notice), the list itself,
    // and a 1-row footer for local-machine hook status.
    let banner_h: u16 = if app.hosts_notice.is_some() { 2 } else { 0 };
    let chunks = Layout::vertical([
        Constraint::Length(banner_h),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);

    if let Some(notice) = &app.hosts_notice {
        render_notice_banner(frame, chunks[0], notice);
    }

    render_hosts_list_body(frame, chunks[1], app);
    render_local_hooks_footer(frame, chunks[2], app);
}

fn render_hosts_list_body(frame: &mut Frame, area: Rect, app: &App) {
    let selected = match app.state {
        AppState::HostsList { selected } => selected,
        AppState::HostForm(ref f) => f.editing_idx.unwrap_or(0),
        _ => 0,
    };

    let mut items: Vec<ListItem> = Vec::with_capacity(app.config.hosts.len() + 1);
    if app.config.hosts.is_empty() {
        items.push(ListItem::new(Line::from(vec![Span::styled(
            "  No hosts configured. Press 'n' to add one.",
            Style::default().fg(theme::OVERLAY1),
        )])));
    } else {
        for host in app.config.hosts.iter() {
            let hook_state = app.host_hooks.get(&host.name).copied().flatten();
            let unreachable = matches!(app.host_hooks.get(&host.name), Some(None));
            items.push(render_host_row(host, hook_state, unreachable));
        }
    }
    items.push(ListItem::new(Line::from(vec![
        Span::styled("  + ", Style::default().fg(theme::GREEN)),
        Span::styled("Add host", Style::default().fg(theme::GREEN)),
    ])));

    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(
                    " Hosts ",
                    Style::default().fg(theme::SUBTEXT1),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE1)),
        )
        .highlight_style(
            Style::default()
                .bg(theme::SURFACE0)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌");

    let mut state = ListState::default();
    let highlight = if app.config.hosts.is_empty() {
        0
    } else {
        selected.min(app.config.hosts.len() - 1)
    };
    state.select(Some(highlight));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_host_row(
    host: &Host,
    hooks_installed: Option<bool>,
    unreachable: bool,
) -> ListItem<'static> {
    let m_color = machine_color(&Machine::Remote(host.name.clone()));
    let kind_color = match host.kind {
        HostKind::Ssh => theme::TEAL,
        HostKind::Mosh => theme::SAPPHIRE,
    };
    let args = if host.ssh_args.is_empty() {
        String::new()
    } else {
        format!("  {}", host.ssh_args.join(" "))
    };

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            host.name.clone(),
            Style::default().fg(m_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{}]", host.kind.label()),
            Style::default().fg(kind_color),
        ),
        Span::raw("  "),
        Span::styled(host.target.clone(), Style::default().fg(theme::SUBTEXT1)),
        Span::styled(args, Style::default().fg(theme::OVERLAY0)),
    ];

    // Status chips — clean state shows nothing; only attention-needed states
    // get a chip, matching the rest of the UI's "quiet by default" feel.
    if unreachable {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            " unreachable ",
            Style::default().fg(theme::OVERLAY1).bg(theme::SURFACE1),
        ));
    } else if hooks_installed == Some(false) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            " install hooks ",
            Style::default()
                .fg(theme::BASE)
                .bg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ));
    }

    ListItem::new(Line::from(spans))
}

fn render_local_hooks_footer(frame: &mut Frame, area: Rect, app: &App) {
    let line = if app.local_hooks_installed {
        Line::from(vec![
            Span::styled(
                " Local: hooks installed",
                Style::default().fg(theme::OVERLAY1),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                " Local: hooks missing",
                Style::default().fg(theme::PEACH).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " — press ",
                Style::default().fg(theme::OVERLAY1),
            ),
            Span::styled(
                "L",
                Style::default().fg(theme::PEACH).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to install",
                Style::default().fg(theme::OVERLAY1),
            ),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_notice_banner(frame: &mut Frame, area: Rect, notice: &Notice) {
    let (bg, fg, prefix) = match notice.kind {
        NoticeKind::Success => (theme::GREEN, theme::BASE, "✓"),
        NoticeKind::Warning => (theme::PEACH, theme::BASE, "⚠"),
        NoticeKind::Error => (theme::RED, theme::BASE, "✕"),
        NoticeKind::Info => (theme::SAPPHIRE, theme::BASE, "ℹ"),
    };

    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", prefix),
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            notice.text.clone(),
            Style::default().fg(theme::TEXT),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_tmux_nudge(frame: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            " Tip ",
            Style::default()
                .fg(theme::BASE)
                .bg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            "Tmux clipboard not configured — run ",
            Style::default().fg(theme::TEXT),
        ),
        Span::styled(
            "ade install-tmux-config",
            Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " for drag-select-to-copy.  ",
            Style::default().fg(theme::TEXT),
        ),
        Span::styled("(x to dismiss)", Style::default().fg(theme::OVERLAY2)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Warning shown when notifications are enabled but the Claude hooks
/// aren't installed (or are stale v1) on local OR any remote — without
/// re-running install-hooks the user will never receive the
/// `permission_prompt` matcher's banners. Distinct copy from the
/// notifications first-run nudge so users with both visible can tell
/// them apart.
fn render_hooks_stale_nudge(frame: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            " Heads-up ",
            Style::default()
                .fg(theme::BASE)
                .bg(theme::RED)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            "Claude hooks out of date — run ",
            Style::default().fg(theme::TEXT),
        ),
        Span::styled(
            "ade install-hooks",
            Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " (and per-host) so notifications fire on every Claude event.",
            Style::default().fg(theme::TEXT),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// First-run prompt for the desktop-notification feature. Shown only when
/// `state.notifications.enabled` is false AND `state.notifications.first_seen`
/// is also false. Pressing `N` enables (and flips first_seen); pressing `x`
/// dismisses (also flips first_seen). Either way the nudge never shows
/// again unless the user manually edits `state.toml`.
fn render_notifications_nudge(frame: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            " Tip ",
            Style::default()
                .fg(theme::BASE)
                .bg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            "Desktop notifications available — press ",
            Style::default().fg(theme::TEXT),
        ),
        Span::styled(
            "N",
            Style::default()
                .fg(theme::PEACH)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " to enable. Banner when Claude finishes in any session.  ",
            Style::default().fg(theme::TEXT),
        ),
        Span::styled("(x to dismiss)", Style::default().fg(theme::OVERLAY2)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_help_bar(frame: &mut Frame, area: Rect, app: &App) {
    let key = |s: &'static str| Span::styled(s, Style::default().fg(theme::PEACH));
    let txt = |s: &'static str| Span::styled(s, Style::default().fg(theme::OVERLAY2));

    // Embedded mode shadows the entire keymap — show the only two
    // shortcuts that matter while the PTY is taking input. When the
    // chord prefix is armed, show a louder hint so the user knows
    // their next key is going through the chord layer (matches the
    // mauve border in render_preview_pane).
    if app.embedded_active() {
        let line = if app.embedded_chord_pending() {
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Ctrl+Space armed",
                    Style::default()
                        .fg(theme::MAUVE)
                        .add_modifier(Modifier::BOLD),
                ),
                txt(" · "),
                key("q"),
                txt(" exit  "),
                key("Ctrl+Space"),
                txt(" send literal  any other key passes through"),
            ])
        } else {
            Line::from(vec![
                Span::raw(" "),
                key("Ctrl+Space q"),
                txt(" exit embedded  "),
                key("Ctrl+Space Ctrl+Space"),
                txt(" send literal NUL"),
            ])
        };
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    let help_text = match app.state {
        AppState::Tree => vec![
            Span::raw(" "),
            key("↑↓/jk"),
            txt(" nav  "),
            key("o/␣"),
            txt(" expand  "),
            key("⏎"),
            txt(" attach  "),
            key("Tab"),
            txt(" embed  "),
            key("p"),
            txt(" preview-pane  "),
            key("N"),
            txt(" notify  "),
            key("n"),
            txt(" new  "),
            key("R"),
            txt(" rename  "),
            key("d"),
            txt(" delete  "),
            key("H"),
            txt(" hosts  "),
            key("r"),
            txt(" refresh  "),
            key("q"),
            txt(" quit"),
        ],
        AppState::CreatingSession(_) => vec![
            Span::raw(" "),
            key("Tab/↑↓"),
            txt(" field  "),
            key("←→"),
            txt(" host  "),
            key("Enter"),
            txt(" create  "),
            key("Esc"),
            txt(" cancel"),
        ],
        AppState::RenamingSession { .. } => vec![
            Span::raw(" "),
            key("Enter"),
            txt(" rename  "),
            key("Esc"),
            txt(" cancel"),
        ],
        AppState::RenamingFolder { .. } => vec![
            Span::raw(" "),
            key("Enter"),
            txt(" preview cascade  "),
            key("Esc"),
            txt(" cancel"),
        ],
        AppState::Confirming(_) => vec![
            Span::raw(" "),
            key("y/Enter"),
            txt(" confirm  "),
            key("n/Esc"),
            txt(" cancel"),
        ],
        AppState::HostsList { .. } => vec![
            Span::raw(" "),
            key("↑↓/jk"),
            txt(" nav  "),
            key("n"),
            txt(" add  "),
            key("Enter/e"),
            txt(" edit  "),
            key("d"),
            txt(" delete  "),
            key("i"),
            txt(" install  "),
            key("L"),
            txt(" install local  "),
            key("Esc/H"),
            txt(" back"),
        ],
        AppState::HostForm(_) => vec![
            Span::raw(" "),
            key("Tab/↑↓"),
            txt(" field  "),
            key("←→"),
            txt(" toggle kind  "),
            key("Enter"),
            txt(" save  "),
            key("Esc"),
            txt(" cancel"),
        ],
    };

    let help = Paragraph::new(Line::from(help_text)).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme::SURFACE1)),
    );
    frame.render_widget(help, area);
}

fn render_create_modal(frame: &mut Frame, app: &App, form: &CreateForm) {
    let area = centered_rect(55, 40, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Span::styled(
            " New session ",
            Style::default().fg(theme::MAUVE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::MAUVE));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // Machine
        Constraint::Length(1), // Folder
        Constraint::Length(1), // Session
        Constraint::Length(1), // spacer
        Constraint::Length(1), // Preview
    ])
    .margin(1)
    .split(inner);

    // Machine row — render a chip for Local + each configured host
    let machine_label = Span::styled(
        "Machine ",
        Style::default().fg(if form.focus == CreateField::Machine {
            theme::PEACH
        } else {
            theme::OVERLAY1
        }),
    );
    let mut machine_spans: Vec<Span<'static>> = vec![machine_label, Span::raw(" ")];
    for m in app.available_machines() {
        let label = format!(" {} ", m.label());
        let selected = m == form.machine;
        let focused = form.focus == CreateField::Machine;
        machine_spans.push(chip(&label, selected, focused, machine_color(&m)));
        machine_spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(machine_spans)), rows[0]);

    // Folder
    frame.render_widget(
        Paragraph::new(field_line(
            "Folder  ",
            &form.prefix,
            form.focus == CreateField::Prefix,
            theme::TEAL,
        )),
        rows[1],
    );

    // Session
    frame.render_widget(
        Paragraph::new(field_line(
            "Session ",
            &form.name,
            form.focus == CreateField::Name,
            theme::TEXT,
        )),
        rows[2],
    );

    // Preview
    let preview = if form.is_valid() {
        Line::from(vec![
            Span::styled("→ ", Style::default().fg(theme::OVERLAY1)),
            Span::styled(
                form.final_name(),
                Style::default()
                    .fg(theme::GREEN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  on {}", form.machine.label()),
                Style::default().fg(theme::OVERLAY1),
            ),
        ])
    } else {
        Line::from(Span::styled(
            "(enter a session name)",
            Style::default().fg(theme::OVERLAY0),
        ))
    };
    frame.render_widget(Paragraph::new(preview), rows[4]);
}

fn render_host_form_modal(frame: &mut Frame, form: &HostForm) {
    let area = centered_rect(60, 50, frame.area());
    frame.render_widget(Clear, area);

    let title = if form.editing_idx.is_some() {
        " Edit host "
    } else {
        " Add host "
    };

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(theme::MAUVE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::MAUVE));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // Name
        Constraint::Length(1), // Kind
        Constraint::Length(1), // Target
        Constraint::Length(1), // SSH args
        Constraint::Length(1), // spacer
        Constraint::Length(1), // Preview command
        Constraint::Length(1), // Spacer
        Constraint::Length(2), // Educational hint about hooks
    ])
    .margin(1)
    .split(inner);

    // Name
    frame.render_widget(
        Paragraph::new(field_line(
            "Name      ",
            &form.name,
            form.focus == HostField::Name,
            theme::MAUVE,
        )),
        rows[0],
    );

    // Kind
    let kind_label = Span::styled(
        "Kind      ",
        Style::default().fg(if form.focus == HostField::Kind {
            theme::PEACH
        } else {
            theme::OVERLAY1
        }),
    );
    let ssh_chip = chip(
        " SSH ",
        form.kind == HostKind::Ssh,
        form.focus == HostField::Kind,
        theme::TEAL,
    );
    let mosh_chip = chip(
        " Mosh ",
        form.kind == HostKind::Mosh,
        form.focus == HostField::Kind,
        theme::SAPPHIRE,
    );
    let kind_line = Line::from(vec![
        kind_label,
        Span::raw(" "),
        ssh_chip,
        Span::raw(" "),
        mosh_chip,
    ]);
    frame.render_widget(Paragraph::new(kind_line), rows[1]);

    // Target
    frame.render_widget(
        Paragraph::new(field_line(
            "Target    ",
            &form.target,
            form.focus == HostField::Target,
            theme::TEXT,
        )),
        rows[2],
    );

    // SSH args
    frame.render_widget(
        Paragraph::new(field_line(
            "SSH args  ",
            &form.ssh_args,
            form.focus == HostField::SshArgs,
            theme::SUBTEXT1,
        )),
        rows[3],
    );

    // Preview
    let preview = if form.is_valid() {
        let host = form.to_host();
        let cmd = preview_command(&host);
        Line::from(vec![
            Span::styled("→ ", Style::default().fg(theme::OVERLAY1)),
            Span::styled(
                cmd,
                Style::default().fg(theme::GREEN).add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(Span::styled(
            "(name and target are required)",
            Style::default().fg(theme::OVERLAY0),
        ))
    };
    frame.render_widget(Paragraph::new(preview), rows[5]);

    // Educational hint about what saving will do.
    let hint_lines = vec![
        Line::from(Span::styled(
            "On save, ADE installs Claude Code status hooks on this host so",
            Style::default().fg(theme::OVERLAY1),
        )),
        Line::from(Span::styled(
            "the tree shows when Claude is busy vs idle there.",
            Style::default().fg(theme::OVERLAY1),
        )),
    ];
    frame.render_widget(Paragraph::new(hint_lines), rows[7]);
}

fn preview_command(host: &Host) -> String {
    let prefix = match host.kind {
        HostKind::Ssh => "ssh",
        HostKind::Mosh => "mosh",
    };
    let args = if host.ssh_args.is_empty() {
        String::new()
    } else {
        format!(" {}", host.ssh_args.join(" "))
    };
    format!("{}{} {}", prefix, args, host.target)
}

fn chip(
    label: &str,
    selected: bool,
    focused: bool,
    accent: ratatui::style::Color,
) -> Span<'static> {
    let style = if selected && focused {
        Style::default()
            .fg(theme::BASE)
            .bg(accent)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::OVERLAY1)
    };
    Span::styled(label.to_string(), style)
}

fn field_line(
    label: &'static str,
    field: &TextField,
    focused: bool,
    accent: ratatui::style::Color,
) -> Line<'static> {
    let label_style = Style::default().fg(if focused { theme::PEACH } else { theme::OVERLAY1 });
    let mut spans: Vec<Span<'static>> = vec![Span::styled(label, label_style)];
    spans.extend(text_field_spans(field, focused, accent));
    Line::from(spans)
}

/// Render a text field as a sequence of spans, with the cursor drawn as an
/// inverted block at the current cursor position when `focused` is true.
fn text_field_spans(
    field: &TextField,
    focused: bool,
    accent: ratatui::style::Color,
) -> Vec<Span<'static>> {
    if !focused {
        return vec![Span::styled(
            field.as_str().to_string(),
            Style::default().fg(theme::SUBTEXT1),
        )];
    }

    let value = field.as_str();
    let cursor = field.cursor();
    let normal = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    let caret = Style::default()
        .fg(theme::BASE)
        .bg(accent)
        .add_modifier(Modifier::BOLD);

    let before = &value[..cursor];
    let at = value[cursor..].chars().next();
    let after_start = cursor + at.map(|c| c.len_utf8()).unwrap_or(0);
    let after = &value[after_start..];
    let caret_char = at.map(|c| c.to_string()).unwrap_or_else(|| " ".to_string());

    let mut spans = Vec::new();
    if !before.is_empty() {
        spans.push(Span::styled(before.to_string(), normal));
    }
    spans.push(Span::styled(caret_char, caret));
    if !after.is_empty() {
        spans.push(Span::styled(after.to_string(), normal));
    }
    spans
}

fn render_confirm_modal(frame: &mut Frame, c: &PendingConfirm) {
    let height_pct = (c.body.len() as u16 * 2 + 12).clamp(25, 70);
    let area = centered_rect(60, height_pct, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Span::styled(
            format!(" {} ", c.title),
            Style::default().fg(theme::PEACH).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::PEACH));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, line) in c.body.iter().enumerate() {
        let style = if i == 0 {
            Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::SUBTEXT1)
        };
        lines.push(Line::from(Span::styled(line.clone(), style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            " y ",
            Style::default()
                .fg(theme::BASE)
                .bg(theme::GREEN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" confirm    "),
        Span::styled(" n ", Style::default().fg(theme::BASE).bg(theme::OVERLAY1)),
        Span::raw(" cancel"),
    ]));

    let p = Paragraph::new(lines).style(Style::default().fg(theme::TEXT));
    frame.render_widget(p, inner);
}

fn render_error_popup(frame: &mut Frame, error: &str) {
    let area = centered_rect(60, 15, frame.area());
    frame.render_widget(Clear, area);

    let error_block = Block::default()
        .title(Span::styled(
            " Error ",
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RED));

    let inner = error_block.inner(area);
    frame.render_widget(error_block, area);

    let error_text = Paragraph::new(error).style(Style::default().fg(theme::RED));
    frame.render_widget(error_text, inner);
}

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}
