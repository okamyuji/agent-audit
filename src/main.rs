use std::io;

use clap::Parser;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;

use agent_audit::app::{App, Msg};
use agent_audit::runtime::{network_loop, Cli};
use agent_audit::tui::{run_ui, CrosstermEvents};

fn read_pat() -> anyhow::Result<String> {
    std::env::var("IGGY_PAT").map_err(|_| anyhow::anyhow!("環境変数 IGGY_PAT を設定してください"))
}

fn enter_alt_screen() -> anyhow::Result<io::Stdout> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(stdout)
}

fn init_terminal() -> anyhow::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let stdout = enter_alt_screen()?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

/// 端末を元の状態に戻すベストエフォート処理。アプリ本来の結果（`result`）を
/// クリーンアップ失敗で覆い隠さないよう、エラーは意図的に無視する
fn teardown_terminal(term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(term.backend_mut(), LeaveAlternateScreen);
    let _ = term.show_cursor();
}

// run_ui has no .await point in its loop body, so on a single-worker runtime it never
// yields and network_loop is never polled — the UI hangs silently forever with an
// empty session list. Pin at least 2 workers so network_loop always gets a thread.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let pat = read_pat()?;
    let (tx, mut rx) = mpsc::channel::<Msg>(64);
    let (sel_tx, sel_rx) = tokio::sync::watch::channel::<(Option<String>, bool)>((None, false));
    tokio::spawn(network_loop(cli, pat, tx, sel_rx));

    let mut term = init_terminal()?;
    let mut app = App::new();
    let result = run_ui(&mut term, &mut app, &mut rx, &sel_tx, &mut CrosstermEvents).await;
    teardown_terminal(&mut term);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同じプロセス内の環境変数を読み書きするため、他のテストと並行実行させない
    /// よう1つのテスト関数にまとめる
    #[test]
    fn read_pat_reads_env_var_and_errors_when_unset() {
        std::env::remove_var("IGGY_PAT");
        let err = read_pat().unwrap_err();
        assert!(err.to_string().contains("IGGY_PAT"));

        std::env::set_var("IGGY_PAT", "secret-token");
        assert_eq!(read_pat().unwrap(), "secret-token");

        std::env::remove_var("IGGY_PAT");
    }
}
