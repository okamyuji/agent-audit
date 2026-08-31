use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};

use crate::event::{decode_all, group_tool_calls, order_and_dedup, AuditEvent, ToolGroup};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Sessions,
    Timeline,
    Detail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Event(usize),
    Group { group: usize, collapsed: bool },
}

pub enum Msg {
    Sessions(Vec<String>),
    Events {
        session: String,
        raw: Vec<Vec<u8>>,
        next_offset: u64,
        replace: bool,
    },
    Error(String),
    Connected,
}

#[derive(Default)]
pub struct App {
    pub sessions: Vec<String>,
    pub selected_session: usize,
    pub events: Vec<AuditEvent>,
    pub groups: Vec<ToolGroup>,
    pub rows: Vec<Row>,
    pub selected_row: usize,
    pub detail_scroll: u16,
    pub focus: Focus,
    pub follow: bool,
    pub banner: Option<String>,
    pub skipped: usize,
    pub next_offset: u64,
    pub collapsed: HashSet<usize>,
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn session_name(&self) -> Option<&str> {
        self.sessions.get(self.selected_session).map(String::as_str)
    }

    pub fn selected_event(&self) -> Option<&AuditEvent> {
        match self.rows.get(self.selected_row)? {
            Row::Event(i) => self.events.get(*i),
            Row::Group { group, .. } => {
                let g = self.groups.get(*group)?;
                g.call
                    .or(g.response)
                    .or(g.result)
                    .and_then(|i| self.events.get(i))
            }
        }
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Sessions(mut names) => {
                // run- で始まるものはセッション不明なので末尾にまとめる
                names.sort_by(|a, b| (a.starts_with("run-"), a).cmp(&(b.starts_with("run-"), b)));
                let current = self.session_name().map(str::to_string);
                self.sessions = names;
                if let Some(c) = current {
                    if let Some(i) = self.sessions.iter().position(|s| *s == c) {
                        self.selected_session = i;
                    }
                }
                self.selected_session = self
                    .selected_session
                    .min(self.sessions.len().saturating_sub(1));
            }
            Msg::Events {
                session,
                raw,
                next_offset,
                replace,
            } => {
                if self.session_name() != Some(session.as_str()) {
                    return;
                }
                let (mut evs, stats) = decode_all(raw.iter().map(Vec::as_slice));
                if replace {
                    self.skipped = stats.skipped;
                } else {
                    self.skipped += stats.skipped;
                    evs.append(&mut self.events);
                }
                self.events = order_and_dedup(evs);
                self.next_offset = next_offset;
                self.rebuild_rows();
                if self.follow && !self.rows.is_empty() {
                    self.selected_row = self.rows.len() - 1;
                }
            }
            Msg::Error(e) => self.banner = Some(e),
            Msg::Connected => self.banner = None,
        }
    }

    fn rebuild_rows(&mut self) {
        self.groups = group_tool_calls(&self.events);
        let mut in_group = vec![None; self.events.len()];
        for (gi, g) in self.groups.iter().enumerate() {
            for i in [g.response, g.call, g.result].into_iter().flatten() {
                in_group[i] = Some(gi);
            }
        }
        let mut rows = Vec::new();
        let mut emitted_group = HashSet::new();
        for (i, _) in in_group.iter().enumerate() {
            match in_group[i] {
                None => rows.push(Row::Event(i)),
                Some(gi) => {
                    if emitted_group.insert(gi) {
                        rows.push(Row::Group {
                            group: gi,
                            collapsed: self.collapsed.contains(&gi),
                        });
                    }
                    if !self.collapsed.contains(&gi) {
                        rows.push(Row::Event(i));
                    }
                }
            }
        }
        self.rows = rows;
        self.selected_row = self.selected_row.min(self.rows.len().saturating_sub(1));
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('f') => self.follow = !self.follow,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Sessions => Focus::Timeline,
                    Focus::Timeline => Focus::Detail,
                    Focus::Detail => Focus::Sessions,
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(1),
            KeyCode::PageDown => self.move_down(10),
            KeyCode::PageUp => self.move_up(10),
            KeyCode::Enter if self.focus == Focus::Timeline => {
                if let Some(Row::Group { group, .. }) = self.rows.get(self.selected_row).cloned() {
                    if !self.collapsed.remove(&group) {
                        self.collapsed.insert(group);
                    }
                    self.rebuild_rows();
                }
            }
            _ => {}
        }
    }

    fn move_down(&mut self, n: usize) {
        match self.focus {
            Focus::Sessions => {
                self.selected_session =
                    (self.selected_session + n).min(self.sessions.len().saturating_sub(1));
            }
            Focus::Timeline => {
                self.selected_row = (self.selected_row + n).min(self.rows.len().saturating_sub(1));
                self.detail_scroll = 0;
            }
            Focus::Detail => self.detail_scroll = self.detail_scroll.saturating_add(n as u16),
        }
    }

    fn move_up(&mut self, n: usize) {
        match self.focus {
            Focus::Sessions => self.selected_session = self.selected_session.saturating_sub(n),
            Focus::Timeline => {
                self.selected_row = self.selected_row.saturating_sub(n);
                self.detail_scroll = 0;
            }
            Focus::Detail => self.detail_scroll = self.detail_scroll.saturating_sub(n as u16),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn raw_fixture(name: &str) -> Vec<Vec<u8>> {
        let raw = std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let vals: Vec<serde_json::Value> = serde_json::from_slice(&raw).unwrap();
        vals.iter().map(|v| v.to_string().into_bytes()).collect()
    }

    #[test]
    fn sessions_put_run_prefixed_last() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec![
            "run-abc".into(),
            "s2".into(),
            "s1".into(),
        ]));
        assert_eq!(app.sessions, vec!["s1", "s2", "run-abc"]);
    }

    #[test]
    fn events_build_rows_with_tool_group() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: raw_fixture("basic.json"),
            next_offset: 7,
            replace: true,
        });
        assert_eq!(app.events.len(), 7);
        assert_eq!(app.groups.len(), 1);
        assert!(app.rows.iter().any(|r| matches!(r, Row::Group { .. })));
        assert_eq!(app.next_offset, 7);
    }

    #[test]
    fn enter_toggles_group_collapse() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: raw_fixture("basic.json"),
            next_offset: 7,
            replace: true,
        });
        app.focus = Focus::Timeline;
        let group_row = app
            .rows
            .iter()
            .position(|r| matches!(r, Row::Group { .. }))
            .unwrap();
        app.selected_row = group_row;
        let before = app.rows.len();
        app.on_key(key(KeyCode::Enter));
        assert!(
            app.rows.len() < before,
            "collapsing hides the group's 3 events"
        );
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.rows.len(), before);
    }

    #[test]
    fn appended_events_merge_and_reorder() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        let all = raw_fixture("two_runs.json");
        let (first, rest) = all.split_at(2);
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: first.to_vec(),
            next_offset: 2,
            replace: true,
        });
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: rest.to_vec(),
            next_offset: 6,
            replace: false,
        });
        assert_eq!(app.events.len(), 5);
        let seqs: Vec<u64> = app.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 0, 1]);
    }

    #[test]
    fn error_sets_banner_and_connected_clears_it() {
        let mut app = App::new();
        app.apply(Msg::Error("down".into()));
        assert_eq!(app.banner.as_deref(), Some("down"));
        app.apply(Msg::Connected);
        assert!(app.banner.is_none());
    }

    #[test]
    fn q_quits_and_f_toggles_follow() {
        let mut app = App::new();
        app.on_key(key(KeyCode::Char('f')));
        assert!(app.follow);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }
}
