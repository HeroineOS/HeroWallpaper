//! `~/.config/hero/wallpaper.toml`, and the theme's colors and animation
//! switch (`~/.config/heroui/theme.conf`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT: &str = include_str!("../res/wallpaper.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Fills the screen, cropping what overflows.
    #[default]
    Cover,
    /// All of it, with bars of the background color.
    Contain,
    Stretch,
    /// Actual pixels, centered.
    Center,
    Tile,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        toml::Value::String(s.to_string()).try_into().ok()
    }
}

/// What one screen shows; unset fields come from the top level.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Screen {
    pub path: Option<String>,
    pub mode: Option<Mode>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub path: Option<String>,
    pub mode: Mode,
    pub color: Option<String>,
    /// Crossfade (ms).
    pub transition: u64,
    pub animate: bool,
    /// MB.
    pub animation_memory: u64,
    pub output: BTreeMap<String, Screen>,
}

impl Default for Config {
    fn default() -> Self {
        Config { path: None, mode: Mode::Cover, color: None, transition: 450, animate: true, animation_memory: 256, output: BTreeMap::new() }
    }
}

/// What a screen ends up showing.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub path: Option<PathBuf>,
    pub mode: Mode,
    /// Set color (else the theme's background).
    pub color: Option<u32>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn for_output(&self, name: &str) -> Shown {
        let s = self.output.get(name);
        let path = s.and_then(|s| s.path.as_deref()).or(self.path.as_deref()).filter(|p| !p.is_empty());
        let color = s.and_then(|s| s.color.as_deref()).or(self.color.as_deref());
        Shown { path: path.map(expand), mode: s.and_then(|s| s.mode).unwrap_or(self.mode), color: color.and_then(parse_color) }
    }
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

pub fn path() -> Option<PathBuf> {
    Some(config_home()?.join("hero").join("wallpaper.toml"))
}

pub fn theme_path() -> Option<PathBuf> {
    Some(config_home()?.join("heroui").join("theme.conf"))
}

/// `~/x` and `$HOME/x`.
pub fn expand(p: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("$HOME/")) {
        home.join(rest)
    } else if p == "~" {
        home
    } else {
        PathBuf::from(p)
    }
}

/// "#rrggbb", "rrggbb" or "0xrrggbb".
pub fn parse_color(s: &str) -> Option<u32> {
    let s = s.trim();
    let hex = s.strip_prefix('#').or_else(|| s.strip_prefix("0x")).unwrap_or(s);
    (hex.len() == 6).then(|| u32::from_str_radix(hex, 16).ok()).flatten()
}

/// What the wallpaper takes from the theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub background: u32,
    pub animations: bool,
}

impl Theme {
    pub fn load() -> Theme {
        let text = theme_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        Theme::parse(&text)
    }

    fn parse(text: &str) -> Theme {
        // The theme file only lists colors that differ from its mode's
        // palette (HeroUI's dark and light backgrounds; "system" counts
        // as dark).
        let (mut set, mut light, mut animations) = (None, false, true);
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "background" => set = parse_color(v).or(set),
                "mode" => light = v.trim() == "light",
                "animations" => animations = !matches!(v.trim(), "false" | "0" | "no" | "off"),
                _ => {}
            }
        }
        Theme { background: set.unwrap_or(if light { 0xf4f4f8 } else { 0x14141c }), animations }
    }
}

/// Sets `key` (at the top or in `[output."name"]`) in the config file,
/// keeping its comments.
pub fn set(file: &Path, output: Option<&str>, key: &str, value: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(file).unwrap_or_else(|_| DEFAULT.to_string());
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {e}", file.display()))?;
    let table = match output {
        None => doc.as_table_mut(),
        Some(name) => {
            let outs = doc.entry("output").or_insert_with(|| {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                toml_edit::Item::Table(t)
            });
            let outs = outs.as_table_mut().ok_or("`output` isn't a table")?;
            outs.entry(name).or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new())).as_table_mut().ok_or("not a table")?
        }
    };
    table[key] = toml_edit::value(value);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // Replaced whole, so the running wallpaper never reads half a file.
    let tmp = file.with_extension("toml.new");
    std::fs::write(&tmp, doc.to_string()).and_then(|_| std::fs::rename(&tmp, file)).map_err(|e| format!("{}: {e}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let c: Config = toml::from_str(DEFAULT).unwrap();
        assert_eq!(c.mode, Mode::Cover);
        assert!(c.animate);
    }

    #[test]
    fn per_output() {
        let c: Config = toml::from_str(
            r##"
path = "~/a.jpg"
color = "#102030"
[output."HDMI-A-1"]
path = "/b.png"
mode = "contain"
"##,
        )
        .unwrap();
        let home = std::env::var("HOME").unwrap();
        assert_eq!(c.for_output("eDP-1"), Shown { path: Some(PathBuf::from(format!("{home}/a.jpg"))), mode: Mode::Cover, color: Some(0x102030) });
        assert_eq!(c.for_output("HDMI-A-1").path, Some(PathBuf::from("/b.png")));
        assert_eq!(c.for_output("HDMI-A-1").mode, Mode::Contain);
        assert_eq!(Mode::parse("tile"), Some(Mode::Tile));
        assert_eq!(Mode::parse("nope"), None);
    }

    #[test]
    fn theme() {
        let t = Theme::parse("background = #101010\nanimations = false\n");
        assert_eq!(t, Theme { background: 0x101010, animations: false });
        assert_eq!(Theme::parse("mode = light\n"), Theme { background: 0xf4f4f8, animations: true });
    }

    #[test]
    fn set_keeps_comments() {
        let f = std::env::temp_dir().join(format!("herowallpaper-set-{}.toml", std::process::id()));
        std::fs::write(&f, "# mine\nmode = \"cover\"\n").unwrap();
        set(&f, None, "path", "/x.png").unwrap();
        set(&f, Some("DP-1"), "mode", "tile").unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        let _ = std::fs::remove_file(&f);
        assert!(text.starts_with("# mine\n"), "{text}");
        let c: Config = toml::from_str(&text).unwrap();
        assert_eq!(c.path.as_deref(), Some("/x.png"));
        assert_eq!(c.for_output("DP-1").mode, Mode::Tile);
    }
}
