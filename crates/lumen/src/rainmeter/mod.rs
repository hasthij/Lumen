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

/// One entry of the Skins folder: a folder of skins, or a single skin (.ini).
#[derive(Debug, Clone)]
pub enum Node {
    Dir { name: String, path: String, children: Vec<Node> },
    /// `rel` is the path under the Skins folder, e.g. `Summit\\Time\\Time.ini`.
    Skin { name: String, rel: String },
}

/// Lists the skins under `root` the way Rainmeter's manager does: folders (configs) with the
/// .ini files inside them. Only file names are read, so OneDrive files that aren't downloaded
/// stay where they are. Folders starting with `@` (resources) are skipped.
pub fn scan(root: &std::path::Path) -> Vec<Node> {
    fn walk(dir: &std::path::Path, rel: &str, depth: u32) -> Vec<Node> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut dirs = Vec::new();
        let mut skins = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}\\{name}") };
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                if name.starts_with('@') || name.starts_with('.') || depth >= 6 {
                    continue;
                }
                let children = walk(&path, &child_rel, depth + 1);
                if !children.is_empty() {
                    dirs.push(Node::Dir { name, path: child_rel, children });
                }
            } else if path.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("ini")) && !rel.is_empty() {
                let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(name);
                skins.push(Node::Skin { name: stem, rel: child_rel });
            }
        }
        let key = |n: &Node| match n {
            Node::Dir { name, .. } | Node::Skin { name, .. } => name.to_lowercase(),
        };
        dirs.sort_by_key(key);
        skins.sort_by_key(key);
        dirs.extend(skins);
        dirs
    }
    walk(root, "", 0)
}
