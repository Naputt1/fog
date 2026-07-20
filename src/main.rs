#![deny(unsafe_op_in_unsafe_fn)]

use std::io::stdout;
use std::{fs, io};
mod app;
mod click_tab;
mod config;
mod keybinding;
mod process;
mod proxy;
mod selection;
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
#[command(name = "fog", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    #[arg(short, long, default_value = "fog.json")]
    config: std::path::PathBuf,

    #[arg(long, help = "Save service output to temp/<name>.txt on exit")]
    save_logs: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    let contents = match fs::read_to_string(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: could not read config '{}': {}", cli.config.display(), e);
            std::process::exit(1);
        }
    };

    let config: Config = match serde_json::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: invalid config '{}': {}", cli.config.display(), e);
            std::process::exit(1);
        }
    };

    let config_path = cli
        .config
        .canonicalize()
        .unwrap_or_else(|_| cli.config.clone());
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    let items: Vec<Terminal> = config
        .service
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            let service_path = config_dir.join(&entry.path);
            let name = entry.name.clone().unwrap_or_else(|| {
                service_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            let service_path = service_path.to_string_lossy().into_owned();
            match Terminal::spawn_command(&service_path, &entry.cmd, name.clone()) {
                Ok(mut t) => {
                    t.save_logs = cli.save_logs;
                    t
                }
                Err(e) => Terminal::spawn_error(name, format!("Failed to spawn: {e}")),
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
                ws: r.ws.unwrap_or(false),
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
