pub mod app;
pub mod click_tab;
pub mod completion;
pub mod config;
pub mod config_watcher;
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(dead_code, unused_variables, unused_imports, clippy::ptr_arg)
)]
pub mod dnsmasq;
pub mod fds;
pub mod index;
pub mod ipc;
pub mod keybinding;
pub mod lock;
pub mod log;
pub mod ports;
pub mod process;
pub mod project;
pub mod proxy;
pub mod render;
pub mod router;
pub mod runtime;
pub mod selection;
pub mod terminal;
pub mod terminal_ws;
pub mod theme;
pub mod worktree;
