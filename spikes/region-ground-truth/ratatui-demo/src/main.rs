//! Plain ratatui app; regions come from the patched ratatui-core/widgets.
//! The only region-specific code is the optional `regions::label` calls.
//! Draws ~12 frames unattended (or quit with `q`).
use std::time::Duration;

use ratatui::{
    crossterm::event::{self, Event, KeyCode},
    layout::{Constraint, Flex, Layout, Margin, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListState, Paragraph},
    Frame,
};
use ratatui_core::regions::label;

fn popup_area(area: Rect, w: u16, h: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(row);
    cell
}

fn draw(frame: &mut Frame, tick: usize, list: &mut ListState) {
    let outer = Block::bordered().title(" cleat region demo ");
    let body = outer.inner(frame.area());
    label("app");
    frame.render_widget(outer, frame.area());

    let [main, input_row] = Layout::vertical([Constraint::Min(5), Constraint::Length(5)]).areas(body);
    let [left, right] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(main);

    let items = ["alpha.rs", "beta.rs", "gamma.rs", "delta.rs", "epsilon.rs", "zeta.rs"];
    let files = List::new(items)
        .block(Block::bordered().title(" Files "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    list.select(Some(tick % items.len()));
    label("files");
    frame.render_stateful_widget(files, left, list);

    // Nested blocks: Preview contains Details contains a paragraph.
    let preview = Block::bordered().title(" Preview ");
    let preview_inner = preview.inner(right);
    label("preview");
    frame.render_widget(preview, right);
    let [summary, details_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(preview_inner);
    frame.render_widget(Paragraph::new(format!("selected: {}", items[tick % items.len()])), summary);
    let details = Block::new().borders(Borders::ALL).title(" Details ");
    let details_inner = details.inner(details_area);
    label("details");
    frame.render_widget(details, details_area);
    frame.render_widget(Paragraph::new(format!("frame {tick}\nnested two levels deep")), details_inner);

    // Inset input box: does not span full rows.
    let input_area = input_row.inner(Margin::new(4, 1));
    label("composer");
    frame.render_widget(Paragraph::new(format!("> type here {}", "_".repeat(tick % 5))).block(Block::bordered().title(" Input ")), input_area);

    if (4..9).contains(&tick) {
        let area = popup_area(frame.area(), 30, 5);
        frame.render_widget(Clear, area);
        label("confirm");
        frame.render_widget(Paragraph::new("Overwrite file?  [y/N]").block(Block::bordered().title(" Confirm ")), area);
    }
}

fn main() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let mut list = ListState::default();
    let mut result = Ok(());
    for tick in 0..12 {
        if let Err(e) = terminal.draw(|f| draw(f, tick, &mut list)) {
            result = Err(e);
            break;
        }
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.code == KeyCode::Char('q') {
                    break;
                }
            }
        }
    }
    ratatui::restore();
    result
}
