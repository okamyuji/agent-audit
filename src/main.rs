use std::io;
use std::time::Duration;

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;

use agent_audit::app::{App, Msg};
use agent_audit::runtime::{network_loop, Cli};
use agent_audit::ui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let pat = std::env::var("IGGY_PAT")
        .map_err(|_| anyhow::anyhow!("環境変数 IGGY_PAT を設定してください"))?;
    let (tx, mut rx) = mpsc::channel::<Msg>(64);
    let (sel_tx, sel_rx) = tokio::sync::watch::channel::<(Option<String>, bool)>((None, false));
    tokio::spawn(network_loop(cli, pat, tx, sel_rx));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new();
    let result = run_ui(&mut term, &mut app, &mut rx, &sel_tx).await;
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    result
}

async fn run_ui(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: &mut mpsc::Receiver<Msg>,
    sel_tx: &tokio::sync::watch::Sender<(Option<String>, bool)>,
) -> anyhow::Result<()> {
    loop {
        term.draw(|f| ui::draw(f, app))?;
        while let Ok(m) = rx.try_recv() {
            app.apply(m);
        }
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    app.on_key(k);
                }
            }
        }
        let want = (app.session_name().map(str::to_string), app.follow);
        if *sel_tx.borrow() != want {
            let _ = sel_tx.send(want);
        }
        if app.should_quit {
            return Ok(());
        }
    }
}
