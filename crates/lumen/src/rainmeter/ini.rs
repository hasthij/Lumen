//! Rainmeter-style INI reading: UTF-16/UTF-8 with or without BOM, case-insensitive keys,
//! section order preserved.

use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Section {
    pub name: String,
    /// Keys in file order. Later duplicates override earlier ones on lookup.
    pub entries: Vec<(String, String)>,
}

impl Section {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Ini {
    pub sections: Vec<Section>,
}

impl Ini {
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(Self::parse(&decode(&bytes)))
    }

    pub fn parse(text: &str) -> Self {
        let mut ini = Ini::default();
        let mut current: Option<Section> = None;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                if let Some(end) = line.find(']') {
                    if let Some(s) = current.take() {
                        ini.sections.push(s);
                    }
                    current = Some(Section { name: line[1..end].trim().to_string(), entries: Vec::new() });
                    continue;
                }
            }
            let Some(eq) = line.find('=') else { continue };
            let key = line[..eq].trim();
            if key.is_empty() || key.starts_with('#') {
                continue;
            }
            let value = line[eq + 1..].trim().to_string();
            if let Some(s) = current.as_mut() {
                s.entries.push((key.to_string(), value));
            }
        }
        if let Some(s) = current {
            ini.sections.push(s);
        }
        ini
    }
}

/// Decodes a text file the way Rainmeter does: UTF-16 LE/BE by BOM, otherwise UTF-8
/// (lossy, so a stray ANSI byte never stops a skin from loading).
pub fn decode(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| {
        let units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Strips one pair of surrounding double quotes, as Rainmeter does for option values.
pub fn unquote(v: &str) -> &str {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        &v[1..v.len() - 1]
    } else {
        v
    }
}
