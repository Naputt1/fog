use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct Theme {
    pub proxy: Color,
    pub terminal: Color,
    pub stopped: Color,
    pub highlight: Color,
    pub status_200: Color,
    pub status_300: Color,
    pub status_400: Color,
    pub status_500: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            proxy: Color::Cyan,
            terminal: Color::Green,
            stopped: Color::Red,
            highlight: Color::Magenta,
            status_200: Color::Green,
            status_300: Color::Yellow,
            status_400: Color::Red,
            status_500: Color::Red,
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
        hex if hex.starts_with('#') && hex.len() == 7 => {
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
            if let Some(ref v) = c.proxy { theme.proxy = parse_color(v); }
            if let Some(ref v) = c.terminal { theme.terminal = parse_color(v); }
            if let Some(ref v) = c.stopped { theme.stopped = parse_color(v); }
            if let Some(ref v) = c.highlight { theme.highlight = parse_color(v); }
            if let Some(ref v) = c.status_200 { theme.status_200 = parse_color(v); }
            if let Some(ref v) = c.status_300 { theme.status_300 = parse_color(v); }
            if let Some(ref v) = c.status_400 { theme.status_400 = parse_color(v); }
            if let Some(ref v) = c.status_500 { theme.status_500 = parse_color(v); }
        }
        theme
    }
}
