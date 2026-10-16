//! Just enough of Rainmeter to run the user's skins natively.

pub mod formula;
pub mod ini;
pub mod skin;
pub mod time;

use std::path::PathBuf;

use ini::Ini;

/// Where Rainmeter keeps its settings and skins on this machine.
pub struct Install {
    pub settings: PathBuf,
    pub skins: Option<PathBuf>,
}

impl Install {
    pub fn locate() -> Self {
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
        let settings = appdata.join("Rainmeter").join("Rainmeter.ini");
        let skins = Ini::load(&settings)
            .ok()
            .and_then(|ini| ini.section("Rainmeter").and_then(|s| s.get("SkinPath")).map(PathBuf::from))
            .or_else(|| {
                let docs = std::env::var_os("USERPROFILE").map(PathBuf::from)?;
                [docs.join("OneDrive").join("Documents"), docs.join("Documents")]
                    .into_iter()
                    .map(|d| d.join("Rainmeter").join("Skins"))
                    .find(|d| d.is_dir())
            });
        Self { settings, skins }
    }

    /// WindowX/WindowY Rainmeter saved for a config such as `Summit\Time`.
    pub fn window_pos(&self, config: &str) -> Option<(i32, i32)> {
        let ini = Ini::load(&self.settings).ok()?;
        let s = ini.section(config)?;
        let n = |k: &str| s.get(k).and_then(|v| v.trim().trim_end_matches(['R', 'r', 'B', 'b']).parse::<i32>().ok());
        Some((n("WindowX")?, n("WindowY")?))
    }
}
