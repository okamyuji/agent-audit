//! CLI 定義とネットワークタスク。`main.rs` から呼ばれる薄い実行部を除き、
//! 結合テストから直接呼べるようライブラリ側に置く。

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;

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
}

/// 再接続バックオフの次の待ち時間。1 秒から倍増し、30 秒で頭打ちにする
pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(Duration::from_secs(30))
}

/// ネットワークタスク。セッション一覧を 5 秒ごと、選択セッションの末尾を追尾中は 500ms ごとに読む。
/// 不達時は 1 秒から倍増（最大 30 秒）で再接続する
pub async fn network_loop(
    cli: Cli,
    pat: String,
    tx: mpsc::Sender<Msg>,
    mut sel: tokio::sync::watch::Receiver<(Option<String>, bool)>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let be = match IggyBackend::connect(&cli.iggy_addr, &cli.stream, &pat).await {
            Ok(b) => Arc::new(b),
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
        let mut sessions_tick = tokio::time::interval(Duration::from_secs(5));
        let mut follow_tick = tokio::time::interval(Duration::from_millis(500));
        let mut loaded: Option<(String, u64)> = None;
        loop {
            tokio::select! {
                _ = sessions_tick.tick() => {
                    match be.list_sessions().await {
                        Ok(s) => { let _ = tx.send(Msg::Sessions(s)).await; }
                        Err(e) => { let _ = tx.send(Msg::Error(format!("一覧取得に失敗: {e:#}"))).await; break; }
                    }
                }
                _ = follow_tick.tick() => {
                    let (session, follow) = sel.borrow().clone();
                    let Some(session) = session else { continue };
                    let need_full = loaded.as_ref().map(|(s, _)| s != &session).unwrap_or(true);
                    if need_full {
                        match fetch_all(be.as_ref(), &session).await {
                            Ok((raw, next)) => {
                                loaded = Some((session.clone(), next));
                                let _ = tx.send(Msg::Events { session, raw, next_offset: next, replace: true }).await;
                            }
                            Err(e) => { let _ = tx.send(Msg::Error(format!("読み込みに失敗: {e:#}"))).await; break; }
                        }
                    } else if follow {
                        let (_, off) = loaded.clone().unwrap();
                        match be.fetch_from(&session, off).await {
                            Ok((raw, next)) if !raw.is_empty() => {
                                loaded = Some((session.clone(), next));
                                let _ = tx.send(Msg::Events { session, raw, next_offset: next, replace: false }).await;
                            }
                            Ok(_) => {}
                            Err(e) => { let _ = tx.send(Msg::Error(format!("追尾に失敗: {e:#}"))).await; break; }
                        }
                    }
                }
                changed = sel.changed() => { if changed.is_err() { return; } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
