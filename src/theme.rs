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
    fn test_theme_default() {
        let t = Theme::default();
        assert_eq!(t.proxy, Color::Cyan);
        assert_eq!(t.terminal, Color::Green);
        assert_eq!(t.stopped, Color::Red);
        assert_eq!(t.highlight, Color::Magenta);
        assert_eq!(t.status_200, Color::Green);
        assert_eq!(t.status_300, Color::Yellow);
        assert_eq!(t.status_400, Color::Red);
        assert_eq!(t.status_500, Color::Red);
    }

    #[test]
    fn test_theme_from_config_none() {
        let t = Theme::from_config(None);
        assert_eq!(t.proxy, Color::Cyan);
    }

    #[test]
    fn test_theme_from_config_partial() {
        let config = ThemeConfig {
            proxy: Some("yellow".into()),
            terminal: None,
            stopped: None,
            highlight: None,
            status_200: None,
            status_300: None,
            status_400: None,
            status_500: None,
        };
        let t = Theme::from_config(Some(&config));
        assert_eq!(t.proxy, Color::Yellow);
        assert_eq!(t.terminal, Color::Green);
    }

    #[test]
    fn test_theme_from_config_full() {
        let config = ThemeConfig {
            proxy: Some("red".into()),
            terminal: Some("blue".into()),
            stopped: Some("white".into()),
            highlight: Some("cyan".into()),
            status_200: Some("green".into()),
            status_300: Some("yellow".into()),
            status_400: Some("magenta".into()),
            status_500: Some("light_red".into()),
        };
        let t = Theme::from_config(Some(&config));
        assert_eq!(t.proxy, Color::Red);
        assert_eq!(t.terminal, Color::Blue);
        assert_eq!(t.stopped, Color::White);
        assert_eq!(t.highlight, Color::Cyan);
        assert_eq!(t.status_200, Color::Green);
        assert_eq!(t.status_300, Color::Yellow);
        assert_eq!(t.status_400, Color::Magenta);
        assert_eq!(t.status_500, Color::LightRed);
    }
}
