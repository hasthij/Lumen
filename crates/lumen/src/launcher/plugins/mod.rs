//! Routes a query to plugins: a keyword prefix ("=", "?", ">") picks one plugin, otherwise
//! every enabled global plugin answers.

pub mod calc;
pub mod files;
pub mod registry;
pub mod services;
pub mod system;
pub mod timedate;
pub mod units;
pub mod values;
pub mod windows;

use crate::config::LauncherConfig;

use super::apps::App;
use super::fuzzy;
use super::item::{file_actions, Action, Icon, Item, Kind};
use files::Job;

pub struct Context<'a> {
    pub cfg: &'a LauncherConfig,
    pub apps: &'a [App],
    pub windows: &'a [windows::OpenWindow],
}

/// The plugin a keyword selects, and the rest of the query.
pub fn route<'q>(cfg: &LauncherConfig, q: &'q str) -> Option<(&'static str, &'q str)> {
    let mut best: Option<(&'static str, usize)> = None;
    for (id, _, _) in crate::config::PLUGIN_DEFAULTS {
        let Some(p) = cfg.plugin(id) else { continue };
        if !p.keyword.is_empty() && q.starts_with(&p.keyword) && best.map_or(true, |(_, len)| p.keyword.len() > len) {
            best = Some((id, p.keyword.len()));
        }
    }
    best.map(|(id, len)| (id, q[len..].trim_start()))
}

/// Fast plugins answer synchronously; slow ones come back as worker jobs.
pub fn query(ctx: &Context, q: &str) -> (Vec<Item>, Vec<Job>) {
    let mut out = Vec::new();
    let mut jobs = Vec::new();
    if q.trim().is_empty() {
        return (out, jobs);
    }
    match route(ctx.cfg, q) {
        Some((id, rest)) => run(ctx, id, rest, true, &mut out, &mut jobs),
        None => {
            if files::is_path(q.trim()) && ctx.cfg.plugin("folder").is_some() {
                jobs.push(Job::Folder(q.trim().to_string()));
                return (out, jobs);
            }
            for (id, _, _) in crate::config::PLUGIN_DEFAULTS {
                if ctx.cfg.plugin(id).is_some_and(|p| p.global) {
                    run(ctx, id, q, false, &mut out, &mut jobs);
                }
            }
        }
    }
    (out, jobs)
}

fn run(ctx: &Context, id: &str, q: &str, explicit: bool, out: &mut Vec<Item>, jobs: &mut Vec<Job>) {
    match id {
        "apps" => apps(ctx.apps, q, explicit, out),
        "folder" if explicit || files::is_path(q) => jobs.push(Job::Folder(q.to_string())),
        "files" => jobs.push(Job::Files(q.to_string())),
        "calculator" => calc::query(q, explicit, out),
        "units" => units::query(q, out),
        "time" => timedate::query(q, explicit, out),
        "values" => values::query(q, out),
        "windows" => windows::query(q, explicit, ctx.windows, out),
        "shell" => shell(q, out),
        "system" => system::commands(q, explicit, out),
        "settings" => system::settings(q, explicit, out),
        "services" => services::query(q, out),
        "registry" => registry::query(q, out),
        "web" if !q.trim().is_empty() => out.push(web_item(ctx.cfg, q).score(5000)),
        _ => {}
    }
}

/// "Search the web for …", opened in the default browser with the chosen search engine.
pub fn web_item(cfg: &crate::config::LauncherConfig, q: &str) -> Item {
    let q = q.trim();
    let url = cfg.search_url_for(q);
    Item::new(
        Kind::Web,
        format!("Search the web for \u{201C}{q}\u{201D}"),
        format!("{}  ·  opens in your default browser", cfg.engine_name()),
        Action::open(url.clone()),
    )
    .key(format!("web:{q}"))
    .with(Action::Copy(url))
}

fn apps(apps: &[App], q: &str, explicit: bool, out: &mut Vec<Item>) {
    let ql = q.trim().to_lowercase();
    for app in apps {
        let s = if ql.is_empty() {
            explicit.then_some(0)
        } else {
            fuzzy::score(&ql, &app.name).or_else(|| {
                // "notepad" finds "Notepad++" by exe name too.
                app.path.as_deref().and_then(|p| std::path::Path::new(p).file_stem()).and_then(|s| fuzzy::score(&ql, &s.to_string_lossy())).map(|s| s - 450)
            })
        };
        let Some(s) = s else { continue };
        // The folder it lives in is enough; the app's own name is already the title.
        let subtitle = match (&app.path, app.packaged) {
            (Some(path), false) => super::item::folder_label(path),
            _ => "App".to_string(),
        };
        let mut item = Item::new(Kind::App, app.name.clone(), subtitle, Action::open(app.launch.clone()))
            .icon(Icon::Shell(app.launch.clone()))
            .key(format!("app:{}", app.launch))
            .score(s + 200);
        if let Some(path) = &app.path {
            item = file_actions(item, path, false, true);
        } else {
            item = item.with(Action::Copy(app.name.clone()));
        }
        out.push(item);
    }
}

fn shell(q: &str, out: &mut Vec<Item>) {
    let cmd = q.trim();
    if cmd.is_empty() {
        out.push(Item::new(Kind::Shell, "Type a command", "Runs in Command Prompt", Action::Copy(String::new())).score(0));
        return;
    }
    let args = format!("/k {cmd}");
    out.push(
        Item::new(Kind::Shell, format!("> {cmd}"), "Run in Command Prompt  ·  Ctrl+Shift+Enter as administrator", Action::Open { target: "cmd.exe".into(), args: args.clone() })
            .key("shell")
            .icon(Icon::Glyph)
            .score(5000)
            .with(Action::RunAsAdmin { target: "cmd.exe".into(), args })
            .with(Action::Copy(cmd.to_string())),
    );
}
