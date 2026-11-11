//! LazyGit-like fixed panels: selection on the left, context on the right.
//!
//! `areas()` is the single source of truth for layout AND mouse targets:
//! the runtime reuses these exact rectangles for clicks. Panels never
//! overlap; narrow terminals collapse to the active panel instead of
//! squeezing unreadable columns side by side.
use super::{App, Panel};
use crate::{model::project_label, paths};
use ratatui::{prelude::*, widgets::*};

#[derive(Clone, Copy)]
struct Theme {
    focus: Color,
    muted: Color,
    selection: Style,
    warning: Color,
}
impl Theme {
    fn new(no_color: bool) -> Self {
        if no_color {
            Self {
                focus: Color::Reset,
                muted: Color::Reset,
                selection: Style::new().reversed().bold(),
                warning: Color::Reset,
            }
        } else {
            Self {
                focus: Color::Cyan,
                muted: Color::DarkGray,
                selection: Style::new().bg(Color::DarkGray).fg(Color::White).bold(),
                warning: Color::Yellow,
            }
        }
    }
    fn block(self, title: String, active: bool) -> Block<'static> {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(format!(" {title} "))
            .border_style(Style::new().fg(if active { self.focus } else { self.muted }))
            .title_style(if active {
                Style::new().fg(self.focus).bold()
            } else {
                Style::new()
            })
    }
}

/// Split the terminal into [projects, sessions, detail].
/// Wide terminals show all three; medium ones show sessions plus context;
/// narrow or zoomed terminals show only the active panel.
pub fn areas(area: Rect, zoom: bool, panel: Panel) -> [Rect; 3] {
    let search_height = 3.min(area.height);
    let body = Rect {
        y: area.y + search_height,
        height: area.height.saturating_sub(search_height + 2),
        ..area
    };
    let empty = Rect::default();
    if zoom || area.width < 65 {
        return match panel {
            Panel::Detail => [empty, empty, body],
            Panel::Projects => [body, empty, empty],
            _ => [empty, body, empty],
        };
    }
    let cols =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(body);
    if area.width < 100 {
        return if panel == Panel::Projects {
            [cols[0], empty, cols[1]]
        } else {
            [empty, cols[0], cols[1]]
        };
    }
    let left = Layout::vertical([
        Constraint::Length(
            (body.height / 3)
                .clamp(7, 14)
                .min(body.height.saturating_sub(6)),
        ),
        Constraint::Min(3),
    ])
    .split(cols[0]);
    [left[0], left[1], cols[1]]
}

pub fn search_area(area: Rect) -> Rect {
    Rect::new(area.x, area.y, area.width, 3.min(area.height))
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let theme = Theme::new(app.no_color);
    let label = match app.panel {
        Panel::Projects => "Search projects",
        Panel::Sessions => "Search sessions",
        Panel::Detail => "Search the page text",
    };
    // Hint text only when idle: the moment editing starts the field clears,
    // so the cursor never sits inside the placeholder (see screenshot).
    let query = if app.search_text().is_empty() && !app.editing_search {
        format!(
            "{}  / or Ctrl-f",
            match app.panel {
                Panel::Projects => "Name or folder…",
                Panel::Sessions => "Name, project, or note…",
                Panel::Detail => "Text to find…",
            }
        )
    } else {
        paths::line(app.search_text())
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{}{}",
            query,
            if app.editing_search { "▏" } else { "" }
        ))
        .block(theme.block(label.into(), app.editing_search)),
        search_area(area),
    );
    let panes = areas(area, app.zoom, app.panel);
    if panes[0].width > 0 {
        projects(frame, app, panes[0], theme);
    }
    if panes[1].width > 0 {
        sessions(frame, app, panes[1], theme);
    }
    if panes[2].width > 0 {
        detail(frame, app, panes[2], theme);
    }
    let hints = match app.panel {
        Panel::Projects if area.width < 65 => "Space fold  v layout  s sort  ? actions",
        Panel::Projects => "Space fold  E/C all  v tree/flat  s sort  Enter pick  ? actions",
        Panel::Sessions if area.width < 65 => "a alerts  r to resume  Enter  ? actions",
        Panel::Sessions => "a alerts  A all  r to resume  d done  Enter open  f view  ? actions",
        Panel::Detail if area.width < 65 => "a alerts  r to resume  j/k  ? actions",
        Panel::Detail => "a alerts  A all  r to resume  j/k scroll  [ ] pages  z expand  ? actions",
    };
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(hints).style(Style::new().fg(theme.muted)),
            Rect::new(area.x, area.bottom() - 2, area.width, 1),
        );
        let status = if app.editing_search {
            "Enter confirms · Esc ends search".to_owned()
        } else if !app.status.is_empty() {
            paths::line(&app.status)
        } else if app.catalog.scanning {
            "Indexing sessions…".into()
        } else {
            app.catalog
                .warnings
                .first()
                .map(|s| paths::line(s))
                .unwrap_or_default()
        };
        let alerts = if !app.notifications.preferences.enabled {
            "Alerts off"
        } else if app.notifications.owner {
            "Alerts on"
        } else {
            "Alerts on*"
        };
        let alerts_width = (alerts.len() as u16).min(area.width);
        frame.render_widget(
            Paragraph::new(status).style(Style::new().fg(theme.warning)),
            Rect::new(
                area.x,
                area.bottom() - 1,
                area.width.saturating_sub(alerts_width + 1),
                1,
            ),
        );
        frame.render_widget(
            Paragraph::new(alerts).style(Style::new().fg(theme.muted)),
            Rect::new(
                area.right() - alerts_width,
                area.bottom() - 1,
                alerts_width,
                1,
            ),
        );
    }
    if let Some(menu) = &app.command_menu {
        command_menu(frame, app, menu, theme);
    } else if let Some(text) = &app.modal {
        let w = area
            .width
            .saturating_sub(4)
            .min(if app.modal_details { 100 } else { 76 });
        let h = area
            .height
            .saturating_sub(2)
            .min(if app.modal_details { 24 } else { 16 });
        let rect = Rect::new(
            area.x + (area.width - w) / 2,
            area.y + (area.height - h) / 2,
            w,
            h,
        );
        frame.render_widget(Clear, rect);
        let block = theme.block(
            if app.modal_details {
                "Details · ↑↓ scroll · Esc close"
            } else {
                "Actions"
            }
            .into(),
            true,
        );
        let inner = block.inner(rect);
        let paragraph = Paragraph::new(paths::clean(text)).wrap(Wrap { trim: false });
        let max = paragraph
            .line_count(inner.width)
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16;
        app.modal_max_scroll.set(max);
        frame.render_widget(block, rect);
        frame.render_widget(paragraph.scroll((app.modal_scroll.min(max), 0)), inner);
    }
}

pub fn menu_area(area: Rect, count: usize) -> Rect {
    let w = area
        .width
        .saturating_sub(if area.width >= 20 { 4 } else { 0 })
        .min(82);
    let h = area
        .height
        .saturating_sub(if area.height >= 10 { 2 } else { 0 })
        .min((count.saturating_add(10).min(24)) as u16);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}
pub fn menu_list_area(area: Rect, count: usize) -> Rect {
    let outer = menu_area(area, count);
    if outer.height < 8 {
        return Block::bordered().inner(outer);
    }
    let top = if outer.height >= 14 { 4 } else { 3 };
    let left = if outer.width >= 60 { 19 } else { 2 };
    Rect::new(
        outer.x + left,
        outer.y + top,
        outer.width.saturating_sub(left + 2),
        outer.height.saturating_sub(top + 5),
    )
}
pub fn menu_groups_area(area: Rect, count: usize) -> Rect {
    let outer = menu_area(area, count);
    if outer.width < 60 || outer.height < 8 {
        return Rect::default();
    }
    let body = menu_list_area(area, count);
    // Thirteen cells for "Notifications", plus the two-cell selection marker.
    Rect::new(outer.x + 2, body.y, 15, body.height)
}
pub(super) fn clipped(text: &str, width: u16) -> String {
    let text = paths::line(text);
    if Line::from(text.clone()).width() <= width as usize {
        return text;
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let size = Span::raw(c.to_string()).width();
        if used + size > width.saturating_sub(1) as usize {
            break;
        }
        used += size;
        out.push(c);
    }
    out.push('…');
    out
}
fn command_menu(frame: &mut Frame, app: &App, menu: &super::menu::CommandMenu, theme: Theme) {
    let rows = menu.rows();
    let count = menu.display_height();
    let outer = menu_area(frame.area(), count);
    let body = menu_list_area(frame.area(), count);
    let categories = menu_groups_area(frame.area(), count);
    let muted = if app.no_color {
        Color::Reset
    } else {
        Color::Gray
    };
    if !app.no_color {
        for cell in &mut frame.buffer_mut().content {
            cell.set_style(
                Style::new()
                    .fg(Color::DarkGray)
                    .bg(Color::Reset)
                    .remove_modifier(Modifier::BOLD | Modifier::REVERSED),
            );
        }
    }
    frame.render_widget(Clear, outer);
    frame.render_widget(theme.block(menu.title.into(), true), outer);
    let groups = menu.groups();
    let group = menu.group_index();
    if outer.height >= 8 {
        frame.render_widget(
            Paragraph::new(clipped(&menu.subtitle, outer.width.saturating_sub(4)))
                .style(Style::new().fg(muted)),
            Rect::new(outer.x + 2, outer.y + 1, outer.width.saturating_sub(4), 1),
        );
        if outer.height >= 14 {
            frame.render_widget(
                Paragraph::new(clipped(&menu.context, outer.width.saturating_sub(4)))
                    .style(Style::new().bold()),
                Rect::new(outer.x + 2, outer.y + 2, outer.width.saturating_sub(4), 1),
            );
        }
        let label = if categories.width > 0 {
            format!(
                "{}  {}/{}",
                groups[group],
                rows.iter()
                    .position(|i| *i == Some(menu.selected))
                    .unwrap_or(0)
                    + 1,
                rows.len()
            )
        } else {
            format!("{}  ←→/Tab  {}/{}", groups[group], group + 1, groups.len())
        };
        frame.render_widget(
            Paragraph::new(clipped(&label, body.width)).style(Style::new().fg(theme.focus).bold()),
            Rect::new(body.x, body.y.saturating_sub(1), body.width, 1),
        );
    }
    if categories.width > 0 {
        frame.render_widget(
            Paragraph::new("Categories").style(Style::new().fg(muted)),
            Rect::new(categories.x, categories.y - 1, categories.width, 1),
        );
        let mut state = ListState::default().with_selected(Some(group));
        frame.render_stateful_widget(
            List::new(groups.iter().map(|g| ListItem::new(*g)))
                .highlight_symbol("▸ ")
                .highlight_style(Style::new().fg(theme.focus).bold()),
            categories,
            &mut state,
        );
        app.menu_group_offset.set(state.offset());
        frame.render_widget(
            Paragraph::new((0..body.height).map(|_| "│").collect::<Vec<_>>().join("\n"))
                .style(Style::new().fg(theme.muted)),
            Rect::new(body.x.saturating_sub(2), body.y, 1, body.height),
        );
    }
    let items = rows
        .iter()
        .filter_map(|i| *i)
        .map(|i| {
            let a = &menu.actions[i];
            let key = format!("[{}]", a.shortcut);
            let width = body.width.saturating_sub(key.len() as u16 + 4);
            let label = clipped(
                &format!("{}{}", if a.enabled { "" } else { "– " }, a.label),
                width,
            );
            let pad = width as usize - Line::from(label.clone()).width();
            ListItem::new(Line::from(vec![
                Span::raw(format!("{label}{} ", " ".repeat(pad))),
                Span::styled(
                    key,
                    Style::new().fg(if a.enabled { theme.focus } else { muted }),
                ),
            ]))
            .style(if a.enabled {
                Style::new()
            } else {
                Style::new().fg(muted)
            })
        })
        .collect::<Vec<_>>();
    let selected = rows
        .iter()
        .position(|i| *i == Some(menu.selected))
        .unwrap_or(0);
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(theme.selection),
        body,
        &mut state,
    );
    app.menu_offset.set(state.offset());
    if outer.height >= 8 {
        let selected = menu.actions.get(menu.selected);
        let description = menu.notice.as_deref().unwrap_or_else(|| {
            selected
                .map(|a| {
                    if a.enabled {
                        a.description
                    } else {
                        a.disabled_reason
                    }
                })
                .unwrap_or("")
        });
        let warning = menu.notice.is_some() || selected.is_some_and(|a| !a.enabled);
        frame.render_widget(
            Paragraph::new("─".repeat(outer.width.saturating_sub(4) as usize))
                .style(Style::new().fg(theme.muted)),
            Rect::new(
                outer.x + 2,
                outer.bottom() - 5,
                outer.width.saturating_sub(4),
                1,
            ),
        );
        frame.render_widget(
            Paragraph::new(description)
                .wrap(Wrap { trim: false })
                .style(Style::new().fg(if warning { theme.warning } else { muted })),
            Rect::new(
                outer.x + 2,
                outer.bottom() - 4,
                outer.width.saturating_sub(4),
                2,
            ),
        );
        frame.render_widget(
            Paragraph::new(if outer.width < 50 {
                "Tab groups  ↑↓ move  ↵ run  Esc"
            } else {
                "Tab/←→ groups   ↑↓ choose   Enter run   Esc close"
            })
            .style(Style::new().fg(theme.focus)),
            Rect::new(
                outer.x + 2,
                outer.bottom() - 2,
                outer.width.saturating_sub(4),
                1,
            ),
        );
    }
}

fn projects(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let total = app.catalog.sessions.len();
    let shown = app
        .catalog
        .sessions
        .iter()
        .filter(|s| app.view.matches(s))
        .count();
    let count = if shown == total {
        total.to_string()
    } else {
        format!("{shown}/{total}")
    };
    let mut items = vec![ListItem::new(format!("All projects  {count}"))];
    for (i, row) in app.project_tree.rows.iter().enumerate() {
        let count = if row.shown == row.total {
            row.total.to_string()
        } else {
            format!("{}/{}", row.shown, row.total)
        };
        let label = app.project_tree.label(i);
        let max = area.width.saturating_sub(count.len() as u16 + 6) as usize;
        let label = label.chars().take(max).collect::<String>();
        let padding = max.saturating_sub(Line::from(label.clone()).width());
        items.push(ListItem::new(Line::from(vec![
            Span::raw(label),
            Span::styled(
                format!("{}  {count}", " ".repeat(padding)),
                Style::new().fg(theme.muted),
            ),
        ])));
    }
    let selected = app
        .project
        .as_ref()
        .and_then(|p| app.visible_project_row(p))
        .unwrap_or(0);
    let mut state = ListState::default().with_selected(Some(if app.panel == Panel::Projects {
        app.project_row
    } else {
        selected
    }));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                theme
                    .block(
                        format!(
                            "Projects{} · {} · {}",
                            if app.view == crate::model::View::All {
                                String::new()
                            } else {
                                format!(" · {}", app.view.label())
                            },
                            app.project_options.layout.label(),
                            app.project_options.order.label()
                        ),
                        app.panel == Panel::Projects && !app.editing_search,
                    )
                    .title_bottom(Line::styled(
                        format!(" {} ", app.project_tree.root),
                        Style::new().fg(theme.muted),
                    )),
            )
            .highlight_style(theme.selection)
            .highlight_symbol("› "),
        area,
        &mut state,
    );
    app.project_offset.set(state.offset());
}

fn sessions(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let rows = app.rows();
    let total = app
        .catalog
        .sessions
        .iter()
        .filter(|s| app.project.as_ref().is_none_or(|p| *p == s.metadata.cwd))
        .count();
    let title = format!(
        "{}{} · {}{}",
        app.project
            .as_ref()
            .map(|cwd| format!("{} / ", project_label(cwd)))
            .unwrap_or_default(),
        app.view.label(),
        rows.len(),
        if rows.len() < total {
            format!("/{total} · f views")
        } else {
            String::new()
        }
    );
    let block = theme.block(title, app.panel == Panel::Sessions && !app.editing_search);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if !rows.is_empty() {
        let height = inner.height as usize;
        let offset = app.row.saturating_sub(height.saturating_sub(1));
        app.session_offset.set(offset);
        let items = rows
            .iter()
            .skip(offset)
            .take(height)
            .map(|s| {
                // Badges, not symbols: [O] means a verified open pane,
                // [~] a probable title/folder hint. Same visual language
                // as [R], readable without color and without a legend hunt.
                let mark = if !s.bindings.is_empty() {
                    "[O]"
                } else if !s.probable.is_empty() {
                    "[~]"
                } else {
                    ""
                };
                let label = if app.project.is_none() {
                    format!(
                        "{} / {}",
                        project_label(&s.metadata.cwd),
                        s.metadata.title()
                    )
                } else {
                    s.metadata.title()
                };
                let resume = if s.state == crate::model::ResumeState::Resume {
                    "[R]"
                } else {
                    ""
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("[{}]", s.metadata.provider.label())),
                    Span::styled(resume, Style::new().fg(theme.focus).bold()),
                    Span::raw(mark),
                    Span::raw(if app.notifications.session_enabled(s) {
                        "[N]"
                    } else {
                        "[-]"
                    }),
                    Span::styled(
                        s.live_activity()
                            .map(|a| format!(" [{}]", a.label()))
                            .unwrap_or_default(),
                        activity_style(s.live_activity(), theme, app.no_color),
                    ),
                    Span::raw(format!(
                        " {}{}",
                        if s.id.starts_with("pane-") {
                            "live terminal · "
                        } else {
                            ""
                        },
                        paths::line(&label)
                    )),
                ]))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(app.row.saturating_sub(offset)));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(theme.selection),
            inner,
            &mut state,
        );
    } else {
        // Empty states teach the next move instead of just saying "nothing".
        let hint = if app.view == crate::model::View::Resume {
            "No sessions to resume.\n\nf → All explores the full history.\nNew sessions will appear here."
        } else {
            "No results.\n\nSearch another name or press Esc to clear.\nf changes the view"
        };
        frame.render_widget(
            Paragraph::new(hint)
                .wrap(Wrap { trim: false })
                .style(Style::new().fg(theme.muted)),
            inner,
        );
    }
    let unbound = app
        .catalog
        .unbound
        .iter()
        .filter(|p| app.project.as_ref().is_none_or(|c| *c == p.cwd))
        .filter(|p| {
            !app.catalog.sessions.iter().any(|s| {
                s.probable
                    .iter()
                    .any(|hint| hint.socket == p.socket && hint.pane == p.pane)
            })
        })
        .collect::<Vec<_>>();
    if !unbound.is_empty() && inner.height > 5 {
        let room = if rows.is_empty() {
            inner.height.saturating_sub(3)
        } else {
            inner.height.saturating_sub(rows.len() as u16 + 1)
        };
        let height = room.min(unbound.len() as u16 + 2);
        if height >= 3 {
            // Leftover space doubles as the unbound-agent strip: live agent
            // panes with no conversation identity. `o` opens the picker.
            // Agent prefix + activity word, e.g. `codex · working`.
            let text = std::iter::once("More open agent runs — o browses panes".to_string())
                .chain(unbound.iter().map(|p| format!("  {}", unbound_label(p))))
                .collect::<Vec<_>>()
                .join("\n");
            frame.render_widget(
                Paragraph::new(text).style(Style::new().fg(theme.warning)),
                Rect::new(inner.x, inner.bottom() - height, inner.width, height),
            );
        }
    }
}

/// One-line label for an unbound agent pane: agent, title, activity.
/// Activity words are lowercase Herdr-style guesses, never facts.
fn activity_style(
    state: Option<crate::activity::AgentActivity>,
    theme: Theme,
    no_color: bool,
) -> Style {
    use crate::activity::AgentActivity::*;
    let color = if no_color {
        Color::Reset
    } else {
        match state {
            Some(Working) => theme.focus,
            Some(Finished) => Color::Green,
            Some(Blocked | Waiting | Error) => theme.warning,
            _ => Color::Gray,
        }
    };
    Style::new().fg(color).bold()
}
pub fn unbound_label(pane: &crate::tmux::PaneIdentity) -> String {
    let agent = pane.provider.map(|p| p.label()).unwrap_or("agent");
    let activity = pane
        .activity
        .map(|a| format!(" · {}", a.label()))
        .unwrap_or_default();
    format!("{agent} · {}{activity}", paths::line(&pane.title))
}

fn detail(frame: &mut Frame, app: &App, area: Rect, theme: Theme) {
    let Some(session) = app.current() else {
        let text = "Pick a session\n\nEvery conversation keeps its name,\nhistory, and your note.\n\nf → All explores every saved session.\n\nHistory and names are discovered automatically.\nOpenings without the extension show as probable.\n\nNo agent ever starts on its own.";
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(theme.block("Conversation".into(), app.panel == Panel::Detail)),
            area,
        );
        return;
    };
    let title = paths::line(&format!(
        "{} / {}",
        project_label(&session.metadata.cwd),
        session.metadata.title()
    ));
    let block = theme.block(title, app.panel == Panel::Detail && !app.editing_search);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.search_match.set(0);
    let mut matched_line = None;
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(session.state.label(), Style::new().fg(theme.focus).bold()),
            Span::raw(format!("    {}", session.presence())),
        ]),
        Line::from(Span::styled(
            project_label(&session.metadata.cwd),
            Style::new().fg(theme.muted),
        )),
        Line::from(Span::styled(
            format!("Last exchange: {}", paths::date(session.metadata.updated)),
            Style::new().fg(theme.muted),
        )),
    ];
    if let Some(state) = session.live_activity() {
        lines.push(Line::styled(
            format!("Live activity: {} · inferred from terminal", state.label()),
            activity_style(Some(state), theme, app.no_color),
        ));
        if state == crate::activity::AgentActivity::Finished {
            lines.push(Line::styled(
                "Turn appears finished; the saved work decision is unchanged.",
                Style::new().fg(theme.muted),
            ));
        }
    }
    for pane in session.openings() {
        if let (Some(provider), Some(state), Some(evidence)) = (
            pane.provider,
            pane.activity,
            pane.activity_evidence.as_ref(),
        ) {
            lines.push(Line::styled(
                format!(
                    "{}: {}",
                    paths::line(&pane.target),
                    crate::activity::describe(provider, state, evidence)
                ),
                Style::new().fg(theme.muted),
            ));
        }
    }
    if !session.probable.is_empty() {
        lines.push(Line::styled(
            format!(
                "[~] {} probable pane(s) · Enter to choose",
                session.probable.len()
            ),
            Style::new().fg(theme.warning),
        ));
        lines.push(Line::styled(
            if session.id.starts_with("pane-") {
                "Live terminal; no transcript association is assumed."
            } else {
                "Preview from the saved file, not the live agent process."
            },
            Style::new().fg(theme.muted),
        ));
    }
    for binding in &session.bindings {
        if let Some(pane) = &binding.pane {
            lines.push(Line::from(format!(
                "{} / window {}",
                paths::line(&pane.session),
                paths::line(&pane.window)
            )));
        }
    }
    if session.changed_after_done() {
        lines.push(Line::styled(
            "Updated after completion",
            Style::new().fg(theme.warning),
        ));
    }
    if !session.available {
        lines.push(Line::styled(
            if session.id.starts_with("pane-") {
                "Live pane · Enter focuses · session identity unverified"
            } else {
                "Conversation unavailable"
            },
            Style::new().fg(theme.warning),
        ));
    }
    lines.push(Line::default());
    if !session.note.is_empty() {
        lines.push(Line::styled("Next step", Style::new().bold()));
        for line in paths::clean(&session.note).lines() {
            lines.push(Line::raw(line.to_owned()));
        }
        lines.push(Line::default());
    }
    if app.detail_id.as_deref() != Some(&session.id) {
        lines.push(Line::styled(
            "Loading conversation…",
            Style::new().fg(theme.muted),
        ));
    } else {
        lines.push(Line::styled(
            if app.conversation.partial {
                "Partial preview"
            } else if app.branch_selected {
                "Chosen branch"
            } else if session
                .bindings
                .iter()
                .any(|b| b.bridge.leaf == app.conversation.leaf)
            {
                "Branch of an open Pi"
            } else {
                "Latest saved branch"
            },
            Style::new().fg(theme.muted),
        ));
        if !app.conversation.warning.is_empty() {
            lines.push(Line::styled(
                paths::line(&app.conversation.warning),
                Style::new().fg(theme.warning),
            ));
        }
        if app.conversation.pages > 1 {
            lines.push(Line::styled(
                format!(
                    "Page {} of {} (newest first)  [ older  ] newer",
                    app.conversation.page + 1,
                    app.conversation.pages
                ),
                Style::new().fg(theme.muted),
            ));
        }
        for message in &app.conversation.messages {
            lines.push(Line::default());
            lines.push(Line::styled(
                format!(
                    "{}  {}",
                    paths::line(&message.role),
                    paths::date(message.time)
                ),
                Style::new().fg(theme.focus).bold(),
            ));
            for text in paths::clean(&message.text).lines() {
                let style = if !app.detail_search.is_empty()
                    && text
                        .to_lowercase()
                        .contains(&app.detail_search.to_lowercase())
                {
                    if matched_line.is_none() {
                        matched_line = Some(lines.len());
                    }
                    theme.selection
                } else {
                    Style::new()
                };
                lines.push(Line::styled(text.to_owned(), style));
            }
        }
    }
    if let Some(index) = matched_line {
        app.search_match.set(
            Paragraph::new(lines[..index].to_vec())
                .wrap(Wrap { trim: false })
                .line_count(inner.width)
                .min(u16::MAX as usize) as u16,
        );
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width)
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    app.max_scroll.set(max_scroll);
    frame.render_widget(paragraph.scroll((app.scroll.min(max_scroll), 0)), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    pub fn fixture() -> App {
        let mut app = App {
            view: View::All,
            ..App::default()
        };
        app.catalog.sessions = [
            "website-redesign",
            "api-migration",
            "onboarding-flow",
            "perf-audit",
            "release-notes",
        ]
        .iter()
        .enumerate()
        .map(|(i, n)| Session {
            id: format!("fixture-{i}"),
            metadata: Metadata {
                name: n.to_string(),
                cwd: "/repo/acme-website".into(),
                ..Metadata::default()
            },
            state: ResumeState::Resume,
            note: if i == 0 {
                "Check the hero spacing in a real browser.".into()
            } else {
                String::new()
            },
            done_revision: 0,
            done_at: 0,
            available: true,
            presence_verified: false,
            bindings: vec![],
            probable: vec![],
        })
        .collect();
        app.refresh_filter();
        app.select(0);
        app.detail_id = app.selected.clone();
        app.conversation.messages = vec![
            Message {
                role: "You".into(),
                text: "Check the new landing copy.".into(),
                time: 0,
                tool: false,
            },
            Message {
                role: "Pi".into(),
                text: "The draft is ready for review.\nMobile spacing still needs checking.".into(),
                time: 0,
                tool: false,
            },
        ];
        app
    }
    #[test]
    fn live_states_are_visible_in_rows_without_hiding_resume_or_changing_notes() {
        use crate::activity::{AgentActivity, MatchEvidence};
        let mut app = fixture();
        for (s, state) in app.catalog.sessions.iter_mut().zip([
            AgentActivity::Working,
            AgentActivity::Finished,
            AgentActivity::Waiting,
            AgentActivity::Error,
            AgentActivity::Unknown,
        ]) {
            s.probable.push(crate::tmux::PaneIdentity {
                socket: "/fake".into(),
                server: "s".into(),
                pane: "%1".into(),
                pane_pid: 1,
                pane_start: "start".into(),
                client_pid: 2,
                client_start: "start".into(),
                target: "fixture:1.0".into(),
                session: "fixture".into(),
                window: "1".into(),
                cwd: s.metadata.cwd.clone(),
                command: "pi".into(),
                title: s.metadata.name.clone(),
                provider: Some(Provider::Pi),
                activity: Some(state),
                activity_evidence: Some(MatchEvidence {
                    rule_id: "synthetic".into(),
                    manifest_source: "bundled".into(),
                    manifest_version: 2,
                    ..Default::default()
                }),
            });
        }
        app.refresh_filter();
        for no_color in [false, true] {
            app.no_color = no_color;
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
            let text = snapshot_text(terminal.backend().buffer(), 160, 40);
            for label in ["working", "finished", "waiting", "error", "unknown"] {
                assert!(text.contains(&format!("[pi][R][~][N] [{label}]")));
            }
            if no_color {
                assert!(
                    terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .all(|c| c.fg == Color::Reset && c.bg == Color::Reset)
                );
            }
        }
        app.search = "finished".into();
        app.refresh_filter();
        assert_eq!(app.rows().len(), 1);
        assert_eq!(app.current().unwrap().state, ResumeState::Resume);
        assert_eq!(
            app.catalog.sessions[0].note,
            "Check the hero spacing in a real browser."
        );
    }
    #[test]
    fn unbound_labels_show_agent_and_activity_word() {
        use crate::activity::AgentActivity;
        use crate::tmux::PaneIdentity;
        let pane = |provider: Option<Provider>, activity: Option<AgentActivity>| PaneIdentity {
            socket: "/s".into(),
            server: "1".into(),
            pane: "%1".into(),
            pane_pid: 1,
            pane_start: "start".into(),
            client_pid: 2,
            client_start: "start".into(),
            target: "t:1.0".into(),
            session: "t".into(),
            window: "1".into(),
            cwd: "/repo".into(),
            command: "codex".into(),
            title: "api work".into(),
            provider,
            activity,
            activity_evidence: None,
        };
        assert_eq!(
            unbound_label(&pane(Some(Provider::Codex), Some(AgentActivity::Working))),
            "codex · api work · working"
        );
        assert_eq!(
            unbound_label(&pane(Some(Provider::Codex), None)),
            "codex · api work"
        );
        assert_eq!(unbound_label(&pane(None, None)), "agent · api work");
    }
    #[test]
    fn open_and_probable_badges_use_brackets_like_resume() {
        use crate::model::{ResumeState, View};
        let mut app = fixture();
        app.project = Some("/repo/acme-website".into());
        app.view = View::All;
        for session in &mut app.catalog.sessions {
            session.state = ResumeState::History;
        }
        // A verified binding renders [O]; a passive hint renders [~].
        // Neither depends on color, and neither looks like [R].
        app.catalog.sessions[1].bindings = vec![Binding {
            bridge: Bridge {
                version: 1,
                nonce: "n".into(),
                generation: 1,
                pid: 1,
                process_start: "start".into(),
                native_id: "native".into(),
                file: None,
                cwd: "/repo/acme-website".into(),
                name: "api-migration".into(),
                leaf: None,
                socket: None,
                pane: None,
                seen: 0,
            },
            record: std::path::PathBuf::from("/tmp/record.json"),
            pane: None,
        }];
        app.catalog.sessions[2].probable = vec![crate::tmux::PaneIdentity {
            socket: "/fake/socket".into(),
            server: "1".into(),
            pane: "%1".into(),
            pane_pid: 1,
            pane_start: "start".into(),
            client_pid: 2,
            client_start: "start".into(),
            target: "project:1.0".into(),
            session: "project".into(),
            window: "1".into(),
            cwd: "/repo/acme-website".into(),
            command: "pi".into(),
            title: "hint".into(),
            provider: Some(Provider::Pi),
            activity: None,
            activity_evidence: None,
        }];
        app.no_color = true;
        app.refresh_filter();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        let open_row = text
            .lines()
            .find(|line| line.contains("api-migration"))
            .unwrap();
        let probable_row = text
            .lines()
            .find(|line| line.contains("onboarding-flow"))
            .unwrap();
        assert!(open_row.contains("[O]"));
        assert!(!open_row.contains("[R]"));
        assert!(probable_row.contains("[~]"));
        assert!(!probable_row.contains("[R]"));
        assert!(!text.contains('●') && !text.contains('≈'));
    }
    #[test]
    fn resume_badge_is_independent_of_presence_and_visible_without_color() {
        use crate::model::{ResumeState, View};
        let mut app = fixture();
        app.project = Some("/repo/acme-website".into());
        app.view = View::All;
        app.catalog.sessions[0].state = ResumeState::Resume;
        app.catalog.sessions[1].state = ResumeState::History;
        app.catalog.sessions[2].state = ResumeState::Done;
        for session in &mut app.catalog.sessions[3..] {
            session.state = ResumeState::History;
        }
        app.no_color = true;
        app.refresh_filter();
        for (w, badge) in [(40, "[R]"), (160, "[R]")] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, 24)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
            let buffer = terminal.backend().buffer();
            let lines = (0..24)
                .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>();
            let row = lines
                .iter()
                .find(|line| line.contains("website-redesign") && line.contains(badge))
                .unwrap();
            assert!(row.contains("[pi][R]"));
            assert!(!row.contains("[~]") && !row.contains("[O]"));
            for name in ["release-notes", "api-migration"] {
                assert!(
                    !lines
                        .iter()
                        .find(|line| line.contains(name))
                        .unwrap()
                        .contains(badge)
                );
            }
            assert!(lines[22].contains("r to resume"));
            assert!(
                buffer
                    .content()
                    .iter()
                    .all(|c| c.fg == Color::Reset && c.bg == Color::Reset)
            );
        }
        app.panel = Panel::Detail;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("r to resume"));
    }
    #[test]
    fn claude_codex_and_opencode_are_visible_in_the_main_session_list() {
        let mut app = fixture();
        app.catalog.sessions.truncate(1);
        for provider in [Provider::Claude, Provider::Codex, Provider::Opencode] {
            let mut row = app.catalog.sessions[0].clone();
            row.id = format!("pane-{}", provider.label());
            row.metadata.provider = provider;
            row.metadata.name = provider.label().into();
            row.available = false;
            app.catalog.sessions.push(row);
        }
        app.refresh_filter();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        for provider in ["claude", "codex", "opencode"] {
            assert!(text.contains(&format!("[{provider}]")));
        }
        assert!(text.contains("live terminal"));
        assert!(text.contains("All · 4"));
    }
    #[test]
    fn project_counts_distinguish_matching_rows_from_total_history() {
        let mut app = fixture();
        for session in &mut app.catalog.sessions {
            session.state = crate::model::ResumeState::History;
        }
        app.view = crate::model::View::Resume;
        app.panel = Panel::Projects;
        app.refresh_filter();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("All projects  0/5"));
        assert!(text.contains("Search projects"));
        assert!(app.visible.is_empty());
        assert!(app.projects().is_empty());
        // Entering search clears the placeholder before the first keystroke,
        // so the cursor never renders inside the hint text.
        app.editing_search = true;
        let mut editing = Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        editing.draw(|f| draw(f, &app)).unwrap();
        let editing_text = editing
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(!editing_text.contains("Name or folder"));
        assert!(editing_text.contains("Search projects"));
    }
    #[test]
    fn every_saved_provider_has_an_agent_badge_with_or_without_resume() {
        let mut app = fixture();
        app.project = Some("/repo/acme-website".into());
        for (i, provider) in [
            Provider::Pi,
            Provider::Claude,
            Provider::Codex,
            Provider::Opencode,
        ]
        .into_iter()
        .enumerate()
        {
            app.catalog.sessions[i].metadata.provider = provider;
            app.catalog.sessions[i].state = if i == 0 {
                ResumeState::Resume
            } else {
                ResumeState::History
            };
        }
        app.refresh_filter();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("[pi][R]"));
        for provider in ["claude", "codex", "opencode"] {
            assert!(text.contains(&format!("[{provider}]")));
        }
    }
    #[test]
    fn no_color_still_has_visible_selection() {
        let mut app = fixture();
        app.no_color = true;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|c| c.fg == Color::Reset && c.bg == Color::Reset)
        );
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|c| c.modifier.contains(Modifier::REVERSED))
        );
    }
    fn snapshot_text(buffer: &ratatui::buffer::Buffer, w: u16, h: u16) -> String {
        // Trailing blank cells carry no visual meaning; keep fixtures diff-clean.
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
                    + "\n"
            })
            .collect()
    }
    #[test]
    fn readable_contextual_action_menus() {
        for (w, h) in [(40, 12), (80, 24), (120, 32)] {
            for panel in [Panel::Projects, Panel::Sessions, Panel::Detail] {
                let mut app = fixture();
                app.panel = panel;
                let mut menu = super::super::menu::CommandMenu::new(&app);
                menu.selected = menu
                    .actions
                    .iter()
                    .position(|action| action.key == crossterm::event::KeyCode::Char('R'))
                    .unwrap();
                app.command_menu = Some(menu);
                let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
                terminal.draw(|f| draw(f, &app)).unwrap();
                let buffer = terminal.backend().buffer();
                let text = snapshot_text(buffer, w, h);
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("tests/snapshots/menu-{panel:?}-{w}x{h}.txt"));
                if std::env::var("UPDATE_LAYOUT_SNAPSHOTS").as_deref() == Ok("1") {
                    std::fs::write(&path, &text).unwrap();
                }
                assert_eq!(text, std::fs::read_to_string(path).unwrap());
                assert!(text.contains("↑↓"));
                assert!(text.contains("Refresh catalog"));
            }
        }
    }
    #[test]
    fn live_action_menu_is_legible_compact_and_clips_context_in_terminal_cells() {
        for (w, h) in [(40, 12), (80, 24), (120, 32)] {
            let mut app = fixture();
            app.catalog.sessions[0].id = "pane-synthetic".into();
            app.catalog.sessions[0].metadata.provider = Provider::Claude;
            app.catalog.sessions[0].metadata.name =
                "Review the navigation and accessibility of the synthetic website mockup 界界界"
                    .into();
            app.select(0);
            let mut menu = super::super::menu::CommandMenu::new(&app);
            menu.selected = menu
                .actions
                .iter()
                .position(|a| a.key == crossterm::event::KeyCode::Char('n'))
                .unwrap();
            assert!(!menu.actions[menu.selected].enabled);
            let count = menu.display_height();
            app.command_menu = Some(menu);
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
            let text = snapshot_text(terminal.backend().buffer(), w, h);
            assert!(text.contains("Saved session required"));
            assert!(text.contains("[n]"));
            if w >= 80 {
                assert!(text.contains("Categories"));
                assert!(text.contains('…'));
            }
            assert!(menu_area(Rect::new(0, 0, w, h), count).height <= 24);
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/snapshots/actions-live-{w}x{h}.txt"));
            if std::env::var("UPDATE_LAYOUT_SNAPSHOTS").as_deref() == Ok("1") {
                std::fs::write(&path, &text).unwrap();
            }
            assert_eq!(text, std::fs::read_to_string(path).unwrap());
            app.no_color = true;
            terminal.draw(|f| draw(f, &app)).unwrap();
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|c| c.fg == Color::Reset && c.bg == Color::Reset)
            );
        }
        assert_eq!(clipped("界界界", 5), "界界…");
        for (w, h) in [(1, 1), (4, 3), (16, 6)] {
            let mut app = fixture();
            app.command_menu = Some(super::super::menu::CommandMenu::new(&app));
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
        }
    }
    #[test]
    fn project_identity_is_visible_without_technical_titles() {
        let mut app = fixture();
        for s in &mut app.catalog.sessions {
            s.metadata.cwd = "/repo/acme-website".into();
        }
        app.search = "acme website".into();
        app.refresh_filter();
        assert_eq!(app.rows().len(), 5);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Acme Website / website-redesign"));
        let mut menu = super::super::menu::CommandMenu::new(&app);
        menu.selected = 0;
        app.command_menu = Some(menu);
        app.no_color = true;
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|c| c.fg == Color::Reset && c.bg == Color::Reset)
        );
    }
    #[test]
    fn project_layout_and_folding_snapshots() {
        for (w, h) in [(40, 12), (80, 24)] {
            for variant in ["collapsed", "flat", "recent"] {
                let mut app = fixture();
                app.panel = Panel::Projects;
                for (id, cwd, updated) in [
                    ("api", "/repo/acme-website/api", 20),
                    ("tests", "/repo/acme-website/api/tests", 30),
                    ("tools", "/repo/tooling", 50),
                ] {
                    let mut s = app.catalog.sessions[0].clone();
                    s.id = id.into();
                    s.metadata.cwd = cwd.into();
                    s.metadata.updated = updated;
                    app.catalog.sessions.push(s);
                }
                app.refresh_filter();
                match variant {
                    "collapsed" => app.fold_all_projects(true),
                    "flat" => app.toggle_project_layout(),
                    _ => app.toggle_project_order(),
                }
                let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
                terminal.draw(|f| draw(f, &app)).unwrap();
                let buffer = terminal.backend().buffer();
                let text = snapshot_text(buffer, w, h);
                assert!(text.contains(match variant {
                    "collapsed" => "▸",
                    "flat" => "Flat",
                    _ => "Recent",
                }));
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(format!("tests/snapshots/projects-{variant}-{w}x{h}.txt"));
                if std::env::var("UPDATE_LAYOUT_SNAPSHOTS").as_deref() == Ok("1") {
                    std::fs::write(&path, &text).unwrap();
                }
                assert_eq!(text, std::fs::read_to_string(path).unwrap());
            }
        }
    }
    #[test]
    fn alert_badges_master_and_menu_remain_legible_without_color() {
        for (w, h) in [(40, 12), (80, 24), (120, 32)] {
            let mut app = fixture();
            app.no_color = true;
            app.notifications.preferences.enabled = true;
            app.notifications.owner = true;
            app.notifications
                .preferences
                .sessions
                .insert("fixture-0".into(), false);
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
            let text = snapshot_text(terminal.backend().buffer(), w, h);
            assert!(text.contains("[pi][R][-]"));
            assert!(text.contains("[pi][R][N]"));
            assert!(text.contains("Alerts on"));
            let mut menu = super::super::menu::CommandMenu::new(&app);
            let group = menu
                .groups()
                .iter()
                .position(|group| *group == "Notifications")
                .unwrap();
            menu.choose_group(group);
            assert_eq!(
                menu.actions[menu.selected].label,
                "Enable session notifications"
            );
            app.command_menu = Some(menu);
            terminal.draw(|f| draw(f, &app)).unwrap();
            let text = snapshot_text(terminal.backend().buffer(), w, h);
            assert!(text.contains("Notifications"));
            assert!(text.contains("[a]"));
            assert!(text.contains("[A]"));
        }
    }
    #[test]
    fn readable_layouts() {
        for (w, h) in [(40, 12), (80, 24), (120, 32), (160, 40)] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            let app = fixture();
            terminal.draw(|f| draw(f, &app)).unwrap();
            let buffer = terminal.backend().buffer();
            let text = snapshot_text(buffer, w, h);
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/snapshots/layout-{w}x{h}.txt"));
            if std::env::var("UPDATE_LAYOUT_SNAPSHOTS").as_deref() == Ok("1") {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &text).unwrap();
            }
            assert_eq!(text, std::fs::read_to_string(path).unwrap());
            assert!(text.contains(if w < 65 { "websit" } else { "website-redesign" }));

            assert!(!text.contains("fixture-"));
            if w >= 80 {
                assert!(text.contains("Next step"));
                assert!(text.contains("You"));
            }
        }
    }
}
