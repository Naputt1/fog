use std::io::stdout;
use std::{fs, io};
mod app;
mod config;
mod service;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::app::App;
use crate::config::Config;
mod click_tab;

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    let contents = fs::read_to_string("config.yml")?;

    let config: Config = serde_yaml::from_str(&contents).unwrap();

    ratatui::run(|terminal| App::new(config.service).run(terminal))?;

    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

    Ok(())
}
