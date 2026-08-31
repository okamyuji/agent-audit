use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::Paragraph,
    Frame,
};

use crate::app::App;
use crate::event::{is_truncated, AuditEvent, Kind};

/// (バナー, セッション, タイムライン, 詳細)
pub fn layout(area: Rect) -> (Rect, Rect, Rect, Rect) {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(40),
            Constraint::Percentage(40),
        ])
        .split(v[1]);
    (v[0], h[0], h[1], h[2])
}

pub fn draw(f: &mut Frame, app: &App) {
    let (banner, sess, tl, det) = layout(f.area());
    let text = match &app.banner {
        Some(b) => format!(" {b} "),
        None => format!(
            " {}件のイベント{}{} ",
            app.events.len(),
            if app.skipped > 0 {
                format!("（スキップ{}件）", app.skipped)
            } else {
                String::new()
            },
            if app.follow { "  追尾中" } else { "" }
        ),
    };
    let style = if app.banner.is_some() {
        Style::default().bg(Color::Red).fg(Color::White)
    } else {
        Style::default()
    };
    f.render_widget(Paragraph::new(text).style(style), banner);
    sessions_draw(f, app, sess);
    timeline_draw(f, app, tl);
    detail_draw(f, app, det);
}

fn sessions_draw(f: &mut Frame, app: &App, area: Rect) {
    use ratatui::{
        style::Modifier,
        widgets::{Block, Borders, List, ListItem, ListState},
    };

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            ListItem::new(if s.starts_with("run-") {
                format!("{s}（セッション不明）")
            } else {
                s.clone()
            })
        })
        .collect();
    let title = if app.focus == crate::app::Focus::Sessions {
        "セッション *"
    } else {
        "セッション"
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(app.selected_session));
    f.render_stateful_widget(list, area, &mut state);
}

fn timeline_draw(f: &mut Frame, app: &App, area: Rect) {
    use ratatui::{
        style::Modifier,
        widgets::{Block, Borders, List, ListItem, ListState},
    };

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|r| match r {
            crate::app::Row::Event(i) => {
                ListItem::new(format!("  {}", event_summary(&app.events[*i])))
            }
            crate::app::Row::Group { group, collapsed } => {
                let g = &app.groups[*group];
                let mark = if *collapsed { "▸" } else { "▾" };
                let status = if g.result.is_none() {
                    "  結果待ち"
                } else {
                    ""
                };
                let attempt = if g.attempt > 1 {
                    format!(" 試行{}", g.attempt)
                } else {
                    String::new()
                };
                ListItem::new(format!("{mark} ツール呼出 {}{attempt}{status}", g.call_id))
            }
        })
        .collect();
    let title = if app.focus == crate::app::Focus::Timeline {
        "タイムライン *"
    } else {
        "タイムライン"
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(app.selected_row));
    f.render_stateful_widget(list, area, &mut state);
}

fn detail_draw(f: &mut Frame, app: &App, area: Rect) {
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

    let body = app.selected_event().map(event_detail).unwrap_or_default();
    let title = if app.focus == crate::app::Focus::Detail {
        "詳細 *"
    } else {
        "詳細"
    };
    let p = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    f.render_widget(p, area);
}

pub fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::LlmRequest => "llm_request",
        Kind::LlmResponse => "llm_response",
        Kind::ToolCall => "tool_call",
        Kind::ToolResult => "tool_result",
        Kind::Usage => "usage",
    }
}

pub fn event_summary(e: &AuditEvent) -> String {
    let ts = e.ts.format("%H:%M:%S%.3f");
    let mut s = format!("{ts} {:<12}", kind_label(e.kind));
    if let Some(n) = is_truncated(&e.payload) {
        s.push_str(&format!(" 切り詰め（{n}バイト）"));
        return s;
    }
    match e.kind {
        Kind::ToolCall | Kind::ToolResult => {
            if let Some(name) = e.payload.get("name").and_then(|v| v.as_str()) {
                s.push_str(&format!(" {name}"));
            }
            if e.payload.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                s.push_str(" [error]");
            }
        }
        Kind::LlmResponse => {
            if let Some(tc) = e
                .payload
                .get("tool_call")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
            {
                s.push_str(&format!(" → {tc}"));
            }
        }
        _ => {}
    }
    s
}

pub fn event_detail(e: &AuditEvent) -> String {
    if let Some(n) = is_truncated(&e.payload) {
        return format!("切り詰め（{n}バイト）。全文は送信元の WAL に残っています。");
    }
    serde_json::to_string_pretty(&e.payload).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Msg;
    use ratatui::{backend::TestBackend, Terminal};

    fn raw_fixture(name: &str) -> Vec<Vec<u8>> {
        let raw = std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let vals: Vec<serde_json::Value> = serde_json::from_slice(&raw).unwrap();
        vals.iter().map(|v| v.to_string().into_bytes()).collect()
    }

    fn render(app: &App) -> String {
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut s = String::new();
        let mut prev_was_wide = false;
        for (idx, cell) in buf.content().iter().enumerate() {
            let symbol = cell.symbol();
            // Skip space cells that follow multi-byte characters (width placeholders)
            if symbol == " " && prev_was_wide {
                prev_was_wide = false;
                continue;
            }
            // Check if this is a newline position
            if (idx + 1) % 120 == 0 {
                s.push('\n');
                prev_was_wide = false;
            } else {
                s.push_str(symbol);
                prev_was_wide = symbol.len() > 1;
            }
        }
        s
    }

    #[test]
    fn layout_reserves_banner_row() {
        let (banner, sessions, timeline, detail) =
            layout(ratatui::layout::Rect::new(0, 0, 120, 40));
        assert_eq!(banner.height, 1);
        assert!(sessions.width > 0 && timeline.width > 0 && detail.width > 0);
    }

    #[test]
    fn renders_sessions_timeline_and_truncated_marker() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: raw_fixture("truncated.json"),
            next_offset: 1,
            replace: true,
        });
        let s = render(&app);
        assert!(s.contains("s1"));
        assert!(s.contains("llm_request"));
        assert!(s.contains("切り詰め"), "{s}");
    }

    #[test]
    fn renders_banner_and_waiting_result() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        let mut raw = raw_fixture("basic.json");
        raw.retain(|r| !String::from_utf8_lossy(r).contains("\"tool_result\""));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw,
            next_offset: 6,
            replace: true,
        });
        app.apply(Msg::Error("Iggyに接続できません".into()));
        let s = render(&app);
        assert!(s.contains("Iggyに接続できません"));
        assert!(s.contains("結果待ち"), "{s}");
    }
}
