use crate::click_tab::ClickTab;
use crate::config::HealthCheckConfig;
use crate::ipc::IpcState;
use crate::proxy::ProxyInstance;
use crate::selection;
use crate::terminal::Terminal;
use crate::theme::Theme;
use crossterm::event::Event;
use ratatui::layout::Rect;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

mod switch;
use switch::SwitchPopup;
mod input;
mod lifecycle;
mod render;
mod tabs;

enum Mode {
    Normal,
    TerminalInput,
    ProxyFilter,
}

/// Events delivered to the TUI loop by the input-reader and health-forwarder
/// threads. Fusing them onto one channel lets the loop wake instantly on either
/// input or a service health change.
enum AppEvent {
    /// A crossterm input event.
    Input(Event),
    /// A service health status changed; re-check pending dependents.
    Health,
}

/// A service waiting for its dependencies to become ready.
pub struct PendingService {
    /// Display name of the service.
    pub name: String,
    /// Shell command to execute.
    pub cmd: String,
    /// Working directory path.
    pub path: String,
    /// Maximum scrollback lines.
    pub scrollback: usize,
    /// Whether to save logs on exit.
    pub save_logs: bool,
    /// Directory to tee this service's raw PTY output into (`<name>.log`),
    /// used by detached (`-d`) runs so an external agent can tail it.
    pub log_dir: Option<PathBuf>,
    /// Names of services this depends on.
    pub dep_names: Vec<String>,
    /// Health check configurations for this service.
    pub health_checks: Vec<HealthCheckConfig>,
    /// Shell command to run on shutdown.
    pub shutdown_cmd: Option<String>,
    /// Resolved env vars for this service (from `${ports.*}` templates).
    pub injected_env: std::collections::HashMap<String, String>,
    /// Index in the `items` vec where this service's terminal lives.
    pub tab_index: usize,
}

/// A transient, auto-dismissing notification shown in the bottom-right corner.
struct Alert {
    message: String,
    created: Instant,
    ttl: Duration,
    /// Whether the alert was just copied (rendered with a green accent).
    copied: bool,
}

/// Screen regions of a visible alert, recorded at draw time for mouse hit-testing.
struct AlertHit {
    /// The whole box; clicking here copies the message.
    body: Rect,
    /// The `✕` control on the top border; clicking here dismisses the alert.
    close: Rect,
    /// Index into [`App::alerts`].
    index: usize,
}

/// How long a raised alert stays before auto-dismissing.
const ALERT_TTL: Duration = Duration::from_secs(8);
/// How long a copied alert lingers with its green confirmation.
const ALERT_COPIED_TTL: Duration = Duration::from_millis(1500);
/// Maximum number of alerts shown at once; older ones are dropped.
const ALERT_MAX_VISIBLE: usize = 4;
/// Preferred alert width, clamped to the frame.
const ALERT_WIDTH: u16 = 60;
/// Repaint cadence while alerts are visible, so they expire on time.
const ALERT_REPAINT: Duration = Duration::from_millis(250);

/// Main application state managing terminals, the proxy, tabs, and input handling.
pub struct App {
    items: Vec<Terminal>,
    pending_services: Vec<PendingService>,
    proxy: Option<ProxyInstance>,
    sigint: Arc<AtomicBool>,
    theme: Theme,
    scrollback: usize,
    tabs: ClickTab,
    mode: Mode,
    scroll_offset: usize,
    exit: bool,
    selecting: bool,
    select_start: Option<(usize, usize)>,
    select_end: Option<(usize, usize)>,
    content_area: Rect,
    show_help: bool,
    errors: Vec<String>,
    proxy_filter: String,
    config_path: std::path::PathBuf,
    /// The `--config` value as passed on the command line (may be relative),
    /// used to resolve a target worktree's config when switching.
    config_rel: PathBuf,
    /// Whether service output is saved to `temp/<name>.txt` on exit.
    save_logs: bool,
    config_rx: std::sync::mpsc::Receiver<()>,
    proxy_tab_index: Option<usize>,
    sidebar_min: u16,
    sidebar_max: u16,
    scrollbar_dragging: bool,
    auto_scrolling: Option<bool>,
    auto_scroll_col: u16,
    content_layout: selection::RowLayout,
    switch_popup: Option<SwitchPopup>,
    config_watcher_stop: Arc<AtomicBool>,
    ipc_state: Arc<IpcState>,
    /// Branch shown in the content panel's top title. Tracked separately from
    /// `ipc_state.branch` (which is set once at startup) so a worktree switch
    /// updates the title without changing the instance's IPC identity.
    title_branch: Option<String>,
    /// Whether `--no-share` was passed: when true shared/reuse services are
    /// always started fresh instead of borrowed/handed over.
    no_share: bool,
    /// Whether `--verbose` was passed: gates informational setup output.
    verbose: bool,
    /// Non-fatal runtime warnings and errors, shown as bottom-right alerts.
    alerts: Vec<Alert>,
    /// Screen regions of each visible alert at the last draw, so a mouse click
    /// can copy the alert body or dismiss it via the close control.
    alert_areas: Vec<AlertHit>,
    /// Set when something happened that requires the next frame to be drawn
    /// (input, health change, config reload, IPC control, resize).
    force_redraw: bool,
    /// Visible terminal's screen generation at the last draw.
    last_drawn_gen: usize,
    /// Proxy request-log fingerprint at the last draw.
    last_drawn_proxy_fp: (usize, u64),
    /// When a frame was last drawn, bounding how long a missed invalidation can
    /// leave the UI stale.
    last_draw: Instant,
}

/// Options for creating an `App` without `clippy::too_many_arguments`.
pub struct AppCreateOpts {
    pub items: Vec<Terminal>,
    pub pending_services: Vec<PendingService>,
    pub proxy: Option<ProxyInstance>,
    pub sigint: Arc<AtomicBool>,
    pub scrollback: usize,
    pub sidebar_min: u16,
    pub sidebar_max: u16,
    pub theme: Theme,
    pub config_path: std::path::PathBuf,
    pub config_rx: std::sync::mpsc::Receiver<()>,
    /// Stop flag owned by the active config watcher, set when it is replaced.
    pub config_watcher_stop: Arc<AtomicBool>,
    pub ipc_state: Arc<IpcState>,
    pub config_rel: PathBuf,
    pub save_logs: bool,
    pub no_share: bool,
    pub verbose: bool,
    /// Non-fatal startup warnings, surfaced as bottom-right alerts and
    /// reprinted to stderr on exit.
    pub startup_messages: Vec<String>,
}

impl App {
    /// Preferred constructor using `AppCreateOpts`.
    pub fn new_with_opts(opts: AppCreateOpts) -> Self {
        let AppCreateOpts {
            items,
            pending_services,
            proxy,
            sigint,
            scrollback,
            sidebar_min,
            sidebar_max,
            theme,
            config_path,
            config_rx,
            config_watcher_stop,
            ipc_state,
            config_rel,
            save_logs,
            no_share,
            verbose,
            startup_messages,
        } = opts;
        let alerts: Vec<Alert> = startup_messages
            .iter()
            .map(|message| Alert {
                message: message.clone(),
                created: Instant::now(),
                ttl: ALERT_TTL,
                copied: false,
            })
            .collect();
        let title_branch = ipc_state.branch.clone();
        let (tabs, proxy_tab_index) = Self::build_tabs(
            &items,
            &pending_services,
            proxy.is_some(),
            sidebar_min,
            sidebar_max,
        );

        Self {
            items,
            pending_services,
            proxy,
            sigint,
            scrollback,
            theme,
            tabs,
            mode: Mode::Normal,
            scroll_offset: 0,
            exit: false,
            selecting: false,
            select_start: None,
            select_end: None,
            content_area: Rect::default(),
            show_help: false,
            errors: startup_messages.clone(),
            proxy_filter: String::new(),
            config_path,
            config_rel,
            save_logs,
            config_rx,
            config_watcher_stop,
            ipc_state,
            title_branch,
            proxy_tab_index,
            sidebar_min,
            sidebar_max,
            scrollbar_dragging: false,
            auto_scrolling: None,
            auto_scroll_col: 0,
            content_layout: Vec::new(),
            switch_popup: None,
            no_share,
            verbose,
            alerts,
            alert_areas: Vec::new(),
            force_redraw: true,
            last_drawn_gen: 0,
            last_drawn_proxy_fp: (0, 0),
            last_draw: Instant::now(),
        }
    }
}

impl App {
    /// Records a runtime warning or error: raised as a bottom-right alert and
    /// reprinted to stderr once the TUI exits.
    pub(crate) fn note_error(&mut self, msg: String) {
        self.push_alert(msg.clone());
        self.errors.push(msg);
    }

    /// Pushes an alert, dropping the oldest beyond [`ALERT_MAX_VISIBLE`].
    ///
    /// Informational setup lines (see [`crate::log::is_info`]) never become
    /// alerts; this is the single gate every caller funnels through.
    pub(crate) fn push_alert(&mut self, msg: String) {
        if crate::log::is_info(&msg) {
            return;
        }
        self.alerts.push(Alert {
            message: msg,
            created: Instant::now(),
            ttl: ALERT_TTL,
            copied: false,
        });
        if self.alerts.len() > ALERT_MAX_VISIBLE {
            let drop = self.alerts.len() - ALERT_MAX_VISIBLE;
            self.alerts.drain(0..drop);
        }
        self.force_redraw = true;
    }

    /// Removes alerts whose lifetime has elapsed. Returns `true` if any were
    /// removed, so the caller can request a repaint.
    pub(crate) fn prune_alerts(&mut self) -> bool {
        let before = self.alerts.len();
        self.alerts.retain(|a| a.created.elapsed() < a.ttl);
        before != self.alerts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::render::panel_title_text;
    use super::*;
    use crate::click_tab::TabKind;
    use crate::worktree::Worktree;
    use crossterm::event::{KeyCode, KeyEvent};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    fn make_app(
        items: Vec<Terminal>,
        proxy: Option<ProxyInstance>,
        tabs: ClickTab,
        mode: Mode,
        content_area: Rect,
    ) -> App {
        let proxy_tab_index = tabs.entries.iter().position(|e| e.kind == TabKind::Proxy);
        let (_tx, rx) = mpsc::channel();
        App {
            items,
            pending_services: vec![],
            proxy,
            sigint: Arc::new(AtomicBool::new(false)),
            theme: Theme::default(),
            scrollback: 0,
            tabs,
            mode,
            scroll_offset: 0,
            exit: false,
            selecting: false,
            select_start: None,
            select_end: None,
            content_area,
            show_help: false,
            errors: vec![],
            proxy_filter: String::new(),
            config_path: PathBuf::new(),
            config_rel: PathBuf::from("fog.json"),
            save_logs: false,
            config_rx: rx,
            config_watcher_stop: Arc::new(AtomicBool::new(false)),
            ipc_state: Arc::new(IpcState::new("test".to_string(), None, None, false)),
            title_branch: None,
            proxy_tab_index,
            sidebar_min: 10,
            sidebar_max: 30,
            scrollbar_dragging: false,
            auto_scrolling: None,
            auto_scroll_col: 0,
            content_layout: Vec::new(),
            switch_popup: None,
            no_share: false,
            verbose: false,
            alerts: vec![],
            alert_areas: Vec::new(),
            force_redraw: true,
            last_drawn_gen: 0,
            last_drawn_proxy_fp: (0, 0),
            last_draw: Instant::now(),
        }
    }

    #[test]
    fn test_panel_title_text() {
        // Git repo: repo name plus branch.
        assert_eq!(
            panel_title_text(Some("/Users/alice/dev/fog/.git"), Some("main")),
            " fog (main) "
        );
        // Detached checkout in a git repo: repo name, no branch.
        assert_eq!(
            panel_title_text(Some("/Users/alice/dev/fog/.git"), None),
            " fog (detached) "
        );
        // Non-git fallback identity: dir name only, no "detached".
        assert_eq!(
            panel_title_text(Some("/tmp/my-project"), None),
            " my-project "
        );
        // No identity at all.
        assert_eq!(panel_title_text(None, Some("main")), "");
    }

    #[test]
    fn test_is_proxy_tab_false() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        assert!(!app.is_proxy_tab());
    }

    #[test]
    fn test_info_lines_never_raise_alerts() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());

        app.note_error("  + port web -> 1".to_string());
        assert!(app.alerts.is_empty(), "info lines never raise an alert");
        assert_eq!(app.errors.len(), 1);

        app.note_error("⚠ docker hung".to_string());
        assert_eq!(app.alerts.len(), 1);
        assert_eq!(app.errors.len(), 2);
    }

    #[test]
    fn test_push_alert_caps_and_flags_redraw() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.force_redraw = false;

        for i in 0..6 {
            app.push_alert(format!("m{i}"));
        }

        assert_eq!(app.alerts.len(), ALERT_MAX_VISIBLE);
        assert_eq!(app.alerts.first().unwrap().message, "m2");
        assert_eq!(app.alerts.last().unwrap().message, "m5");
        assert!(app.force_redraw);
    }

    #[test]
    fn test_prune_alerts_removes_expired() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.push_alert("⚠ boom".to_string());
        app.alerts[0].created = Instant::now() - Duration::from_secs(60);

        assert!(app.prune_alerts());
        assert!(app.alerts.is_empty());
        assert!(!app.prune_alerts());
    }

    #[test]
    fn test_alert_click_copies_and_flags() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.push_alert("⚠ boom".to_string());
        app.alert_areas = vec![AlertHit {
            body: Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 3,
            },
            close: Rect {
                x: 8,
                y: 0,
                width: 1,
                height: 1,
            },
            index: 0,
        }];

        assert!(app.handle_alert_click(5, 1));
        assert!(app.alerts[0].copied);
        assert_eq!(app.alerts[0].ttl, ALERT_COPIED_TTL);
        assert!(!app.handle_alert_click(50, 50));
    }

    #[test]
    fn test_alert_close_dismisses_without_copying() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.push_alert("⚠ boom".to_string());
        app.alert_areas = vec![AlertHit {
            body: Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 3,
            },
            close: Rect {
                x: 8,
                y: 0,
                width: 1,
                height: 1,
            },
            index: 0,
        }];

        assert!(app.handle_alert_click(8, 0));
        assert!(app.alerts.is_empty());
    }

    #[test]
    fn test_alert_renders_message() {
        use ratatui::Terminal as RatatuiTerminal;
        use ratatui::backend::TestBackend;

        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.push_alert("⚠ docker hung".to_string());

        let mut terminal = RatatuiTerminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();

        let buf = terminal.backend().buffer();
        let content: String = (0..24)
            .flat_map(|y| (0..60).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        // Exactly one warning marker: the box title. The message's own `⚠ ` is
        // stripped from the display, so copy/stderr keep the full text.
        assert_eq!(
            content.matches('⚠').count(),
            1,
            "expected one marker: {content}"
        );
        assert!(content.contains('✕'), "close control missing");
        assert!(content.contains("docker hung"), "alert text missing");
    }

    #[test]
    fn test_is_proxy_tab_true() {
        let mut tabs = ClickTab::new(vec![], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        assert!(app.is_proxy_tab());
    }

    #[test]
    fn test_is_proxy_tab_not_selected() {
        let mut tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        tabs.index = 0;
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        assert!(!app.is_proxy_tab());
    }

    #[test]
    fn test_item_index_for_tab_with_proxy_at_zero() {
        // Mirrors build_tabs: proxy inserted at index 0, items have no proxy.
        let mut tabs = ClickTab::new(vec!["svc_a".into(), "svc_b".into()], 10, 30);
        tabs.insert_at(0, "proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        // proxy tab maps to no item
        assert_eq!(app.item_index_for_tab(0), None);
        // first service tab -> items[0]
        assert_eq!(app.item_index_for_tab(1), Some(0));
        // second service tab -> items[1]
        assert_eq!(app.item_index_for_tab(2), Some(1));
    }

    #[test]
    fn test_item_index_for_tab_without_proxy() {
        let tabs = ClickTab::new(vec!["svc_a".into(), "svc_b".into()], 10, 30);
        let app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        assert_eq!(app.item_index_for_tab(0), Some(0));
        assert_eq!(app.item_index_for_tab(1), Some(1));
    }

    #[test]
    fn test_content_height() {
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 20,
            },
        );
        assert_eq!(app.content_height(), 18);
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 5,
            },
        );
        assert_eq!(app.content_height(), 3);
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect {
                x: 0,
                y: 0,
                width: 50,
                height: 1,
            },
        );
        assert_eq!(app.content_height(), 0);
    }

    #[test]
    fn test_current_total_lines_no_proxy_empty() {
        let app = make_app(
            vec![],
            None,
            ClickTab::new(vec![], 10, 30),
            Mode::Normal,
            Rect::default(),
        );
        assert_eq!(app.current_total_lines(), 0);
    }

    #[test]
    fn test_current_total_lines_with_proxy() {
        let mut tabs = ClickTab::new(vec![], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::Normal,
            Rect::default(),
        );
        assert_eq!(app.current_total_lines(), 3);
    }

    #[test]
    fn test_current_total_lines_with_proxy_filter_mode() {
        let mut tabs = ClickTab::new(vec![], 10, 30);
        tabs.add("proxy".into(), TabKind::Proxy);
        let app = make_app(
            vec![],
            Some(ProxyInstance::new(8080, None, vec![], 1000, None, None)),
            tabs,
            Mode::ProxyFilter,
            Rect::default(),
        );
        assert_eq!(app.current_total_lines(), 4);
    }

    #[test]
    fn test_switch_popup_filter_by_branch() {
        let popup = SwitchPopup {
            worktrees: vec![
                Worktree {
                    path: PathBuf::from("/repo/fog"),
                    branch: Some("main".to_string()),
                },
                Worktree {
                    path: PathBuf::from("/repo/fog-feature"),
                    branch: Some("feature-x".to_string()),
                },
            ],
            filter: "feature".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        let matches = popup.matches();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].branch.as_deref(), Some("feature-x"));
    }

    #[test]
    fn test_switch_popup_filter_empty_matches_all() {
        let popup = SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        assert_eq!(popup.matches().len(), 1);
    }

    #[test]
    fn test_switch_popup_filter_matches_path() {
        let popup = SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog-detached"),
                branch: None,
            }],
            filter: "detached".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        assert_eq!(popup.matches().len(), 1);
    }

    #[test]
    fn test_switch_popup_filter_no_match() {
        let popup = SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: "zzz".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        assert!(popup.matches().is_empty());
    }

    #[test]
    fn test_switch_popup_fuzzy_subsequence() {
        let popup = SwitchPopup {
            worktrees: vec![
                Worktree {
                    path: PathBuf::from("/repo/fog"),
                    branch: Some("main".to_string()),
                },
                Worktree {
                    path: PathBuf::from("/repo/fog-feature"),
                    branch: Some("feature-x".to_string()),
                },
                Worktree {
                    path: PathBuf::from("/repo/fog-detached"),
                    branch: Some("detached-cleanup".to_string()),
                },
            ],
            filter: "ftx".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        let matches = popup.matches();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].branch.as_deref(), Some("feature-x"));
    }

    #[test]
    fn test_switch_popup_fuzzy_subsequence_case_insensitive() {
        let popup = SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog-feature"),
                branch: Some("feature-x".to_string()),
            }],
            filter: "FTX".to_string(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        };
        assert_eq!(popup.matches().len(), 1);
    }

    #[test]
    fn test_switch_popup_search_mode_keys() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        });
        // 'f' enters search mode; typing appends to the filter.
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(!app.switch_popup.as_ref().unwrap().searching);
        assert!(app.switch_popup.as_ref().unwrap().filter.is_empty());
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('f')));
        assert!(app.switch_popup.as_ref().unwrap().searching);
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(app.switch_popup.as_ref().unwrap().filter, "a");
        // Backspace clears the filter while searching.
        app.handle_switch_key(KeyEvent::from(KeyCode::Backspace));
        assert!(app.switch_popup.as_ref().unwrap().filter.is_empty());
        // Esc exits search mode but keeps the popup open.
        app.handle_switch_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.switch_popup.is_some());
        assert!(!app.switch_popup.as_ref().unwrap().searching);
        // Typing outside search mode does nothing.
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('x')));
        assert!(app.switch_popup.as_ref().unwrap().filter.is_empty());
        // Esc while browsing closes the popup.
        app.handle_switch_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.switch_popup.is_none());
    }

    #[test]
    fn test_switch_popup_d_while_searching_is_filter_input() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: true,
            running: Vec::new(),
            status: None,
        });
        // While searching `d` is filter input, never a terminate.
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('d')));
        let popup = app.switch_popup.as_ref().unwrap();
        assert_eq!(popup.filter, "d");
        assert!(popup.status.is_none());
    }

    #[test]
    fn test_switch_popup_status_cleared_on_next_key() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: Some("terminated 1 instance".to_string()),
        });
        // Any key press clears the transient status before handling itself.
        app.handle_switch_key(KeyEvent::from(KeyCode::Down));
        assert!(app.switch_popup.as_ref().unwrap().status.is_none());
    }

    #[test]
    fn test_switch_popup_d_reports_no_instances() {
        // make_app's IpcState has no project, so no instance can match: `d`
        // reports the zero-outcome status without touching any process.
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        });
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('d')));
        assert_eq!(
            app.switch_popup.as_ref().unwrap().status.as_deref(),
            Some("no running instances on this branch")
        );
    }

    #[test]
    fn test_switch_popup_d_on_detached_worktree() {
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog-detached"),
                branch: None,
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        });
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('d')));
        assert_eq!(
            app.switch_popup.as_ref().unwrap().status.as_deref(),
            Some("no running instances on this branch (detached)")
        );
    }

    #[test]
    fn test_switch_popup_d_on_current_branch_no_other_instances() {
        // `d` on the current branch should report "no other instances" rather
        // than "no running instances" to clarify that self is excluded.
        let tabs = ClickTab::new(vec!["svc".into()], 10, 30);
        let mut app = make_app(vec![], None, tabs, Mode::Normal, Rect::default());
        // Make this app's config_dir be /repo/fog so the worktree counts as current.
        app.config_path = PathBuf::from("/repo/fog/fog.json");
        app.switch_popup = Some(SwitchPopup {
            worktrees: vec![Worktree {
                path: PathBuf::from("/repo/fog"),
                branch: Some("main".to_string()),
            }],
            filter: String::new(),
            selected: 0,
            searching: false,
            running: Vec::new(),
            status: None,
        });
        app.handle_switch_key(KeyEvent::from(KeyCode::Char('d')));
        assert_eq!(
            app.switch_popup.as_ref().unwrap().status.as_deref(),
            Some("no other instances on this branch")
        );
    }
}
