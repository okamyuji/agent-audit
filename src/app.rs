use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};

use crate::event::{decode_all, group_tool_calls, order_and_dedup, AuditEvent, ToolGroup};

fn collapse_key(g: &ToolGroup) -> (String, usize) {
    (g.call_id.clone(), g.attempt)
}

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

#[derive(Debug, PartialEq)]
pub enum Msg {
    Sessions(Vec<String>),
    Events {
        session: String,
        raw: Vec<Vec<u8>>,
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
    // グループの配列添字ではなく (call_id, attempt) をキーにする。添字は再整列や
    // セッション切替のたびに変わるため、添字キーだと折り畳みが別のグループへ
    // 移ってしまう。
    pub collapsed: HashSet<(String, usize)>,
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
                    let key = collapse_key(&self.groups[gi]);
                    let collapsed = self.collapsed.contains(&key);
                    if emitted_group.insert(gi) {
                        rows.push(Row::Group {
                            group: gi,
                            collapsed,
                        });
                    }
                    if !collapsed {
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
            KeyCode::Tab => self.cycle_focus(),
            KeyCode::Char('j') | KeyCode::Down => self.move_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(1),
            KeyCode::PageDown => self.move_down(10),
            KeyCode::PageUp => self.move_up(10),
            KeyCode::Enter => self.toggle_collapse_if_timeline(),
            _ => {}
        }
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Sessions => Focus::Timeline,
            Focus::Timeline => Focus::Detail,
            Focus::Detail => Focus::Sessions,
        };
    }

    /// Timeline フォーカス時、選択中の行がグループならその折り畳みを切り替える
    fn toggle_collapse_if_timeline(&mut self) {
        if self.focus != Focus::Timeline {
            return;
        }
        let Some(Row::Group { group, .. }) = self.rows.get(self.selected_row).cloned() else {
            return;
        };
        let key = collapse_key(&self.groups[group]);
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild_rows();
    }

    fn move_down(&mut self, n: usize) {
        match self.focus {
            Focus::Sessions => {
                let next = (self.selected_session + n).min(self.sessions.len().saturating_sub(1));
                self.select_session(next);
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
            Focus::Sessions => {
                let next = self.selected_session.saturating_sub(n);
                self.select_session(next);
            }
            Focus::Timeline => {
                self.selected_row = self.selected_row.saturating_sub(n);
                self.detail_scroll = 0;
            }
            Focus::Detail => self.detail_scroll = self.detail_scroll.saturating_sub(n as u16),
        }
    }

    /// セッション選択が実際に変わる時だけ、前のセッションのタイムラインを消す。
    /// 消さないと、新セッション名の下に前セッションのイベントが最初の500msの
    /// 読み込みが届くまで表示され続ける（Msg::Sessionsでの再選択・同名再選択は
    /// 対象外 — こちらはセッション一覧の定期更新であって切替ではない）。
    fn select_session(&mut self, idx: usize) {
        if idx == self.selected_session {
            return;
        }
        self.selected_session = idx;
        self.events.clear();
        self.groups.clear();
        self.rows.clear();
        self.selected_row = 0;
        self.detail_scroll = 0;
        self.skipped = 0;
        self.collapsed.clear();
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

    fn ev(seq: u64, ts: &str, run_id: &str, kind: &str, call_id: Option<&str>) -> Vec<u8> {
        let payload = match kind {
            "llm_request" => serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
            "llm_response" => serde_json::json!({"content": "ok"}),
            "tool_call" => serde_json::json!({"name": "t"}),
            "tool_result" => {
                serde_json::json!({"name": "t", "content": "r", "is_error": false, "duration_ms": 1})
            }
            _ => panic!("unknown kind"),
        };
        let mut v = serde_json::json!({
            "v": 1,
            "id": format!("id-{run_id}-{seq}"),
            "session_id": "s1",
            "run_id": run_id,
            "seq": seq,
            "ts": ts,
            "kind": kind,
            "payload": payload,
        });
        if let Some(cid) = call_id {
            v["call_id"] = serde_json::Value::String(cid.to_string());
        }
        v.to_string().into_bytes()
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
            replace: true,
        });
        assert_eq!(app.events.len(), 7);
        assert_eq!(app.groups.len(), 1);
        assert!(app.rows.iter().any(|r| matches!(r, Row::Group { .. })));
    }

    #[test]
    fn enter_toggles_group_collapse() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: raw_fixture("basic.json"),
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
            replace: true,
        });
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: rest.to_vec(),
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

    #[test]
    fn j_k_pagedown_pageup_move_session_selection() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into(), "s2".into(), "s3".into()]));
        assert_eq!(app.focus, Focus::Sessions);
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_session, 1);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected_session, 2);
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.selected_session, 2, "capped at the last session");
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_session, 1);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected_session, 0);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.selected_session, 0, "capped at 0");
    }

    #[test]
    fn tab_cycles_focus_and_timeline_detail_navigation_moves_rows_and_scroll() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: raw_fixture("basic.json"),
            replace: true,
        });
        assert_eq!(app.focus, Focus::Sessions);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Timeline);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Detail);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Sessions);

        app.focus = Focus::Timeline;
        let last_row = app.rows.len() - 1;
        assert!(last_row >= 1, "fixture must produce at least 2 rows");
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.selected_row, last_row, "capped at the last row");
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_row, last_row - 1);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.selected_row, 0, "capped at 0");
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_row, 1);

        app.focus = Focus::Detail;
        app.detail_scroll = 5;
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.detail_scroll, 6);
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.detail_scroll, 16);
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.detail_scroll, 15);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.detail_scroll, 5);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.detail_scroll, 4);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.detail_scroll, 5);
    }

    #[test]
    fn enter_is_noop_outside_timeline_or_on_non_group_row_selected_event_resolves_both_row_kinds() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: raw_fixture("basic.json"),
            replace: true,
        });

        // Enter に反応するのは Timeline フォーカス時のみ
        app.focus = Focus::Sessions;
        let before = app.rows.clone();
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            before, app.rows,
            "Sessions フォーカスでは Enter は無視される"
        );

        app.focus = Focus::Timeline;
        let event_row = app
            .rows
            .iter()
            .position(|r| matches!(r, Row::Event(_)))
            .expect("fixture must contain a plain event row");
        app.selected_row = event_row;
        let before = app.rows.clone();
        app.on_key(key(KeyCode::Enter));
        assert_eq!(before, app.rows, "Event 行では Enter は何もしない");
        assert!(
            app.selected_event().is_some(),
            "Row::Event が指すイベントが返る"
        );

        let group_row = app
            .rows
            .iter()
            .position(|r| matches!(r, Row::Group { .. }))
            .expect("fixture must contain a group row");
        app.selected_row = group_row;
        assert!(
            app.selected_event().is_some(),
            "Row::Group でも代表イベントが返る"
        );
    }

    #[test]
    fn collapse_state_survives_a_reorder_by_call_id_and_attempt() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into()]));
        // run-a: 1 tool group (call_id "c1")
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: vec![
                ev(
                    0,
                    "2025-01-01T10:00:05Z",
                    "run-a",
                    "llm_response",
                    Some("c1"),
                ),
                ev(1, "2025-01-01T10:00:06Z", "run-a", "tool_call", Some("c1")),
                ev(
                    2,
                    "2025-01-01T10:00:07Z",
                    "run-a",
                    "tool_result",
                    Some("c1"),
                ),
            ],
            replace: true,
        });
        app.focus = Focus::Timeline;
        let group_row = app
            .rows
            .iter()
            .position(|r| matches!(r, Row::Group { .. }))
            .unwrap();
        app.selected_row = group_row;
        app.on_key(key(KeyCode::Enter)); // collapse c1/attempt 1
        assert_eq!(app.collapsed.len(), 1);

        // run-b arrives with an earlier first_ts, so order_and_dedup moves it to the
        // front and the c1 group's array index shifts.
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: vec![
                ev(
                    0,
                    "2025-01-01T10:00:00Z",
                    "run-b",
                    "llm_response",
                    Some("c2"),
                ),
                ev(1, "2025-01-01T10:00:01Z", "run-b", "tool_call", Some("c2")),
            ],
            replace: false,
        });

        let g = app
            .rows
            .iter()
            .find_map(|r| match r {
                Row::Group { group, collapsed } if app.groups[*group].call_id == "c1" => {
                    Some(*collapsed)
                }
                _ => None,
            })
            .expect("c1 group still present after reorder");
        assert!(g, "c1's collapse state must survive the reorder");
    }

    #[test]
    fn collapse_state_does_not_leak_across_session_switch() {
        let mut app = App::new();
        app.apply(Msg::Sessions(vec!["s1".into(), "s2".into()]));
        app.apply(Msg::Events {
            session: "s1".into(),
            raw: raw_fixture("basic.json"),
            replace: true,
        });
        app.focus = Focus::Timeline;
        let group_row = app
            .rows
            .iter()
            .position(|r| matches!(r, Row::Group { .. }))
            .unwrap();
        app.selected_row = group_row;
        app.on_key(key(KeyCode::Enter)); // collapse the group in s1
        assert_eq!(app.collapsed.len(), 1);
        assert!(!app.events.is_empty());

        // Switch to s2 via the Sessions pane (j)
        app.focus = Focus::Sessions;
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_session, 1);
        assert!(
            app.collapsed.is_empty(),
            "collapse state must not carry over to the new session"
        );
        assert!(
            app.events.is_empty(),
            "stale events must not render under s2"
        );
        assert!(app.rows.is_empty());
        assert!(app.groups.is_empty());
    }
}
