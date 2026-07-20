use std::io::stdout;
use std::{fs, io};
mod app;
mod click_tab;
mod config;
mod proxy;
mod terminal;
use clap::Parser;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::app::App;
use crate::config::Config;
use crate::proxy::{ProxyInstance, RouteEntry};
use crate::terminal::Terminal;

#[derive(Parser)]
#[command(name = "fog")]
struct Cli {
    #[arg(short, long, default_value = "fog.json")]
    config: std::path::PathBuf,

    #[arg(long, help = "Save service output to temp/<name>.txt on exit")]
    save_logs: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    let contents = fs::read_to_string(&cli.config)?;

    let config: Config = serde_json::from_str(&contents).unwrap();

    let items: Vec<Terminal> = config
        .service
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let name = std::path::Path::new(&entry.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            match Terminal::spawn_command(&entry.path, &entry.cmd, name) {
                Ok(mut t) => {
                    t.save_logs = cli.save_logs;
                    Some(t)
                }
                Err(e) => {
                    eprintln!("error spawning {}: {}", entry.cmd, e);
                    None
                }
            }
        })
        .collect();

    let proxy = config.proxy.map(|pc| {
        let routes: Vec<RouteEntry> = pc
            .routes
            .into_iter()
            .map(|r| RouteEntry {
                path: r.path,
                upstream: r.upstream,
            })
            .collect();
        let mut p = ProxyInstance::new(pc.port, routes);
        p.start();
        p
    });

    ratatui::run(|terminal| App::new(items, proxy).run(terminal))?;

    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

    Ok(())
}
