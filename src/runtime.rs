//! CLI 定義とネットワークタスク。`main.rs` から呼ばれる薄い実行部を除き、
//! 結合テストから直接呼べるようライブラリ側に置く。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clap::Parser;
use tokio::sync::{mpsc, watch};

use crate::app::Msg;
use crate::iggy::{fetch_all, Backend, IggyBackend};

#[derive(Parser, Debug)]
#[command(
    name = "agent-audit",
    version,
    about = "Replay viewer for go-llm-agent audit events in Apache Iggy"
)]
pub struct Cli {
    /// Iggy の TCP アドレス
    #[arg(long, default_value = "127.0.0.1:8090")]
    pub iggy_addr: String,
    /// stream 名
    #[arg(long, default_value = "agent-audit")]
    pub stream: String,
    /// TLS で接続する（ループバック以外のアドレスでは必須）
    #[arg(long)]
    pub tls: bool,
}

/// PAT を平文で送らないための事前検査。TLS か、ループバック宛だけを許す
pub fn validate_transport(addr: &str, tls: bool) -> anyhow::Result<()> {
    if tls {
        return Ok(());
    }
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    let host = host.trim_matches(|c| c == '[' || c == ']');
    let is_loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if is_loopback {
        return Ok(());
    }
    anyhow::bail!("{addr} は平文 TCP のリモート宛です。--tls を付けるか 127.0.0.1 を使ってください")
}

/// 再接続バックオフの次の待ち時間。1 秒から倍増し、30 秒で頭打ちにする
pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(Duration::from_secs(30))
}

/// Iggy への接続を抽象化する。テストでは実ネットワークなしに接続結果を差し替えられる。
#[async_trait]
trait Connector: Send + Sync {
    async fn connect(&self) -> anyhow::Result<Arc<dyn Backend>>;
}

struct IggyConnector {
    addr: String,
    stream: String,
    pat: String,
    tls: bool,
}

#[async_trait]
impl Connector for IggyConnector {
    async fn connect(&self) -> anyhow::Result<Arc<dyn Backend>> {
        let be = IggyBackend::connect(&self.addr, &self.stream, &self.pat, self.tls).await?;
        Ok(Arc::new(be))
    }
}

/// Iggy への1呼び出しに許す上限時間。この crate の TCP クライアントは接続先が
/// 突然消えても読み取りが OS の再送タイムアウト任せになり、実測で数十秒以上
/// ハングし続けることを確認したため、明示的なタイムアウトで区切って
/// エラー経路（再接続・バナー表示）に落とす
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

async fn with_timeout<T>(
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(CALL_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!("応答がありません（タイムアウト）"),
    }
}

/// 内側のセッションループの終了理由
#[derive(Debug, PartialEq)]
enum LoopExit {
    /// 再接続して継続する
    Reconnect,
    /// アプリ終了（selection チャンネルが閉じた）
    Shutdown,
}

/// ネットワークタスク。セッション一覧を 5 秒ごと、選択セッションの末尾を追尾中は 500ms ごとに読む。
/// 不達時は 1 秒から倍増（最大 30 秒）で再接続する
pub async fn network_loop(
    cli: Cli,
    pat: String,
    tx: mpsc::Sender<Msg>,
    sel: watch::Receiver<(Option<String>, bool)>,
) {
    let connector = IggyConnector {
        addr: cli.iggy_addr,
        stream: cli.stream,
        pat,
        tls: cli.tls,
    };
    run_network_loop(&connector, tx, sel).await
}

async fn run_network_loop(
    connector: &dyn Connector,
    tx: mpsc::Sender<Msg>,
    mut sel: watch::Receiver<(Option<String>, bool)>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let be = match connector.connect().await {
            Ok(b) => b,
            Err(e) => {
                let _ = tx
                    .send(Msg::Error(format!(
                        "Iggyに接続できません: {e:#}（{}秒後に再試行）",
                        backoff.as_secs()
                    )))
                    .await;
                tokio::time::sleep(backoff).await;
                backoff = next_backoff(backoff);
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        let _ = tx.send(Msg::Connected).await;
        match run_session(be, &tx, &mut sel).await {
            LoopExit::Shutdown => return,
            LoopExit::Reconnect => continue,
        }
    }
}

/// 1回の接続でのセッション読み込みループ。エラーで `Reconnect`、
/// selection チャンネルが閉じたら `Shutdown` を返す
async fn run_session(
    be: Arc<dyn Backend>,
    tx: &mpsc::Sender<Msg>,
    sel: &mut watch::Receiver<(Option<String>, bool)>,
) -> LoopExit {
    let mut sessions_tick = tokio::time::interval(Duration::from_secs(5));
    let mut follow_tick = tokio::time::interval(Duration::from_millis(500));
    let mut loaded: Option<(String, u64)> = None;
    loop {
        tokio::select! {
            _ = sessions_tick.tick() => {
                match with_timeout(be.list_sessions()).await {
                    Ok(s) => { let _ = tx.send(Msg::Sessions(s)).await; }
                    Err(e) => {
                        let _ = tx.send(Msg::Error(format!("一覧取得に失敗: {e:#}"))).await;
                        return LoopExit::Reconnect;
                    }
                }
            }
            _ = follow_tick.tick() => {
                let (session, follow) = sel.borrow().clone();
                let Some(session) = session else { continue };
                let need_full = loaded.as_ref().map(|(s, _)| s != &session).unwrap_or(true);
                if need_full {
                    match fetch_all(be.as_ref(), &session, CALL_TIMEOUT).await {
                        Ok((raw, next)) => {
                            loaded = Some((session.clone(), next));
                            let _ = tx.send(Msg::Events { session, raw, replace: true }).await;
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::Error(format!("読み込みに失敗: {e:#}"))).await;
                            return LoopExit::Reconnect;
                        }
                    }
                } else if follow {
                    let (_, off) = loaded.clone().unwrap();
                    match with_timeout(be.fetch_from(&session, off)).await {
                        Ok((raw, next)) if !raw.is_empty() => {
                            loaded = Some((session.clone(), next));
                            let _ = tx.send(Msg::Events { session, raw, replace: false }).await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = tx.send(Msg::Error(format!("追尾に失敗: {e:#}"))).await;
                            return LoopExit::Reconnect;
                        }
                    }
                }
            }
            changed = sel.changed() => { if changed.is_err() { return LoopExit::Shutdown; } }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[test]
    fn transport_requires_tls_unless_loopback() {
        for (addr, tls, ok) in [
            ("127.0.0.1:8090", false, true),
            ("localhost:8090", false, true),
            ("[::1]:8090", false, true),
            ("10.0.0.5:8090", false, false),
            ("iggy.example.com:8090", false, false),
            ("iggy.example.com:8090", true, true),
        ] {
            assert_eq!(
                validate_transport(addr, tls).is_ok(),
                ok,
                "{addr} tls={tls}"
            );
        }
    }

    #[test]
    fn cli_parses_defaults() {
        let cli = Cli::parse_from(["agent-audit"]);
        assert_eq!(cli.iggy_addr, "127.0.0.1:8090");
        assert_eq!(cli.stream, "agent-audit");
    }

    #[test]
    fn cli_parses_overrides() {
        let cli = Cli::parse_from([
            "agent-audit",
            "--iggy-addr",
            "10.0.0.1:9000",
            "--stream",
            "other",
        ]);
        assert_eq!(cli.iggy_addr, "10.0.0.1:9000");
        assert_eq!(cli.stream, "other");
    }

    #[test]
    fn backoff_doubles_then_caps_at_30s() {
        let mut b = Duration::from_secs(1);
        for expected in [2, 4, 8, 16, 30, 30, 30] {
            b = next_backoff(b);
            assert_eq!(b, Duration::from_secs(expected));
        }
    }

    type ListResult = Result<Vec<String>, String>;
    type FetchResult = Result<(Vec<Vec<u8>>, u64), String>;

    /// list_sessions / fetch_from の返答を順に払い出す。台本が尽きたら無害な既定値
    /// （空一覧・空バッチ）を返し、無関係な tick で誤ってエラー扱いにならないようにする。
    #[derive(Default)]
    struct ScriptedBackend {
        list_script: Mutex<VecDeque<ListResult>>,
        fetch_script: Mutex<VecDeque<FetchResult>>,
    }

    impl ScriptedBackend {
        fn with_list(self, r: ListResult) -> Self {
            self.list_script.lock().unwrap().push_back(r);
            self
        }
        fn with_fetch(self, r: FetchResult) -> Self {
            self.fetch_script.lock().unwrap().push_back(r);
            self
        }
    }

    #[async_trait]
    impl Backend for ScriptedBackend {
        async fn list_sessions(&self) -> anyhow::Result<Vec<String>> {
            match self.list_script.lock().unwrap().pop_front() {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) => Err(anyhow::anyhow!(e)),
                None => Ok(vec![]),
            }
        }
        async fn fetch_from(
            &self,
            _session: &str,
            _offset: u64,
        ) -> anyhow::Result<(Vec<Vec<u8>>, u64)> {
            match self.fetch_script.lock().unwrap().pop_front() {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) => Err(anyhow::anyhow!(e)),
                None => Ok((vec![], 0)),
            }
        }
    }

    /// `rx` から次のメッセージを取り出す。無害な空一覧通知はスキップする
    /// （sessions_tick と follow_tick は select! で競合し、どちらが先に発火するか
    /// テストからは制御できないため）
    async fn next_meaningful(rx: &mut mpsc::Receiver<Msg>) -> Msg {
        // 上限を切らないと、期待したメッセージが来ない欠陥（mutant）で無限に待ち続けて
        // cargo-mutants 側の timeout に落ちる。start_paused の時計では sessions_tick が
        // 連続発火して idle にならず time::timeout が効かないため、スキップ回数で打ち切る
        let mut skipped = 0usize;
        loop {
            let m = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("no message within 5s")
                .expect("channel closed unexpectedly");
            if let Msg::Sessions(s) = &m {
                if s.is_empty() {
                    skipped += 1;
                    assert!(skipped < 1000, "only empty session lists arrived");
                    continue;
                }
            }
            return m;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sessions_error_sends_error_and_reconnects() {
        let (tx, mut rx) = mpsc::channel(16);
        let (_sel_tx, sel_rx) = watch::channel((None, false));
        let mut sel = sel_rx;
        let be: Arc<dyn Backend> = Arc::new(
            ScriptedBackend::default()
                .with_list(Ok(vec!["s1".to_string()]))
                .with_list(Err("boom".to_string())),
        );
        let exit = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_session(be, &tx, &mut sel),
        )
        .await
        .expect("run_session did not exit");
        assert_eq!(exit, LoopExit::Reconnect);

        let first = next_meaningful(&mut rx).await;
        assert_eq!(first, Msg::Sessions(vec!["s1".to_string()]));
        let second = rx.try_recv().expect("expected error message");
        match second {
            Msg::Error(e) => assert!(e.contains("一覧取得に失敗"), "unexpected message: {e}"),
            other => panic!("expected Msg::Error, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn full_load_success_sends_replace_true() {
        let (tx, mut rx) = mpsc::channel(16);
        let (sel_tx, sel_rx) = watch::channel((Some("sess-a".to_string()), false));
        let mut sel = sel_rx;
        let be: Arc<dyn Backend> = Arc::new(
            ScriptedBackend::default()
                .with_fetch(Ok((vec![b"a".to_vec()], 1)))
                .with_fetch(Ok((vec![], 1))),
        );
        let tx_task = tx.clone();
        let handle = tokio::spawn(async move { run_session(be, &tx_task, &mut sel).await });

        let msg = next_meaningful(&mut rx).await;
        assert_eq!(
            msg,
            Msg::Events {
                session: "sess-a".to_string(),
                raw: vec![b"a".to_vec()],
                replace: true,
            }
        );

        // 読み込み後は無害にアイドルするだけなので、selection を閉じて Shutdown させる
        drop(sel_tx);
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("run_session did not exit")
            .unwrap();
        assert_eq!(exit, LoopExit::Shutdown);
    }

    #[tokio::test(start_paused = true)]
    async fn full_load_error_sends_error_and_reconnects() {
        let (tx, mut rx) = mpsc::channel(16);
        let (_sel_tx, sel_rx) = watch::channel((Some("sess-a".to_string()), false));
        let mut sel = sel_rx;
        let be: Arc<dyn Backend> =
            Arc::new(ScriptedBackend::default().with_fetch(Err("boom".to_string())));
        let exit = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_session(be, &tx, &mut sel),
        )
        .await
        .expect("run_session did not exit");
        assert_eq!(exit, LoopExit::Reconnect);
        let msg = next_meaningful(&mut rx).await;
        match msg {
            Msg::Error(e) => assert!(e.contains("読み込みに失敗"), "unexpected message: {e}"),
            other => panic!("expected Msg::Error, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn follow_incremental_sends_replace_false_then_empty_sends_nothing() {
        let (tx, mut rx) = mpsc::channel(16);
        let (sel_tx, sel_rx) = watch::channel((Some("sess-a".to_string()), true));
        let mut sel = sel_rx;
        let be: Arc<dyn Backend> = Arc::new(
            ScriptedBackend::default()
                // 初回 (need_full) は fetch_all 経由。空バッチ1回で即終了する
                .with_fetch(Ok((vec![], 0)))
                // 2回目 (follow, 直接 fetch_from) はデータあり
                .with_fetch(Ok((vec![b"x".to_vec()], 5)))
                // 3回目 (follow, 直接 fetch_from) は空 -> メッセージなし
                .with_fetch(Ok((vec![], 5))),
        );
        let tx_task = tx.clone();
        let handle = tokio::spawn(async move { run_session(be, &tx_task, &mut sel).await });

        let first = next_meaningful(&mut rx).await;
        assert_eq!(
            first,
            Msg::Events {
                session: "sess-a".to_string(),
                raw: vec![],
                replace: true,
            }
        );
        let second = next_meaningful(&mut rx).await;
        assert_eq!(
            second,
            Msg::Events {
                session: "sess-a".to_string(),
                raw: vec![b"x".to_vec()],
                replace: false,
            }
        );

        // 3回目の空応答はメッセージを送らない。selection を閉じて Shutdown させ、
        // 追加のメッセージが来ていないことを確認する
        drop(sel_tx);
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("run_session did not exit")
            .unwrap();
        assert_eq!(exit, LoopExit::Shutdown);
        while let Ok(m) = rx.try_recv() {
            if let Msg::Sessions(s) = &m {
                if s.is_empty() {
                    continue;
                }
            }
            panic!("unexpected extra message: {m:?}");
        }
    }

    /// 接続成功時、以後 `list_sessions` が即エラーになるバックエンドを返すか
    /// (Reconnect を誘発する)、何もしない無害なバックエンドを返すか (Shutdown 待ち用) を選べる
    enum ConnectOutcome {
        Fail(String),
        SucceedThenSessionError,
        SucceedIdle,
    }

    struct ScriptedConnector {
        results: Mutex<VecDeque<ConnectOutcome>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Connector for ScriptedConnector {
        async fn connect(&self) -> anyhow::Result<Arc<dyn Backend>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.results.lock().unwrap().pop_front() {
                Some(ConnectOutcome::Fail(e)) => Err(anyhow::anyhow!(e)),
                Some(ConnectOutcome::SucceedThenSessionError) => Ok(Arc::new(
                    ScriptedBackend::default().with_list(Err("session boom".to_string())),
                )
                    as Arc<dyn Backend>),
                Some(ConnectOutcome::SucceedIdle) => {
                    Ok(Arc::new(ScriptedBackend::default()) as Arc<dyn Backend>)
                }
                None => Err(anyhow::anyhow!("script exhausted")),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn outer_loop_backoff_escalates_then_resets_after_success() {
        let (tx, mut rx) = mpsc::channel(16);
        let (sel_tx, sel_rx) = watch::channel((None, false));
        let connector = ScriptedConnector {
            results: Mutex::new(VecDeque::from([
                ConnectOutcome::Fail("boom1".to_string()),
                ConnectOutcome::Fail("boom2".to_string()),
                ConnectOutcome::SucceedThenSessionError,
                ConnectOutcome::Fail("boom3".to_string()),
                ConnectOutcome::SucceedIdle,
            ])),
            calls: AtomicUsize::new(0),
        };

        let handle = tokio::spawn(async move {
            run_network_loop(&connector, tx, sel_rx).await;
            connector.calls.load(Ordering::SeqCst)
        });

        let m1 = rx.recv().await.unwrap();
        assert_eq!(
            m1,
            Msg::Error("Iggyに接続できません: boom1（1秒後に再試行）".to_string())
        );
        let m2 = rx.recv().await.unwrap();
        assert_eq!(
            m2,
            Msg::Error("Iggyに接続できません: boom2（2秒後に再試行）".to_string())
        );
        let m3 = rx.recv().await.unwrap();
        assert_eq!(m3, Msg::Connected);
        // 接続成功後、session_errors=true の ScriptedBackend が list_sessions で即エラーになり
        // Reconnect する
        let m4 = next_meaningful(&mut rx).await;
        match m4 {
            Msg::Error(e) => assert!(e.contains("一覧取得に失敗"), "unexpected: {e}"),
            other => panic!("expected Msg::Error, got {other:?}"),
        }
        let m5 = rx.recv().await.unwrap();
        // バックオフが 1 秒にリセットされていることを確認する（4秒にはならない）
        assert_eq!(
            m5,
            Msg::Error("Iggyに接続できません: boom3（1秒後に再試行）".to_string())
        );
        let m6 = rx.recv().await.unwrap();
        assert_eq!(m6, Msg::Connected);

        drop(sel_tx);
        let total_calls = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("network loop did not exit")
            .unwrap();
        assert_eq!(total_calls, 5);
    }

    // `network_loop` 自体（`IggyConnector` を組み立てて `run_network_loop` に渡す
    // 薄い配線部分、cc=1）は、拒否された接続に対してテストすることができない。
    // `iggy` クレートの TCP クライアントは既定で無制限リトライ・1秒間隔の
    // 自動再接続を内蔵しており、`client.connect()` はエラーを返さず内部で
    // 無限にリトライし続ける（`iggy-0.10.0/src/tcp/tcp_client.rs` で確認）。
    // 実 Iggy サーバなしにこの配線を検証する現実的な方法がないため、
    // `.cargo/mutants.toml` で該当ミュータントを除外している（理由はそちらに記載）
}
