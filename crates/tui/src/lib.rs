//! Terminal UI front end: sets up the terminal, pumps input events, and runs the
//! [`App`] event loop. Public entry point is [`run`].

use std::io;
use std::sync::Arc;

use anyhow::{Context, Result};
use browser_core::Config;
use crossterm::event::Event;
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

mod app;
mod dispatch;
mod render;

pub use dispatch::Dispatcher;

/// Headless render: fetch `target` in its routed mode and return the rendered
/// page as plain text. Used for testing/debugging without a terminal.
pub async fn dump(config: Config, target: &str, width: usize) -> Result<String> {
    let dispatcher = Dispatcher::new(config).context("initializing backends")?;
    let mode = dispatcher.resolve_mode(target);
    let doc = dispatcher.open(mode, target).await?;
    let mut out = format!("[{}] {}\n", mode.as_str(), doc.title);
    for line in render::render_document(&doc, width) {
        out.push_str(&render::line_text(&line));
        out.push('\n');
    }
    Ok(out)
}

/// Run the browser TUI. `initial` is an optional first target (URL or query).
pub async fn run(config: Config, initial: Option<String>) -> Result<()> {
    let dispatcher = Arc::new(Dispatcher::new(config.clone()).context("initializing backends")?);

    let mut terminal = setup_terminal().context("setting up terminal")?;
    let result = run_inner(&mut terminal, &config, dispatcher, initial).await;
    restore_terminal(&mut terminal).ok();
    result
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &Config,
    dispatcher: Arc<Dispatcher>,
    initial: Option<String>,
) -> Result<()> {
    // Read terminal events on a dedicated blocking thread, forwarding into async.
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if input_tx.send(event).is_err() {
                break;
            }
        }
    });

    let app = app::App::new(config, dispatcher);
    app.run(terminal, initial, input_rx).await
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
