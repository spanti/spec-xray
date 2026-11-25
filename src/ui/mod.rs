mod app;
mod live;
mod sessions;
mod view;

use anyhow::Result;
use app::App;
use crossterm::{
    cursor::Show,
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{io, panic, path::Path, time::Duration};

pub fn run(project_dir: &Path, session_id: Option<&str>, threshold: u64) -> Result<()> {
    let mut app = App::new(project_dir, session_id, threshold)?;
    app.watch()?;
    install_panic_hook();

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    enable_raw_mode()?;
    if let Err(error) = execute!(terminal.backend_mut(), EnterAlternateScreen) {
        disable_raw_mode()?;
        return Err(error.into());
    }

    let run_result = run_loop(&mut terminal, &mut app);
    let raw_result = disable_raw_mode();
    let leave_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let cursor_result = terminal.show_cursor();
    run_result?;
    raw_result?;
    leave_result?;
    cursor_result?;
    Ok(())
}

fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        previous(info);
    }));
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|frame| view::draw(frame, app))?;
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key);
        }
        app.poll_refresh();
    }
    Ok(())
}
