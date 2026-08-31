use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
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

/// 詳細ペインに渡す前に制御文字（`\x1b` を含む）を落とす。ここは信頼できない
/// 本文（LLM/ツールの出力）が端末へ渡る境界なので、エスケープシーケンスの
/// 素通しは避ける。改行は複数行本文の表示に要るので残す。
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

pub fn event_detail(e: &AuditEvent) -> String {
    if let Some(n) = is_truncated(&e.payload) {
        return format!("切り詰め（{n}バイト）。全文は送信元の WAL に残っています。");
    }
    match e.kind {
        Kind::LlmRequest => {
            if let Some(messages) = e.payload.get("messages").and_then(|v| v.as_array()) {
                let mut out = String::new();
                for m in messages {
                    let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
                    let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    out.push_str(&sanitize(&format!("{role}: {content}\n")));
                }
                return out;
            }
        }
        Kind::LlmResponse | Kind::ToolResult => {
            if let Some(content) = e.payload.get("content").and_then(|v| v.as_str()) {
                return sanitize(content);
            }
        }
        _ => {}
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
        use unicode_width::UnicodeWidthStr;

        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut s = String::new();
        let width = buf.area.width as usize;
        let mut prev_was_wide = false;
        for (idx, cell) in buf.content().iter().enumerate() {
            let symbol = cell.symbol();
            // Skip space cells following multi-byte characters (width placeholders)
            if symbol == " " && prev_was_wide {
                prev_was_wide = false;
                continue;
            }
            s.push_str(symbol);
            prev_was_wide = symbol.width() > 1;
            // Add newline at end of each row
            if (idx + 1) % width == 0 {
                s.push('\n');
                prev_was_wide = false;
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
            replace: true,
        });
        app.apply(Msg::Error("Iggyに接続できません".into()));
        let s = render(&app);
        assert!(s.contains("Iggyに接続できません"));
        assert!(s.contains("結果待ち"), "{s}");
    }

    #[test]
    fn detail_pane_renders_multiline_content_as_real_line_breaks() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: raw_fixture("multiline.json"),
            replace: true,
        });
        // selected_row defaults to 0: the llm_request event
        let s = render(&app);
        assert!(s.contains("user: line one"), "{s}");
        // real line breaks put "line two" and "line three" on their own terminal
        // rows, not appended after "line one" on the same row
        let row_with_line_one = s
            .lines()
            .find(|l| l.contains("user: line one"))
            .expect("line one must be rendered");
        assert!(!row_with_line_one.contains("line two"), "{s}");
        assert!(s.contains("line two"), "{s}");
        assert!(s.contains("line three"), "{s}");
        assert!(
            !s.contains("line one\\nline two"),
            "content must not stay as an escaped single line: {s}"
        );
    }

    #[test]
    fn detail_pane_renders_llm_response_content_as_raw_text() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: raw_fixture("multiline.json"),
            replace: true,
        });
        app.focus = crate::app::Focus::Timeline;
        app.selected_row = 1; // the llm_response event
        let s = render(&app);
        assert!(s.contains("answer one"), "{s}");
        assert!(s.contains("answer two"), "{s}");
        let row_with_answer_one = s
            .lines()
            .find(|l| l.contains("answer one"))
            .expect("answer one must be rendered");
        assert!(!row_with_answer_one.contains("answer two"), "{s}");
    }
}
