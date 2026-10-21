//! A Rainmeter skin loaded from disk and laid out into drawable items.
//!
//! Supported: [Variables] with @include, #Var# and built-in variables, formulas,
//! Time measures with Substitute, String and Shape (Rectangle) meters, relative X/Y
//! (`r`/`R`), StringAlign, StringCase, StringStyle, FontWeight, CharacterSpacing,
//! Hidden, DynamicWindowSize. Anything else is logged once and skipped.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use windows::Win32::Foundation::SYSTEMTIME;

use super::formula;
use super::ini::{unquote, Ini, Section};
use super::time;

pub type Rgba = [f32; 4];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    pub face: String,
    /// Size in pixels (Rainmeter's FontSize is in points at 96 DPI).
    pub size: f32,
    pub weight: u32,
    pub italic: bool,
    /// CharacterSpacing: leading, trailing, minimum advance.
    pub spacing: [f32; 3],
}

#[derive(Debug, Clone)]
pub enum Item {
    /// `stretch` is the horizontal scale from TransformationMatrix (1.0 = none).
    Text { text: String, style: TextStyle, color: Rgba, x: f32, y: f32, stretch: f32 },
    /// A picture file. `h` of 0 keeps the picture's own proportions; `tint` multiplies its colours.
    Image { path: String, x: f32, y: f32, w: f32, h: f32, alpha: f32, tint: Rgba },
    Rect { x: f32, y: f32, w: f32, h: f32, radius: f32, fill: Rgba, stroke: Rgba, stroke_width: f32 },
}

#[derive(Debug, Clone, Default)]
pub struct Scene {
    pub items: Vec<Item>,
    pub width: u32,
    pub height: u32,
}

struct Measure {
    kind: String,
    format: String,
    substitutes: Vec<(String, String)>,
    /// RegExpSubstitute=1: the substitute patterns are regular expressions.
    regex: Vec<(regex::Regex, String)>,
}

/// Lumen-side adjustments applied over the skin's own variables (files are not touched).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    pub hour24: Option<bool>,
    /// "r,g,b" applied to every variable whose name ends in "Color".
    pub color: Option<String>,
}

pub struct Skin {
    pub file: PathBuf,
    pub fonts_dir: PathBuf,
    vars: HashMap<String, String>,
    measures: HashMap<String, Measure>,
    meters: Vec<Section>,
    /// Files whose change should trigger a reload, with the mtime seen at load.
    watched: Vec<(PathBuf, Option<SystemTime>)>,
    pub needs_seconds: bool,
    /// x, y, width, height of every meter from the previous layout pass, for `[Meter:W]` style
    /// references (a meter may refer to ones that come after it).
    geo: std::cell::RefCell<HashMap<String, [f64; 4]>>,
    /// Meters and measures this skin uses that Lumen doesn't draw yet (short names).
    pub unsupported: Vec<String>,
}

impl Skin {
    /// `skins_root` is the Skins folder; `rel` is e.g. `Summit\Time\Time.ini`.
    pub fn load(skins_root: &Path, rel: &str, overrides: &Overrides) -> Result<Self, String> {
        let file = skins_root.join(rel);
        let ini = Ini::load(&file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;

        let rel_dir = Path::new(rel).parent().unwrap_or(Path::new(""));
        let root_config = rel_dir.components().next().map(|c| c.as_os_str().to_owned()).unwrap_or_default();
        let resources = skins_root.join(&root_config).join("@Resources");
        let current_path = file.parent().unwrap_or(skins_root).to_path_buf();

        let mut vars = HashMap::new();
        let with_slash = |p: &Path| format!("{}\\", p.display());
        vars.insert("@".into(), with_slash(&resources));
        vars.insert("currentpath".into(), with_slash(&current_path));
        vars.insert("skinspath".into(), with_slash(skins_root));
        vars.insert("rootconfigpath".into(), with_slash(&skins_root.join(&root_config)));
        vars.insert("currentconfig".into(), rel_dir.display().to_string());
        vars.insert("currentfile".into(), file.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
        vars.insert("crlf".into(), "\n".into());

        let mut skin = Skin {
            fonts_dir: resources.join("Fonts"),
            file: file.clone(),
            vars,
            measures: HashMap::new(),
            meters: Vec::new(),
            watched: vec![(file.clone(), mtime(&file))],
            needs_seconds: false,
            geo: Default::default(),
            unsupported: Vec::new(),
        };

        let mut sections = Vec::new();
        let mut seen = HashSet::new();
        skin.flatten(ini, &mut sections, &mut seen, 0);
        skin.apply(overrides);

        let mut unsupported = Vec::new();
        for s in sections {
            if s.name.eq_ignore_ascii_case("Variables") || s.name.eq_ignore_ascii_case("Rainmeter") || s.name.eq_ignore_ascii_case("Metadata") {
                continue;
            }
            if let Some(kind) = s.get("Measure") {
                let kind = kind.to_ascii_lowercase();
                let format = unquote(&skin.expand(s.get("Format").unwrap_or("%H:%M:%S"))).to_string();
                if kind == "time" && time::needs_seconds(&format) {
                    skin.needs_seconds = true;
                }
                if kind != "time" {
                    unsupported.push(format!("Measure={kind} [{}]", s.name));
                }
                let substitutes = s.get("Substitute").map(|v| parse_substitute(&skin.expand(v))).unwrap_or_default();
                let use_regex = s.get("RegExpSubstitute").is_some_and(|v| v.trim() == "1");
                let regex = if use_regex {
                    substitutes
                        .iter()
                        .filter_map(|(from, to)| {
                            // Rainmeter writes group references as \1; the regex crate wants ${1}.
                            let mut rep = String::new();
                            let mut chars = to.chars().peekable();
                            while let Some(c) = chars.next() {
                                match (c, chars.peek()) {
                                    ('\\', Some(d)) if d.is_ascii_digit() => {
                                        rep.push_str(&format!("${{{d}}}"));
                                        chars.next();
                                    }
                                    ('$', _) => rep.push_str("$$"),
                                    _ => rep.push(c),
                                }
                            }
                            regex::Regex::new(from).ok().map(|r| (r, rep))
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                skin.measures.insert(s.name.to_ascii_lowercase(), Measure { kind, format, substitutes, regex });
            } else if let Some(kind) = s.get("Meter") {
                let k = kind.to_ascii_lowercase();
                if k != "string" && k != "shape" && k != "image" {
                    unsupported.push(format!("Meter={kind} [{}]", s.name));
                }
                skin.meters.push(s);
            }
        }
        if !unsupported.is_empty() {
            crate::warn!("{rel}: not supported yet, skipped: {}", unsupported.join(", "));
            // Short names for the settings page: "Meter=Image [x]" -> "Image".
            let mut names: Vec<String> = unsupported
                .iter()
                .filter_map(|u| u.split('=').nth(1).and_then(|r| r.split(' ').next()).map(|n| {
                    let mut c = n.chars();
                    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
                }))
                .collect();
            names.sort();
            names.dedup();
            skin.unsupported = names;
        }
        Ok(skin)
    }

    /// Inlines @include files into the section list, registering [Variables] as it goes.
    fn flatten(&mut self, ini: Ini, out: &mut Vec<Section>, seen: &mut HashSet<PathBuf>, depth: u32) {
        for section in ini.sections {
            if section.name.eq_ignore_ascii_case("Variables") {
                for (k, v) in &section.entries {
                    if k.to_ascii_lowercase().starts_with("@include") {
                        let mut path = PathBuf::from(unquote(&self.expand(v)));
                        // A bare file name is relative to the skin's own folder, as in Rainmeter.
                        if path.is_relative() {
                            if let Some(dir) = self.file.parent() {
                                path = dir.join(path);
                            }
                        }
                        if depth < 8 && seen.insert(path.clone()) {
                            self.watched.push((path.clone(), mtime(&path)));
                            match Ini::load(&path) {
                                Ok(inc) => self.flatten(inc, out, seen, depth + 1),
                                Err(e) => crate::warn!("include {} failed: {e}", path.display()),
                            }
                        }
                    } else {
                        self.vars.insert(k.to_ascii_lowercase(), v.trim().to_string());
                    }
                }
            }
            match out.iter_mut().find(|s| s.name.eq_ignore_ascii_case(&section.name)) {
                Some(existing) => existing.entries.extend(section.entries),
                None => out.push(section),
            }
        }
    }

    fn apply(&mut self, o: &Overrides) {
        if let Some(h24) = o.hour24 {
            // Skins like Summit switch with Format=H/I plus Hidden/Hidden2 for the two meters.
            if self.vars.get("format").is_some_and(|v| matches!(unquote(v), "H" | "I")) {
                self.vars.insert("format".into(), if h24 { "H" } else { "I" }.into());
            }
            if self.vars.contains_key("hidden") && self.vars.contains_key("hidden2") {
                self.vars.insert("hidden".into(), if h24 { "0" } else { "1" }.into());
                self.vars.insert("hidden2".into(), if h24 { "1" } else { "0" }.into());
            }
        }
        if let Some(c) = &o.color {
            for (k, v) in self.vars.iter_mut() {
                if k.ends_with("color") {
                    *v = c.clone();
                }
            }
        }
    }

    /// True when the skin or one of its includes changed on disk since load.
    pub fn changed_on_disk(&self) -> bool {
        self.watched.iter().any(|(p, t)| mtime(p) != *t)
    }

    /// Replaces #Name# with variable values, repeatedly, so variables may reference variables.
    pub fn expand(&self, s: &str) -> String {
        let mut cur = s.to_string();
        for _ in 0..10 {
            let mut out = String::with_capacity(cur.len());
            let mut rest = cur.as_str();
            let mut changed = false;
            while let Some(start) = rest.find('#') {
                out.push_str(&rest[..start]);
                let after = &rest[start + 1..];
                match after.find('#') {
                    Some(end) => match self.vars.get(&after[..end].to_ascii_lowercase()) {
                        Some(v) => {
                            out.push_str(v);
                            rest = &after[end + 1..];
                            changed = true;
                        }
                        None => {
                            out.push('#');
                            rest = after;
                        }
                    },
                    None => {
                        out.push('#');
                        rest = after;
                    }
                }
            }
            out.push_str(rest);
            cur = out;
            if !changed {
                break;
            }
        }
        cur
    }

    fn opt(&self, s: &Section, key: &str) -> Option<String> {
        s.get(key).map(|v| unquote(&self.expand(v)).to_string())
    }

    /// Evaluates a number or formula; `[Meter:X|Y|W|H]` read the previous layout pass.
    fn eval(&self, v: &str) -> Option<f64> {
        formula::number_with(v, &|name| {
            let (meter, prop) = name.split_once(':')?;
            let geo = self.geo.borrow();
            let g = geo.get(&meter.trim().to_ascii_lowercase())?;
            Some(match prop.trim().to_ascii_lowercase().as_str() {
                "x" => g[0],
                "y" => g[1],
                "w" => g[2],
                "h" => g[3],
                _ => return None,
            })
        })
    }

    fn num(&self, s: &Section, key: &str, default: f64) -> f64 {
        self.opt(s, key).and_then(|v| self.eval(&v)).unwrap_or(default)
    }

    fn measure_value(&self, name: &str, now: &SYSTEMTIME) -> String {
        let Some(m) = self.measures.get(&name.trim().to_ascii_lowercase()) else { return String::new() };
        if m.kind != "time" {
            return String::new();
        }
        let mut v = time::format(&m.format, now);
        if !m.regex.is_empty() {
            for (re, rep) in &m.regex {
                v = re.replace_all(&v, rep.as_str()).into_owned();
            }
        } else {
            for (from, to) in &m.substitutes {
                if !from.is_empty() {
                    v = v.replace(from.as_str(), to);
                }
            }
        }
        v
    }

    /// Lays the skin out at `now`. `measure` returns the pixel width and height of a string.
    /// Every FontFace the skin's string meters use (lower-case), so only those fonts get loaded.
    pub fn font_faces(&self) -> HashSet<String> {
        self.meters
            .iter()
            .filter_map(|m| self.opt(m, "FontFace"))
            .map(|f| f.trim().to_lowercase())
            .filter(|f| !f.is_empty())
            .collect()
    }

    /// Lays the skin out at `now`. Two passes, so a meter can use the size or position of a
    /// meter that comes after it (`[MeterDate:Y]`), as Rainmeter skins often do.
    pub fn layout(&self, now: &SYSTEMTIME, measure: &mut dyn FnMut(&str, &TextStyle) -> (f32, f32)) -> Scene {
        let _ = self.layout_pass(now, measure);
        self.layout_pass(now, measure)
    }

    fn layout_pass(&self, now: &SYSTEMTIME, measure: &mut dyn FnMut(&str, &TextStyle) -> (f32, f32)) -> Scene {
        let mut scene = Scene::default();
        let mut geo: HashMap<String, [f64; 4]> = HashMap::new();
        // Previous meter's anchor and size, for relative positioning.
        let (mut px, mut py, mut pw, mut ph) = (0i32, 0i32, 0i32, 0i32);
        let (mut max_r, mut max_b) = (0f32, 0f32);

        for m in &self.meters {
            let kind = m.get("Meter").unwrap_or("").to_ascii_lowercase();
            let hidden = self.num(m, "Hidden", 0.0) != 0.0;

            let x = self.position(m, "X", px, pw);
            let y = self.position(m, "Y", py, ph);
            let (w, h);
            let solid = self.opt(m, "SolidColor").and_then(|c| parse_color(&c)).filter(|c| c[3] > 0.0);

            match kind.as_str() {
                "string" => {
                    let style = self.text_style(m);
                    let text = self.meter_text(m, now);
                    let (tw, th) = if text.is_empty() { (0.0, 0.0) } else { measure(&text, &style) };
                    let stretch = self.opt(m, "TransformationMatrix").and_then(|t| t.split(';').next().and_then(|a| self.eval(a))).map_or(1.0, |v| v as f32).max(0.05);
                    let align = match self.opt(m, "StringAlign").unwrap_or_default().to_ascii_lowercase() {
                        a if a.starts_with("center") => Align::Center,
                        a if a.starts_with("right") => Align::Right,
                        _ => Align::Left,
                    };
                    let left = match align {
                        Align::Left => x as f32,
                        Align::Center => x as f32 - tw / 2.0,
                        Align::Right => x as f32 - tw,
                    };
                    w = tw.ceil() as i32;
                    h = th.ceil() as i32;
                    if !hidden {
                        if let Some(c) = solid {
                            let (bw, bh) = (self.num(m, "W", tw as f64) as f32, self.num(m, "H", th as f64) as f32);
                            scene.items.push(Item::Rect { x: left, y: y as f32, w: bw, h: bh, radius: 0.0, fill: c, stroke: [0.0; 4], stroke_width: 0.0 });
                        }
                        if !text.is_empty() {
                            let color = self.color(&self.opt(m, "FontColor").unwrap_or_default(), [0.0, 0.0, 0.0, 1.0]);
                            max_r = max_r.max(left + tw * stretch);
                            max_b = max_b.max(y as f32 + th);
                            scene.items.push(Item::Text { text, style, color, x: left, y: y as f32, stretch });
                        }
                    }
                }
                "image" => {
                    let iw = self.num(m, "W", 0.0) as f32;
                    let ih = self.num(m, "H", 0.0) as f32;
                    // Icons are square when only a width is given; the picture keeps its own
                    // proportions when it is drawn, so this only matters for layout references.
                    w = iw.round() as i32;
                    h = if ih > 0.0 { ih.round() as i32 } else { iw.round() as i32 };
                    if !hidden {
                        if let (Some(c), true, true) = (solid, iw > 0.0, ih > 0.0) {
                            scene.items.push(Item::Rect { x: x as f32, y: y as f32, w: iw, h: ih, radius: 0.0, fill: c, stroke: [0.0; 4], stroke_width: 0.0 });
                            max_r = max_r.max(x as f32 + iw);
                            max_b = max_b.max(y as f32 + ih);
                        }
                        if let Some(name) = self.opt(m, "ImageName").filter(|n| !n.trim().is_empty()) {
                            let mut path = PathBuf::from(name.trim());
                            if path.is_relative() {
                                let base = self.opt(m, "ImagePath").map(PathBuf::from).or_else(|| self.file.parent().map(|d| d.to_path_buf())).unwrap_or_default();
                                path = base.join(path);
                            }
                            let alpha = self.num(m, "ImageAlpha", 255.0).clamp(0.0, 255.0) as f32 / 255.0;
                            let tint = self.opt(m, "ImageTint").and_then(|t| parse_color(&t)).unwrap_or([1.0; 4]);
                            if iw > 0.0 {
                                max_r = max_r.max(x as f32 + iw);
                                max_b = max_b.max(y as f32 + if ih > 0.0 { ih } else { iw });
                            }
                            scene.items.push(Item::Image { path: path.display().to_string(), x: x as f32, y: y as f32, w: iw, h: ih, alpha, tint });
                        }
                    }
                }
                "shape" => {
                    let (mut sw, mut sh) = (0f32, 0f32);
                    for key in shape_keys(m) {
                        let Some(def) = self.opt(m, &key) else { continue };
                        if let Some(item) = self.shape(&def, x as f32, y as f32) {
                            if let Item::Rect { x: rx, y: ry, w: rw, h: rh, stroke_width, .. } = &item {
                                let pad = stroke_width / 2.0;
                                sw = sw.max(rx + rw + pad - x as f32);
                                sh = sh.max(ry + rh + pad - y as f32);
                            }
                            if !hidden {
                                scene.items.push(item);
                            }
                        }
                    }
                    w = sw.ceil() as i32;
                    h = sh.ceil() as i32;
                    if !hidden {
                        max_r = max_r.max(x as f32 + sw);
                        max_b = max_b.max(y as f32 + sh);
                    }
                }
                _ => {
                    w = self.num(m, "W", 0.0) as i32;
                    h = self.num(m, "H", 0.0) as i32;
                }
            }

            geo.insert(m.name.to_ascii_lowercase(), [x as f64, y as f64, w as f64, h as f64]);
            // String meters anchor relative positioning on their X (the align point).
            px = x;
            py = y;
            pw = w;
            ph = h;
        }

        *self.geo.borrow_mut() = geo;
        scene.width = max_r.ceil().max(1.0) as u32;
        scene.height = max_b.ceil().max(1.0) as u32;
        scene
    }

    fn position(&self, m: &Section, key: &str, prev: i32, prev_size: i32) -> i32 {
        let raw = self.opt(m, key).unwrap_or_else(|| "0".into());
        let raw = raw.trim();
        let (body, rel) = match raw.chars().last() {
            Some('r') => (&raw[..raw.len() - 1], Some(false)),
            Some('R') => (&raw[..raw.len() - 1], Some(true)),
            _ => (raw, None),
        };
        let v = self.eval(body).unwrap_or(0.0) as i32;
        match rel {
            None => v,
            Some(false) => prev + v,
            Some(true) => prev + prev_size + v,
        }
    }

    fn text_style(&self, m: &Section) -> TextStyle {
        let style = self.opt(m, "StringStyle").unwrap_or_default().to_ascii_lowercase();
        let mut weight = if style.contains("bold") { 700 } else { 400 };
        if let Some(fw) = self.opt(m, "FontWeight").and_then(|v| self.eval(&v)) {
            weight = fw.clamp(1.0, 999.0) as u32;
        }
        let mut spacing = [0.0; 3];
        for key in ["InlineSetting", "InlineSetting2", "InlineSetting3", "InlineSetting4"] {
            let Some(v) = self.opt(m, key) else { continue };
            let parts: Vec<&str> = v.split('|').map(str::trim).collect();
            if parts.first().is_some_and(|p| p.eq_ignore_ascii_case("CharacterSpacing")) {
                for (i, p) in parts.iter().skip(1).take(3).enumerate() {
                    spacing[i] = self.eval(p).unwrap_or(0.0) as f32;
                }
            }
        }
        TextStyle {
            face: self.opt(m, "FontFace").unwrap_or_else(|| "Arial".into()),
            size: self.num(m, "FontSize", 10.0) as f32 * 4.0 / 3.0,
            weight,
            italic: style.contains("italic"),
            spacing,
        }
    }

    fn meter_text(&self, m: &Section, now: &SYSTEMTIME) -> String {
        let mut values = Vec::new();
        if let Some(name) = self.opt(m, "MeasureName") {
            values.push(self.measure_value(&name, now));
        }
        for i in 2..10 {
            match self.opt(m, &format!("MeasureName{i}")) {
                Some(name) => values.push(self.measure_value(&name, now)),
                None => break,
            }
        }
        let mut text = match self.opt(m, "Text") {
            Some(t) => {
                let mut t = t;
                for (i, v) in values.iter().enumerate().rev() {
                    t = t.replace(&format!("%{}", i + 1), v);
                }
                t
            }
            None => values.first().cloned().unwrap_or_default(),
        };
        text = text.replace("\\n", "\n");
        match self.opt(m, "StringCase").unwrap_or_default().to_ascii_lowercase().as_str() {
            "upper" => text.to_uppercase(),
            "lower" => text.to_lowercase(),
            "proper" => proper_case(&text),
            _ => text,
        }
    }

    fn color(&self, v: &str, default: Rgba) -> Rgba {
        parse_color(v).unwrap_or(default)
    }

    /// Parses `Rectangle x,y,w,h[,r] | Fill Color c | Stroke Color c | StrokeWidth n`.
    fn shape(&self, def: &str, mx: f32, my: f32) -> Option<Item> {
        let mut parts = def.split('|').map(str::trim);
        let head = parts.next()?;
        let (kind, args) = head.split_once(char::is_whitespace)?;
        if !kind.eq_ignore_ascii_case("Rectangle") {
            return None;
        }
        let a: Vec<f32> = split_args(args).iter().filter_map(|s| self.eval(s)).map(|v| v as f32).collect();
        if a.len() < 4 {
            return None;
        }
        let mut fill = [1.0, 1.0, 1.0, 1.0];
        let mut stroke = [0.0, 0.0, 0.0, 1.0];
        let mut stroke_width = 1.0;
        for p in parts {
            let lower = p.to_ascii_lowercase();
            if let Some(c) = lower.strip_prefix("fill color") {
                fill = parse_color(c.trim()).unwrap_or(fill);
            } else if let Some(c) = lower.strip_prefix("stroke color") {
                stroke = parse_color(c.trim()).unwrap_or(stroke);
            } else if let Some(n) = lower.strip_prefix("strokewidth") {
                stroke_width = self.eval(n.trim()).unwrap_or(1.0) as f32;
            }
        }
        Some(Item::Rect {
            x: mx + a[0],
            y: my + a[1],
            w: a[2],
            h: a[3],
            radius: a.get(4).copied().unwrap_or(0.0),
            fill,
            stroke,
            stroke_width,
        })
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn shape_keys(m: &Section) -> Vec<String> {
    let mut keys = vec!["Shape".to_string()];
    for i in 2..64 {
        let k = format!("Shape{i}");
        if m.get(&k).is_none() {
            break;
        }
        keys.push(k);
    }
    keys
}

/// Splits on commas that are not inside parentheses.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).collect()
}

/// `r,g,b[,a]` (0-255, formulas allowed) or hex `RRGGBB[AA]`.
pub fn parse_color(v: &str) -> Option<Rgba> {
    let v = v.trim();
    if v.contains(',') {
        let n: Vec<f64> = split_args(v).iter().map(|s| formula::number(s)).collect::<Option<_>>()?;
        if n.len() < 3 {
            return None;
        }
        let c = |i: usize| (n.get(i).copied().unwrap_or(255.0).clamp(0.0, 255.0) / 255.0) as f32;
        return Some([c(0), c(1), c(2), c(3)]);
    }
    let hex = v.trim_start_matches('#');
    if (hex.len() == 6 || hex.len() == 8) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let b = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(255) as f32 / 255.0;
        return Some([b(0), b(2), b(4), if hex.len() == 8 { b(6) } else { 1.0 }]);
    }
    None
}

/// `"a":"b","c":"d"` → [(a, b), (c, d)]. Quotes are optional; separators inside quotes
/// are literal.
fn parse_substitute(v: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut parts: Vec<String> = vec![String::new()];
    let mut in_quote = false;
    let flush = |parts: &mut Vec<String>, pairs: &mut Vec<(String, String)>| {
        if parts.len() == 2 {
            pairs.push((parts[0].clone(), parts[1].clone()));
        }
        parts.clear();
        parts.push(String::new());
    };
    for c in v.chars() {
        match c {
            '"' => in_quote = !in_quote,
            ':' if !in_quote => parts.push(String::new()),
            ',' if !in_quote => flush(&mut parts, &mut pairs),
            c if c.is_whitespace() && !in_quote => {}
            _ => parts.last_mut().unwrap().push(c),
        }
    }
    flush(&mut parts, &mut pairs);
    pairs
}

fn proper_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if start {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        start = c.is_whitespace();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_and_colors() {
        assert_eq!(parse_substitute(r#""AM":"am","PM":"pm""#), vec![("AM".into(), "am".into()), ("PM".into(), "pm".into())]);
        assert_eq!(parse_substitute(r#""Friday":"Fri","17":"evening""#)[1], ("17".into(), "evening".into()));
        assert_eq!(parse_color("255,255,255"), Some([1.0, 1.0, 1.0, 1.0]));
        assert_eq!(parse_color("255,255,255,0"), Some([1.0, 1.0, 1.0, 0.0]));
        assert_eq!(parse_color("FF0000"), Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(split_args("1,1,(1*1.3),(58*1.3),0").len(), 5);
    }
}
