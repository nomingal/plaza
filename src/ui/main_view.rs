use crate::app::{ActiveView, App, Focus, MainView};
use crate::model::{HighlightMode, SourceId};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};
use ratatui::Frame;

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    match app.active_view {
        ActiveView::Search => match app.main_view {
            MainView::Results => {
                // Empty search + no results: the branded welcome / hero screen.
                if app.query.is_empty() && app.rows.is_empty() {
                    crate::ui::welcome::draw(frame, app, area);
                } else {
                    draw_results(frame, app, area);
                }
            }
            MainView::Detail => crate::ui::detail::draw(frame, app, area),
        },
        ActiveView::Manage => draw_manage(frame, app, area),
    }
}

/// The Manage view: installed-package list with upgradable packages floated to
/// the top, filtered by the search bar.
fn draw_manage(frame: &mut Frame, app: &App, area: Rect) {
    let list_border = crate::ui::border_color(app, Focus::List);
    let pal = &app.palette;
    let pkg_icon = crate::ui::ic_package(app);

    // --- installed list (updates floated to top, filtered by the search bar) ---
    let rows = app.manage_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|pk| {
            let mut spans: Vec<Span> = Vec::new();
            if !pkg_icon.is_empty() {
                spans.push(Span::styled(format!("{pkg_icon} "), Style::default().fg(pal.muted)));
            }
            spans.extend(name_cell(
                app.manage_label(pk),
                &app.manage_filter,
                Style::default().fg(pal.fg),
                app.settings.highlight,
                pal.accent,
            ));
            spans.push(Span::styled(
                format!("{:<16} ", truncate(&pk.version, 16)),
                Style::default().fg(pal.muted),
            ));
            match app.update_for(&pk.name) {
                Some(nv) => spans.push(Span::styled(
                    format!("{}{:<13} ", crate::ui::ic_update(app), truncate(nv, 13)),
                    Style::default().fg(pal.update),
                )),
                None => spans.push(Span::raw(format!("{:<15} ", ""))),
            }
            // pacman cannot tell which repo a native package came from, only that
            // it is foreign (AUR) or not, so badge just official vs aur (or flatpak).
            let (label, src) = match pk.origin.as_str() {
                "aur" => ("aur", SourceId::Aur),
                "flatpak" => ("flatpak", SourceId::Flatpak),
                "apt" => ("apt", SourceId::Apt),
                "dnf" => ("dnf", SourceId::Dnf),
                _ => ("official", SourceId::Pacman),
            };
            spans.push(crate::ui::badge_span(app, label, src, 1));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let reason = match app.manage_reason {
        crate::model::ReasonFilter::All => String::new(),
        r => format!("· {} ", r.label()),
    };
    let hints = if app.settings.show_hotkeys { "· \u{23ce} upgrade/remove · r remove · u all " } else { "" };
    let title = if app.manage_filter.is_empty() {
        format!(" installed ({}) {reason}{hints}", rows.len())
    } else {
        format!(
            " installed ({}/{}) {reason}· filter:'{}' ",
            rows.len(),
            app.installed_list.len(),
            app.manage_filter
        )
    };
    let cursor = crate::ui::cursor_symbol(app);
    // One outer box titled with the list header; the list and the detail pane sit
    // inside it as two parts separated by a vertical divider (drawn by the pane).
    let outer = crate::ui::themed_block(app, list_border, title);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let list = List::new(items)
        .highlight_style(crate::ui::highlight_style(app))
        .highlight_symbol(&cursor);
    let mut state = ListState::default();
    *state.offset_mut() = app.manage_offset.get();
    if !rows.is_empty() {
        state.select(Some(app.installed_selected.min(rows.len() - 1)));
    }
    // On a narrow terminal there is no room for the detail part, so the list fills
    // the box.
    if inner.width >= 80 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Min(0)])
            .split(inner);
        frame.render_stateful_widget(list, cols[0], &mut state);
        crate::ui::manage_detail::draw(frame, app, cols[1]);
    } else {
        frame.render_stateful_widget(list, inner, &mut state);
    }
    // Persist the offset ratatui adjusted so the next frame keeps the viewport.
    app.manage_offset.set(state.offset());
}

fn draw_results(frame: &mut Frame, app: &App, area: Rect) {
    let border = crate::ui::border_color(app, Focus::Main);
    let pal = &app.palette;
    let pkg_icon = crate::ui::ic_package(app);
    let cursor = crate::ui::cursor_symbol(app);

    let rows = app.search_rows();
    let total = rows.len();
    // Build ListItems for the visible window only. A short query against the dnf
    // catalog yields tens of thousands of rows, and every item here allocates
    // several Strings and Spans; doing that for the whole list on every frame cost
    // ~80ms per redraw at that size, on top of whatever else the frame did.
    // The block is built first so its real inner height (the skin can turn borders
    // off) drives the window.
    let block = crate::ui::themed_block(app, border, format!(" results ({total}) "));
    let height = block.inner(area).height as usize;
    let selected = app.results_selected.min(total.saturating_sub(1));
    let offset = scroll_offset(app.results_offset.get(), selected, total, height);
    let window = &rows[offset..(offset + height).min(total)];

    let items: Vec<ListItem> = window
        .iter()
        .map(|row| {
            let shown = app.effective_providers(row);
            let ver = shown
                .first()
                .map(|p| p.version.as_str())
                .or_else(|| row.providers.first().map(|p| p.version.as_str()))
                .unwrap_or("");
            let mut spans: Vec<Span> = Vec::new();
            if !pkg_icon.is_empty() {
                spans.push(Span::styled(format!("{pkg_icon} "), Style::default().fg(pal.muted)));
            }
            // Installed packages show their name green wherever they appear.
            let name_color = if row.any_installed() { pal.installed } else { pal.fg };
            spans.extend(name_cell(
                &row.name,
                &app.query,
                Style::default().fg(name_color),
                app.settings.highlight,
                pal.accent,
            ));
            for g in app.badge_groups(row) {
                match app.settings.variant_badge {
                    crate::model::VariantBadge::Count => {
                        spans.push(crate::ui::badge_span(app, &g.label, g.source_id, g.count));
                        spans.push(Span::raw(" "));
                    }
                    crate::model::VariantBadge::Repeat => {
                        for _ in 0..g.count {
                            spans.push(crate::ui::badge_span(app, &g.label, g.source_id, 1));
                            spans.push(Span::raw(" "));
                        }
                    }
                }
            }
            spans.push(Span::styled(ver.to_string(), Style::default().fg(pal.muted)));
            if row.any_installed() {
                spans.push(Span::styled(
                    format!("  {}", crate::ui::ic_check(app)),
                    Style::default().fg(pal.installed),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(crate::ui::highlight_style(app))
        .highlight_symbol(&cursor);

    // The widget is handed the window, so the selection is indexed within it and
    // its own offset stays 0; `scroll_offset` above already did the scrolling.
    let mut state = ListState::default();
    if total > 0 {
        state.select(Some(selected - offset));
    }
    frame.render_stateful_widget(list, area, &mut state);
    app.results_offset.set(offset);
}

/// Scroll offset for a list of `len` single-line rows in a viewport `height`
/// tall: keep `prev` unless the selection has moved out of view, and never leave
/// blank space past the end. This is the scrolling ratatui's `List` would do
/// internally, hoisted out so only the visible slice needs building.
fn scroll_offset(prev: usize, selected: usize, len: usize, height: usize) -> usize {
    if height == 0 || len == 0 {
        return 0;
    }
    let max = len.saturating_sub(height);
    let mut off = prev.min(max);
    if selected < off {
        off = selected;
    } else if selected >= off + height {
        off = selected + 1 - height;
    }
    off.min(max)
}

/// The package-name cell for a list: the name padded to 28 chars, with the part
/// matching `query` styled per `mode` so the eye finds it in substring matches.
fn name_cell(
    name: &str,
    query: &str,
    base: Style,
    mode: HighlightMode,
    accent: Color,
) -> Vec<Span<'static>> {
    let shown = truncate(name, 28);
    let pad = " ".repeat(28usize.saturating_sub(shown.chars().count()) + 1);
    let range = if mode == HighlightMode::Off {
        None
    } else {
        crate::search::aggregator::match_range(&shown, query)
    };
    match range {
        Some((s, e)) => {
            let hi = match mode {
                HighlightMode::Color => base.fg(accent),
                HighlightMode::Underline => base.add_modifier(Modifier::UNDERLINED),
                HighlightMode::Both => base.fg(accent).add_modifier(Modifier::UNDERLINED),
                HighlightMode::Off => base,
            };
            vec![
                Span::styled(shown[..s].to_string(), base),
                Span::styled(shown[s..e].to_string(), hi),
                Span::styled(format!("{}{}", &shown[e..], pad), base),
            ]
        }
        None => vec![Span::styled(format!("{shown}{pad}"), base)],
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::scroll_offset;

    #[test]
    fn scroll_offset_keeps_viewport_until_selection_leaves_it() {
        // Selection inside the current window: the window does not move.
        assert_eq!(scroll_offset(10, 12, 100, 10), 10);
        assert_eq!(scroll_offset(10, 10, 100, 10), 10);
        assert_eq!(scroll_offset(10, 19, 100, 10), 10);
        // Moving above the window pulls it up to the selection.
        assert_eq!(scroll_offset(10, 9, 100, 10), 9);
        assert_eq!(scroll_offset(10, 0, 100, 10), 0);
        // Moving below scrolls just far enough to show it at the bottom row.
        assert_eq!(scroll_offset(10, 20, 100, 10), 11);
        assert_eq!(scroll_offset(10, 99, 100, 10), 90);
    }

    #[test]
    fn scroll_offset_never_leaves_blank_space_or_underflows() {
        // Fewer rows than the viewport: always start at the top.
        assert_eq!(scroll_offset(5, 2, 3, 10), 0);
        // A stale offset past the new end is pulled back.
        assert_eq!(scroll_offset(90, 0, 20, 10), 0);
        assert_eq!(scroll_offset(90, 19, 20, 10), 10);
        // Degenerate sizes.
        assert_eq!(scroll_offset(4, 0, 0, 10), 0);
        assert_eq!(scroll_offset(4, 3, 100, 0), 0);
    }
}
