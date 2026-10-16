//! Search results and what they do.

use windows::Win32::Foundation::HWND;

/// Which plugin produced a result; decides the tile colour, glyph and tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    App,
    Folder,
    File,
    Calculator,
    Units,
    Time,
    Values,
    Window,
    Shell,
    System,
    Settings,
    Service,
    Registry,
    Web,
}

impl Kind {
    pub fn tag(self) -> &'static str {
        match self {
            Kind::App => "App",
            Kind::Folder => "Folder",
            Kind::File => "File",
            Kind::Calculator => "Calculator",
            Kind::Units => "Units",
            Kind::Time => "Time",
            Kind::Values => "Value",
            Kind::Window => "Window",
            Kind::Shell => "Shell",
            Kind::System => "System",
            Kind::Settings => "Settings",
            Kind::Service => "Service",
            Kind::Registry => "Registry",
            Kind::Web => "Web",
        }
    }

    /// Segoe Fluent Icons glyph shown when there is no real icon.
    pub fn glyph(self) -> char {
        match self {
            Kind::App => '\u{E71D}',
            Kind::Folder => '\u{E8B7}',
            Kind::File => '\u{E8A5}',
            Kind::Calculator => '\u{E8EF}',
            Kind::Units => '\u{ECC6}',
            Kind::Time => '\u{E823}',
            Kind::Values => '\u{E943}',
            Kind::Window => '\u{E7C4}',
            Kind::Shell => '\u{E756}',
            Kind::System => '\u{E7E8}',
            Kind::Settings => '\u{E713}',
            Kind::Service => '\u{E912}',
            Kind::Registry => '\u{E8F1}',
            Kind::Web => '\u{E774}',
        }
    }

    /// Tile colour (RGB 0-1), one hue per kind so they are easy to tell apart.
    pub fn tint(self) -> [f32; 3] {
        let hex = match self {
            Kind::App => 0x378ADD,
            Kind::Folder | Kind::File => 0xEF9F27,
            Kind::Calculator | Kind::Units | Kind::Values => 0x1D9E75,
            Kind::Time => 0x5DCAA5,
            Kind::Window => 0x85B7EB,
            Kind::Shell => 0x888780,
            Kind::System => 0xE24B4A,
            Kind::Settings => 0xB4B2A9,
            Kind::Service => 0x7F77DD,
            Kind::Registry => 0xD4537E,
            Kind::Web => 0x3FA9D8,
        };
        [((hex >> 16) & 0xFF) as f32 / 255.0, ((hex >> 8) & 0xFF) as f32 / 255.0, (hex & 0xFF) as f32 / 255.0]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Icon {
    /// The kind's glyph on a coloured tile.
    Glyph,
    /// A specific glyph on the kind's tile.
    Custom(char),
    /// A real shell icon: a file path or `shell:AppsFolder\...` id.
    Shell(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// ShellExecute "open" on a path, URI or `shell:AppsFolder\...`, with optional args.
    Open { target: String, args: String },
    RunAsAdmin { target: String, args: String },
    /// An elevated command with its own name, e.g. "Stop service".
    Elevated { target: String, args: String, label: &'static str, glyph: char },
    ShowInFolder(String),
    Copy(String),
    Terminal(String),
    SwitchTo(isize),
    CloseWindow(isize),
    System(SystemCommand),
    RegistryOpen(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SystemCommand {
    Lock,
    Sleep,
    Hibernate,
    Shutdown,
    Restart,
    SignOut,
    EmptyRecycleBin,
}

impl Action {
    pub fn open(target: impl Into<String>) -> Self {
        Action::Open { target: target.into(), args: String::new() }
    }

    /// Glyph for the action button on the selected row.
    pub fn glyph(&self) -> char {
        match self {
            Action::RunAsAdmin { .. } => '\u{EA18}',
            Action::Elevated { glyph, .. } => *glyph,
            Action::ShowInFolder(_) => '\u{E838}',
            Action::Copy(_) => '\u{E8C8}',
            Action::Terminal(_) => '\u{E756}',
            Action::CloseWindow(_) => '\u{E711}',
            _ => '\u{E768}',
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Action::Open { .. } | Action::RegistryOpen(_) => "Open",
            Action::RunAsAdmin { .. } => "Run as administrator",
            Action::Elevated { label, .. } => label,
            Action::ShowInFolder(_) => "Show in folder",
            Action::Copy(_) => "Copy",
            Action::Terminal(_) => "Open terminal here",
            Action::SwitchTo(_) => "Switch to",
            Action::CloseWindow(_) => "Close window",
            Action::System(_) => "Run",
        }
    }

    /// Keyboard shortcut shown in the footer.
    pub fn shortcut(&self) -> &'static str {
        match self {
            Action::RunAsAdmin { .. } => "Ctrl+Shift+Enter",
            Action::ShowInFolder(_) => "Ctrl+Shift+E",
            Action::Copy(_) => "Ctrl+C",
            Action::Terminal(_) => "Ctrl+Shift+C",
            Action::CloseWindow(_) => "Ctrl+W",
            Action::Elevated { .. } => "Tab",
            _ => "Enter",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub title: String,
    pub subtitle: String,
    pub kind: Kind,
    pub icon: Icon,
    pub primary: Action,
    pub secondary: Vec<Action>,
    pub score: i32,
    /// Identity for history and for keeping the selection across refreshes.
    pub key: String,
    /// Destructive commands ask for a second Enter.
    pub confirm: bool,
}

impl Item {
    pub fn new(kind: Kind, title: impl Into<String>, subtitle: impl Into<String>, primary: Action) -> Self {
        let title = title.into();
        Self {
            key: format!("{}:{title}", kind.tag()),
            title,
            subtitle: subtitle.into(),
            kind,
            icon: Icon::Glyph,
            primary,
            secondary: Vec::new(),
            score: 0,
            confirm: false,
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = icon;
        self
    }

    pub fn score(mut self, score: i32) -> Self {
        self.score = score;
        self
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = key.into();
        self
    }

    pub fn with(mut self, action: Action) -> Self {
        self.secondary.push(action);
        self
    }

    pub fn confirm(mut self) -> Self {
        self.confirm = true;
        self
    }

    /// The secondary action bound to a shortcut, if this item has one.
    pub fn find(&self, pred: impl Fn(&Action) -> bool) -> Option<&Action> {
        self.secondary.iter().find(|a| pred(a))
    }
}

/// The folder a file lives in, shortened for display: `C:\Users\you\Documents\Notes` becomes
/// `~\Documents\Notes`. A path ending in a backslash is already a folder.
pub fn folder_label(path: &str) -> String {
    let dir = match path.trim_end_matches('\\').rfind('\\') {
        Some(i) if !path.ends_with('\\') => &path[..i],
        _ => path.trim_end_matches('\\'),
    };
    let dir = if dir.len() == 2 && dir.ends_with(':') { format!("{dir}\\") } else { dir.to_string() };
    match std::env::var("USERPROFILE") {
        Ok(home) if !home.is_empty() && dir.len() >= home.len() && dir[..home.len()].eq_ignore_ascii_case(&home) => {
            format!("~{}", &dir[home.len()..])
        }
        _ => dir,
    }
}

pub fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

/// Adds the usual file actions (admin, show in folder, copy path, terminal) for a path.
pub fn file_actions(item: Item, path: &str, is_dir: bool, runnable: bool) -> Item {
    let mut item = item;
    if runnable {
        item = item.with(Action::RunAsAdmin { target: path.to_string(), args: String::new() });
    }
    item = item.with(Action::ShowInFolder(path.to_string())).with(Action::Copy(path.to_string()));
    let dir = if is_dir {
        path.to_string()
    } else {
        std::path::Path::new(path).parent().map(|p| p.display().to_string()).unwrap_or_default()
    };
    if !dir.is_empty() {
        item = item.with(Action::Terminal(dir));
    }
    item
}
