//! 実 Iggy コンテナに対して主要 6 導線を通す E2E。端末は TestBackend で代替し、
//! キー入力は App::on_key へ直接渡す。tests/iggy_integration.rs と同じ固定ポート
//! （host network）を使うため、cargo test がテストバイナリを直列実行することに
//! 依存して同時起動を避けている。

use std::process::Command;
use std::time::{Duration, Instant};

use agent_audit::app::{App, Msg};
use agent_audit::iggy::testsupport::{start_iggy, ContainerGuard};
use agent_audit::iggy::IggyBackend;
use agent_audit::runtime::{network_loop, Cli};
use agent_audit::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};
use tokio::sync::{mpsc, watch};

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

/// 日本語などの全角文字は2セル幅で描画され、2セル目は幅合わせ用のプレースホルダ
/// （空白）になる。素直に連結すると文字の間に余分な空白が入るので、
/// ui.rs のテスト用 render() と同じくプレースホルダをスキップする。
/// セッション一覧は画面幅の20%しかないため、幅は「run-<uuid>（セッション不明）」
/// がその列に収まる余裕を持って表示できる 200 桁にする
fn render(app: &App) -> String {
    use unicode_width::UnicodeWidthStr;

    let mut term = Terminal::new(TestBackend::new(200, 40)).unwrap();
    term.draw(|f| ui::draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    let width = buf.area.width as usize;
    let mut s = String::new();
    let mut prev_was_wide = false;
    for (idx, cell) in buf.content().iter().enumerate() {
        let symbol = cell.symbol();
        if symbol == " " && prev_was_wide {
            prev_was_wide = false;
            continue;
        }
        s.push_str(symbol);
        prev_was_wide = symbol.width() > 1;
        if (idx + 1) % width == 0 {
            s.push('\n');
            prev_was_wide = false;
        }
    }
    s
}

fn fixture_lines(name: &str) -> Vec<Vec<u8>> {
    let raw = std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let vals: Vec<serde_json::Value> = serde_json::from_slice(&raw).unwrap();
    vals.iter().map(|v| v.to_string().into_bytes()).collect()
}

/// rx から届く Msg を app に流し込みながら、cond が真になるまで最大 timeout 待つ
async fn pump_until(
    app: &mut App,
    rx: &mut mpsc::Receiver<Msg>,
    timeout: Duration,
    mut cond: impl FnMut(&App) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Ok(m) = rx.try_recv() {
            app.apply(m);
        }
        if cond(app) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn main_flows_end_to_end() -> anyhow::Result<()> {
    if !docker_available() {
        eprintln!("skip: docker not available");
        return Ok(());
    }
    let (addr, pat, container) = start_iggy().await?;
    let _guard = ContainerGuard::new(container.clone());
    let stream = "agent-audit-e2e";
    let be = IggyBackend::connect(&addr, stream, &pat, false).await?;
    be.ensure_stream_for_test().await?;
    be.produce_for_test("s-alpha", &fixture_lines("basic.json"))
        .await?;
    // セッション一覧ペインは画面幅の20%しかなく、フル UUID + "（セッション不明）"
    // は収まらないので8桁の短縮形にする（一意性は保ったまま）
    let orphan = format!("run-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    be.produce_for_test(&orphan, &fixture_lines("truncated.json"))
        .await?;

    let (tx, mut rx) = mpsc::channel::<Msg>(64);
    let (sel_tx, sel_rx) = watch::channel((None::<String>, false));
    tokio::spawn(network_loop(
        Cli {
            iggy_addr: addr.clone(),
            stream: stream.into(),
            tls: false,
        },
        pat.clone(),
        tx,
        sel_rx,
    ));
    let mut app = App::new();

    // 起動→一覧
    assert!(
        pump_until(&mut app, &mut rx, Duration::from_secs(10), |a| a
            .sessions
            .len()
            == 2)
        .await
    );
    assert_eq!(app.sessions[0], "s-alpha");
    assert!(app.sessions[1].starts_with("run-"));
    assert!(render(&app).contains("セッション不明"));

    // 選択→タイムライン
    sel_tx.send((Some("s-alpha".into()), false))?;
    assert!(
        pump_until(&mut app, &mut rx, Duration::from_secs(10), |a| a
            .events
            .len()
            == 7)
        .await
    );
    assert_eq!(app.groups.len(), 1);

    // 折りたたみ
    app.focus = agent_audit::app::Focus::Timeline;
    app.selected_row = app
        .rows
        .iter()
        .position(|r| matches!(r, agent_audit::app::Row::Group { .. }))
        .unwrap();
    let rows_before = app.rows.len();
    app.on_key(key(KeyCode::Enter));
    assert!(app.rows.len() < rows_before);
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.rows.len(), rows_before);

    // 詳細スクロール
    app.on_key(key(KeyCode::Tab));
    let before = render(&app);
    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(app.detail_scroll, 1);
    assert_ne!(before, render(&app));

    // 追尾
    app.on_key(key(KeyCode::Char('f')));
    sel_tx.send((Some("s-alpha".into()), true))?;
    be.produce_for_test("s-alpha", &fixture_lines("two_runs.json"))
        .await?;
    assert!(
        pump_until(&mut app, &mut rx, Duration::from_secs(5), |a| a
            .events
            .len()
            > 7)
        .await
    );
    assert_eq!(app.selected_row, app.rows.len() - 1);

    // 不達→再接続
    Command::new("docker").args(["stop", &container]).status()?;
    assert!(
        pump_until(&mut app, &mut rx, Duration::from_secs(15), |a| a
            .banner
            .is_some())
        .await
    );
    assert!(render(&app).contains("接続できません") || render(&app).contains("失敗"));
    Command::new("docker")
        .args(["start", &container])
        .status()?;
    assert!(
        pump_until(&mut app, &mut rx, Duration::from_secs(40), |a| a
            .banner
            .is_none())
        .await
    );

    Ok(())
}
