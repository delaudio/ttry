use std::io;

use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::{Block, Borders, Paragraph};

struct TerminalStateGuard;

impl Drop for TerminalStateGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let _terminal_state = TerminalStateGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    let mut message = String::from("Ratatui ready");
    loop {
        terminal.draw(|frame| {
            frame.render_widget(
                Paragraph::new(message.as_str())
                    .block(Block::default().borders(Borders::ALL).title("ttry fixture")),
                frame.area(),
            );
        })?;
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Char(ch) => message = format!("Ratatui key: {ch}"),
                _ => {}
            }
        }
    }
    terminal.show_cursor()?;
    Ok(())
}
