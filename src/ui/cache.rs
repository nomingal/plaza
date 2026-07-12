use crate::app::{App, Focus};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// The CACHE block's height: concise idle is one total row; expanded (focused,
/// or the "full" idle mode) is one row per source plus total, and a hotkey hint
/// line when focused. Plus 2 for the block's own borders.
pub fn height(app: &App) -> u16 {
    let inner = if app.cache_block_expanded() {
        let hint = if app.is_active(Focus::Cache) && app.settings.show_hotkeys { 1 } else { 0 };
        app.cache_rows().len() + hint
    } else {
        1
    };
    inner as u16 + 2
}

/// The cache sizes block: a row per present source's cache size plus a total,
/// with `Enter` on a row queuing that source's clean (or all sources, on total).
pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let active = app.is_active(Focus::Cache);
    let border = crate::ui::border_color(app, Focus::Cache);
    let pal = &app.palette;
    let cursor = crate::ui::cursor_symbol(app);

    let mut lines = Vec::new();
    if app.cache_block_expanded() {
        for (i, row) in app.cache_rows().iter().enumerate() {
            let marker =
                if active && i == app.cache_selected { cursor.clone() } else { "  ".to_string() };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker}{:<8}", row.label), Style::default().fg(pal.fg)),
                Span::styled(row.text.clone(), Style::default().fg(pal.muted)),
            ]));
        }
        if active && app.settings.show_hotkeys {
            lines.push(Line::from(Span::styled(
                " \u{23ce} clean".to_string(),
                Style::default().fg(pal.muted),
            )));
        }
    } else {
        let total = app
            .cache_rows()
            .pop()
            .map(|r| r.text)
            .unwrap_or_else(|| "\u{2014}".into());
        lines.push(Line::from(vec![
            Span::styled("cache ".to_string(), Style::default().fg(pal.fg)),
            Span::styled(total, Style::default().fg(pal.muted).add_modifier(Modifier::BOLD)),
        ]));
    }

    let block = crate::ui::themed_block(app, border, " Cache ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}
