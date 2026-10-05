use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct Theme {
    /// App background, painted across the whole frame.
    pub bg: Color,
    /// Raised surfaces: header, sidebar rail, status bar.
    pub surface: Color,
    /// Selected/active row fill on top of [`Self::surface`].
    pub surface_alt: Color,
    /// Separators and popup borders.
    pub border: Color,
    /// Primary text.
    pub text: Color,
    /// Dimmed metadata, section labels, secondary text.
    pub text_muted: Color,
    /// Single UI accent: brand, selection bar, focus.
    pub accent: Color,
    pub proxy: Color,
    pub terminal: Color,
    pub stopped: Color,
    pub highlight: Color,
    pub status_200: Color,
    pub status_300: Color,
    pub status_400: Color,
    pub status_500: Color,
    pub scrollbar: Color,
    /// Selected row fill (sidebar selection, menus).
    pub selection_bg: Color,
    /// Text drawn on top of [`Self::selection_bg`].
    pub selection_fg: Color,
    /// Panel titles embedded in borders.
    pub title: Color,
    /// Keybar keycaps.
    pub key: Color,
    /// Per-tab accent hues, assigned by tab order and cycled.
    pub tab_colors: Vec<Color>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg: Color::Rgb(0x0d, 0x0d, 0x0f),
            surface: Color::Rgb(0x16, 0x16, 0x19),
            surface_alt: Color::Rgb(0x1e, 0x1e, 0x22),
            border: Color::Rgb(0x2a, 0x2a, 0x2f),
            text: Color::Rgb(0xe6, 0xe6, 0xe7),
            text_muted: Color::Rgb(0x8f, 0x8f, 0x98),
            accent: Color::Rgb(0x7a, 0xa2, 0xf7),
            proxy: Color::Rgb(0x7d, 0xcf, 0xff),
            terminal: Color::Rgb(0x9e, 0xce, 0x6a),
            stopped: Color::Rgb(0xf7, 0x76, 0x8e),
            highlight: Color::Rgb(0x7a, 0xa2, 0xf7),
            status_200: Color::Rgb(0x9e, 0xce, 0x6a),
            status_300: Color::Rgb(0xe0, 0xaf, 0x68),
            status_400: Color::Rgb(0xf7, 0x76, 0x8e),
            status_500: Color::Rgb(0xf7, 0x76, 0x8e),
            scrollbar: Color::Rgb(0x3b, 0x3b, 0x44),
            selection_bg: Color::Rgb(0x3b, 0x42, 0x61),
            selection_fg: Color::Rgb(0xe6, 0xe6, 0xe7),
            title: Color::Rgb(0x7a, 0xa2, 0xf7),
            key: Color::Rgb(0xe0, 0xaf, 0x68),
            tab_colors: vec![
                Color::Rgb(0x7d, 0xcf, 0xff),
                Color::Rgb(0xbb, 0x9a, 0xf7),
                Color::Rgb(0x7a, 0xa2, 0xf7),
                Color::Rgb(0x9e, 0xce, 0x6a),
                Color::Rgb(0xe0, 0xaf, 0x68),
                Color::Rgb(0xf7, 0x76, 0x8e),
            ],
        }
    }
}

fn parse_color(s: &str) -> Color {
    match s.to_lowercase().as_str() {
        "reset" | "default" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "dark_gray" | "dark_grey" => Color::DarkGray,
        "light_red" => Color::LightRed,
        "light_green" => Color::LightGreen,
        "light_yellow" => Color::LightYellow,
        "light_blue" => Color::LightBlue,
        "light_magenta" => Color::LightMagenta,
        "light_cyan" => Color::LightCyan,
        hex if hex.starts_with('#') && hex.len() == 7 && hex.is_ascii() => {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[1..3], 16),
                u8::from_str_radix(&hex[3..5], 16),
                u8::from_str_radix(&hex[5..7], 16),
            ) {
                Color::Rgb(r, g, b)
            } else {
                Color::Reset
            }
        }
        _ => Color::Reset,
    }
}

impl Theme {
    pub fn from_config(config: Option<&crate::config::ThemeConfig>) -> Self {
        let mut theme = Self::default();
        if let Some(c) = config {
            if let Some(ref v) = c.bg {
                theme.bg = parse_color(v);
            }
            if let Some(ref v) = c.surface {
                theme.surface = parse_color(v);
            }
            if let Some(ref v) = c.surface_alt {
                theme.surface_alt = parse_color(v);
            }
            if let Some(ref v) = c.border {
                theme.border = parse_color(v);
            }
            if let Some(ref v) = c.text {
                theme.text = parse_color(v);
            }
            if let Some(ref v) = c.text_muted {
                theme.text_muted = parse_color(v);
            }
            if let Some(ref v) = c.accent {
                theme.accent = parse_color(v);
            }
            if let Some(ref v) = c.proxy {
                theme.proxy = parse_color(v);
            }
            if let Some(ref v) = c.terminal {
                theme.terminal = parse_color(v);
            }
            if let Some(ref v) = c.stopped {
                theme.stopped = parse_color(v);
            }
            if let Some(ref v) = c.highlight {
                theme.highlight = parse_color(v);
            }
            if let Some(ref v) = c.status_200 {
                theme.status_200 = parse_color(v);
            }
            if let Some(ref v) = c.status_300 {
                theme.status_300 = parse_color(v);
            }
            if let Some(ref v) = c.status_400 {
                theme.status_400 = parse_color(v);
            }
            if let Some(ref v) = c.status_500 {
                theme.status_500 = parse_color(v);
            }
            if let Some(ref v) = c.scrollbar {
                theme.scrollbar = parse_color(v);
            }
            if let Some(ref v) = c.selection_bg {
                theme.selection_bg = parse_color(v);
            }
            if let Some(ref v) = c.selection_fg {
                theme.selection_fg = parse_color(v);
            }
            if let Some(ref v) = c.title {
                theme.title = parse_color(v);
            }
            if let Some(ref v) = c.key {
                theme.key = parse_color(v);
            }
            if let Some(ref v) = c.tab_colors
                && !v.is_empty()
            {
                theme.tab_colors = v.iter().map(|s| parse_color(s)).collect();
            }
        }
        theme
    }

    /// Returns the accent hue for the tab at `index`, cycling through
    /// [`Self::tab_colors`] and falling back to [`Self::accent`] when empty.
    pub fn tab_color(&self, index: usize) -> Color {
        if self.tab_colors.is_empty() {
            self.accent
        } else {
            self.tab_colors[index % self.tab_colors.len()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeConfig;

    #[test]
    fn test_parse_color_named() {
        assert_eq!(parse_color("red"), Color::Red);
        assert_eq!(parse_color("GREEN"), Color::Green);
        assert_eq!(parse_color("Blue"), Color::Blue);
        assert_eq!(parse_color("cyan"), Color::Cyan);
        assert_eq!(parse_color("magenta"), Color::Magenta);
        assert_eq!(parse_color("yellow"), Color::Yellow);
        assert_eq!(parse_color("black"), Color::Black);
        assert_eq!(parse_color("white"), Color::White);
    }

    #[test]
    fn test_parse_color_extended() {
        assert_eq!(parse_color("gray"), Color::Gray);
        assert_eq!(parse_color("grey"), Color::Gray);
        assert_eq!(parse_color("dark_gray"), Color::DarkGray);
        assert_eq!(parse_color("light_red"), Color::LightRed);
        assert_eq!(parse_color("light_green"), Color::LightGreen);
        assert_eq!(parse_color("light_blue"), Color::LightBlue);
        assert_eq!(parse_color("light_cyan"), Color::LightCyan);
        assert_eq!(parse_color("light_magenta"), Color::LightMagenta);
        assert_eq!(parse_color("light_yellow"), Color::LightYellow);
    }

    #[test]
    fn test_parse_color_hex() {
        assert_eq!(parse_color("#ff0000"), Color::Rgb(255, 0, 0));
        assert_eq!(parse_color("#00ff00"), Color::Rgb(0, 255, 0));
        assert_eq!(parse_color("#0000ff"), Color::Rgb(0, 0, 255));
        assert_eq!(parse_color("#ffffff"), Color::Rgb(255, 255, 255));
        assert_eq!(parse_color("#000000"), Color::Rgb(0, 0, 0));
    }

    #[test]
    fn test_parse_color_invalid() {
        assert_eq!(parse_color(""), Color::Reset);
        assert_eq!(parse_color("notacolor"), Color::Reset);
        assert_eq!(parse_color("xyz"), Color::Reset);
        assert_eq!(parse_color("#ff00"), Color::Reset);
        assert_eq!(parse_color("#gggggg"), Color::Reset);
    }

    #[test]
    fn test_parse_color_default() {
        assert_eq!(parse_color("reset"), Color::Reset);
        assert_eq!(parse_color("default"), Color::Reset);
    }

    #[test]
    fn test_parse_color_non_ascii_hex() {
        // 7-byte strings whose bytes are not all ASCII must not be sliced by
        // byte index (doing so would split a multi-byte char and panic).
        assert_eq!(parse_color("#日xxx"), Color::Reset);
        assert_eq!(parse_color("#ab日x"), Color::Reset);
    }

    #[test]
    fn test_theme_default() {
        let t = Theme::default();
        assert_eq!(t.bg, Color::Rgb(0x0d, 0x0d, 0x0f));
        assert_eq!(t.surface, Color::Rgb(0x16, 0x16, 0x19));
        assert_eq!(t.surface_alt, Color::Rgb(0x1e, 0x1e, 0x22));
        assert_eq!(t.border, Color::Rgb(0x2a, 0x2a, 0x2f));
        assert_eq!(t.text, Color::Rgb(0xe6, 0xe6, 0xe7));
        assert_eq!(t.text_muted, Color::Rgb(0x8f, 0x8f, 0x98));
        assert_eq!(t.accent, Color::Rgb(0x7a, 0xa2, 0xf7));
        assert_eq!(t.proxy, Color::Rgb(0x7d, 0xcf, 0xff));
        assert_eq!(t.terminal, Color::Rgb(0x9e, 0xce, 0x6a));
        assert_eq!(t.stopped, Color::Rgb(0xf7, 0x76, 0x8e));
        assert_eq!(t.highlight, Color::Rgb(0x7a, 0xa2, 0xf7));
        assert_eq!(t.status_200, Color::Rgb(0x9e, 0xce, 0x6a));
        assert_eq!(t.status_300, Color::Rgb(0xe0, 0xaf, 0x68));
        assert_eq!(t.status_400, Color::Rgb(0xf7, 0x76, 0x8e));
        assert_eq!(t.status_500, Color::Rgb(0xf7, 0x76, 0x8e));
        assert_eq!(t.scrollbar, Color::Rgb(0x3b, 0x3b, 0x44));
        assert_eq!(t.selection_bg, Color::Rgb(0x3b, 0x42, 0x61));
        assert_eq!(t.selection_fg, Color::Rgb(0xe6, 0xe6, 0xe7));
        assert_eq!(t.title, Color::Rgb(0x7a, 0xa2, 0xf7));
        assert_eq!(t.key, Color::Rgb(0xe0, 0xaf, 0x68));
        assert_eq!(t.tab_colors.len(), 6);
        assert_eq!(t.tab_colors[0], Color::Rgb(0x7d, 0xcf, 0xff));
    }

    #[test]
    fn test_theme_from_config_none() {
        let t = Theme::from_config(None);
        assert_eq!(t.proxy, Theme::default().proxy);
    }

    #[test]
    fn test_theme_from_config_partial() {
        let config = ThemeConfig {
            bg: Some("#000000".into()),
            surface: None,
            surface_alt: None,
            border: None,
            text: None,
            text_muted: None,
            accent: Some("#ff0000".into()),
            proxy: Some("yellow".into()),
            terminal: None,
            stopped: None,
            highlight: None,
            status_200: None,
            status_300: None,
            status_400: None,
            status_500: None,
            scrollbar: None,
            selection_bg: None,
            selection_fg: None,
            title: None,
            key: None,
            tab_colors: None,
        };
        let t = Theme::from_config(Some(&config));
        assert_eq!(t.bg, Color::Rgb(0, 0, 0));
        assert_eq!(t.accent, Color::Rgb(255, 0, 0));
        assert_eq!(t.proxy, Color::Yellow);
        assert_eq!(t.terminal, Theme::default().terminal);
    }

    #[test]
    fn test_theme_from_config_full() {
        let config = ThemeConfig {
            bg: Some("black".into()),
            surface: Some("dark_gray".into()),
            surface_alt: Some("gray".into()),
            border: Some("white".into()),
            text: Some("light_gray_missing".into()),
            text_muted: Some("dark_gray".into()),
            accent: Some("cyan".into()),
            proxy: Some("red".into()),
            terminal: Some("blue".into()),
            stopped: Some("white".into()),
            highlight: Some("cyan".into()),
            status_200: Some("green".into()),
            status_300: Some("yellow".into()),
            status_400: Some("magenta".into()),
            status_500: Some("light_red".into()),
            scrollbar: Some("gray".into()),
            selection_bg: Some("black".into()),
            selection_fg: Some("white".into()),
            title: Some("cyan".into()),
            key: Some("yellow".into()),
            tab_colors: Some(vec!["red".into(), "green".into()]),
        };
        let t = Theme::from_config(Some(&config));
        assert_eq!(t.bg, Color::Black);
        assert_eq!(t.surface, Color::DarkGray);
        assert_eq!(t.surface_alt, Color::Gray);
        assert_eq!(t.border, Color::White);
        // An unknown name falls back to `reset`, per the documented contract.
        assert_eq!(t.text, Color::Reset);
        assert_eq!(t.text_muted, Color::DarkGray);
        assert_eq!(t.accent, Color::Cyan);
        assert_eq!(t.proxy, Color::Red);
        assert_eq!(t.terminal, Color::Blue);
        assert_eq!(t.stopped, Color::White);
        assert_eq!(t.highlight, Color::Cyan);
        assert_eq!(t.status_200, Color::Green);
        assert_eq!(t.status_300, Color::Yellow);
        assert_eq!(t.status_400, Color::Magenta);
        assert_eq!(t.status_500, Color::LightRed);
        assert_eq!(t.scrollbar, Color::Gray);
        assert_eq!(t.selection_bg, Color::Black);
        assert_eq!(t.selection_fg, Color::White);
        assert_eq!(t.title, Color::Cyan);
        assert_eq!(t.key, Color::Yellow);
        assert_eq!(t.tab_colors, vec![Color::Red, Color::Green]);
    }

    #[test]
    fn test_tab_color_cycles() {
        let t = Theme::default();
        assert_eq!(t.tab_color(0), t.tab_colors[0]);
        assert_eq!(t.tab_color(6), t.tab_colors[0]);
        let empty = Theme {
            tab_colors: vec![],
            ..Theme::default()
        };
        assert_eq!(empty.tab_color(3), empty.accent);
    }
}
