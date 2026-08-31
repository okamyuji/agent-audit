//! ターミナルのメインループ。`main.rs` から呼ばれる薄い実行部を除き、
//! ユニットテストから直接呼べるようライブラリ側に置く。

use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::Terminal;
use tokio::sync::{mpsc, watch};

use crate::app::{App, Msg};
use crate::ui;

/// キー入力の取得元。実端末に依存しない実装に差し替えられるようにして、
/// メインループ自体をユニットテストできるようにする
pub trait EventSource {
    fn poll_key(&mut self, timeout: Duration) -> anyhow::Result<Option<KeyEvent>>;
}

/// 実端末からキー入力を読む本番実装
pub struct CrosstermEvents;

impl EventSource for CrosstermEvents {
    fn poll_key(&mut self, timeout: Duration) -> anyhow::Result<Option<KeyEvent>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        read_key()
    }
}

fn read_key() -> anyhow::Result<Option<KeyEvent>> {
    Ok(classify_key(event::read()?))
}

/// キー押下イベントだけを取り出す（リリース/リピートやキー以外のイベントは無視）
fn classify_key(ev: Event) -> Option<KeyEvent> {
    let Event::Key(k) = ev else { return None };
    (k.kind == KeyEventKind::Press).then_some(k)
}

pub async fn run_ui<B, E>(
    term: &mut Terminal<B>,
    app: &mut App,
    rx: &mut mpsc::Receiver<Msg>,
    sel_tx: &watch::Sender<(Option<String>, bool)>,
    events: &mut E,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    E: EventSource,
{
    loop {
        term.draw(|f| ui::draw(f, app))?;
        drain_messages(app, rx);
        if let Some(k) = events.poll_key(Duration::from_millis(50))? {
            app.on_key(k);
        }
        publish_selection(app, sel_tx);
        if app.should_quit {
            return Ok(());
        }
    }
}

fn drain_messages(app: &mut App, rx: &mut mpsc::Receiver<Msg>) {
    while let Ok(m) = rx.try_recv() {
        app.apply(m);
    }
}

fn publish_selection(app: &App, sel_tx: &watch::Sender<(Option<String>, bool)>) {
    let want = (app.session_name().map(str::to_string), app.follow);
    if *sel_tx.borrow() != want {
        let _ = sel_tx.send(want);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Msg;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::backend::TestBackend;
    use std::collections::VecDeque;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn classify_key_keeps_only_key_press_events() {
        let press = key(KeyCode::Char('q'));
        assert_eq!(classify_key(Event::Key(press)), Some(press));

        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        assert_eq!(classify_key(Event::Key(release)), None);

        assert_eq!(
            classify_key(Event::Resize(10, 10)),
            None,
            "non-key events must be ignored"
        );
    }

    #[tokio::test]
    async fn drain_messages_applies_all_queued_messages() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(Msg::Error("boom".to_string())).await.unwrap();
        tx.send(Msg::Connected).await.unwrap();
        let mut app = App::new();
        drain_messages(&mut app, &mut rx);
        assert_eq!(app.banner, None, "Connected clears the banner set by Error");
        assert!(rx.try_recv().is_err(), "channel should be drained");
    }

    #[test]
    fn publish_selection_sends_only_when_changed() {
        let mut app = App::new();
        let (sel_tx, mut sel_rx) = watch::channel((None, false));

        publish_selection(&app, &sel_tx);
        assert!(
            !sel_rx.has_changed().unwrap(),
            "no send should happen when selection is unchanged"
        );

        app.follow = true;
        publish_selection(&app, &sel_tx);
        assert!(
            sel_rx.has_changed().unwrap(),
            "a send should happen when selection differs"
        );
        assert_eq!(*sel_rx.borrow_and_update(), (None, true));
    }

    /// テスト専用の擬似端末入力。台本の順に `None`（未入力）または `Some(key)` を返す
    struct FakeEvents(VecDeque<Option<KeyEvent>>);

    impl EventSource for FakeEvents {
        fn poll_key(&mut self, _timeout: Duration) -> anyhow::Result<Option<KeyEvent>> {
            // 台本が尽きたら意図的にエラーを返す。`run_ui` のループは `should_quit` に
            // しか依存しないため、'q' の処理が壊れているとここに到達するまで回り続ける。
            // `None` を返し続けると（実イベントが尽きた通常時の挙動）テストが無限ループ
            // し、ハングとして検出される（cargo-mutants ではタイムアウト扱いになり、
            // 遅く不安定な kill になる）。エラーにすることで `?` により `run_ui` から
            // 即座に Err が返り、`.unwrap()` が速く確実に失敗する
            match self.0.pop_front() {
                Some(v) => Ok(v),
                None => {
                    anyhow::bail!("FakeEvents script exhausted: run_ui did not quit as expected")
                }
            }
        }
    }

    #[tokio::test]
    async fn run_ui_applies_messages_dispatches_keys_and_quits_on_q() {
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut app = App::new();
        let (tx, mut rx) = mpsc::channel(4);
        let (sel_tx, _sel_rx) = watch::channel((None, false));

        tx.send(Msg::Sessions(vec!["s1".to_string()]))
            .await
            .unwrap();

        // 1回目: 入力なし。2回目: 'f' で追尾切替。3回目: 'q' で終了
        let mut events = FakeEvents(VecDeque::from([
            None,
            Some(key(KeyCode::Char('f'))),
            Some(key(KeyCode::Char('q'))),
        ]));

        run_ui(&mut term, &mut app, &mut rx, &sel_tx, &mut events)
            .await
            .unwrap();

        assert_eq!(
            app.sessions,
            vec!["s1".to_string()],
            "queued Msg must be applied"
        );
        assert!(app.follow, "'f' must toggle follow");
        assert!(app.should_quit, "'q' must set should_quit");
    }
}
