use std::io::stdout;
use std::{fs, io};
mod app;
mod click_tab;
mod config;
mod terminal;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::app::App;
use crate::config::Config;
use crate::terminal::Terminal;

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    let contents = fs::read_to_string("config.yml")?;

    let config: Config = serde_yaml::from_str(&contents).unwrap();

    let items: Vec<Terminal> = config
        .service
        .into_iter()
        .filter_map(|entry| {
            let name = std::path::Path::new(&entry.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            match Terminal::spawn_command(&entry.path, &entry.cmd, name) {
                Ok(t) => Some(t),
                Err(e) => {
                    eprintln!("error spawning {}: {}", entry.cmd, e);
                    None
                }
            }
        })
        .collect();

    ratatui::run(|terminal| App::new(items).run(terminal))?;

    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

    Ok(())
}
