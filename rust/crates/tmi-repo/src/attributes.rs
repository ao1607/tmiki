//! `.tmiattributes` (PLUGINS.md 4.1): which reader a file goes to.
//!
//! The file at the root of the working tree, tracked like any other, a
//! subset of gitattributes: one pattern and its attributes per line, `#`
//! starts a comment (a whole line, or from a word that begins with `#`).
//!
//! ```text
//! *.ora            format=plugin:ora
//! *.psd            format=binary       # opaque, whatever it holds
//! *.ora            on-error=binary     # a file the plugin cannot read: opaque
//! ```
//!
//! Patterns match as `.tmiignore`'s do (the path from the right, the whole
//! path, or the file name), but case-insensitively in ASCII on every OS (a
//! normcase that differs between systems would give different records).
//! For each attribute the last matching line wins. `format`: `plugin:<id>`
//! (the component `.tmiplugins/<id>.wasm`), `binary` (an opaque blob) or
//! `builtin` (tmi's own rules: `.clip`, `.mdp`, image signatures, else
//! opaque; the default). `on-error`: `refuse` (the default: a file the
//! plugin cannot read stops `add`) or `binary`. Unknown attributes are
//! ignored. `.tmiattributes` itself and `.tmiplugins/` always follow the
//! built-in rules.

use crate::error::Result;
use crate::py;
use std::path::Path;

pub const FILE: &str = ".tmiattributes";
/// Where the components live: `.tmiplugins/<id>.wasm` (PLUGINS.md 4.1, Q2).
pub const PLUGIN_DIR: &str = ".tmiplugins";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Format {
    Builtin,
    Binary,
    Plugin(String),
    /// A value tmi does not know (the line and the text): a file it maps
    /// cannot be imported.
    Invalid(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnError {
    Refuse,
    Binary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub format: Format,
    pub on_error: OnError,
}

impl Mapping {
    pub const BUILTIN: Mapping = Mapping { format: Format::Builtin, on_error: OnError::Refuse };

    pub fn plugin_id(&self) -> Option<&str> {
        match &self.format {
            Format::Plugin(id) => Some(id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct Rule {
    /// As written (for `plugin list`).
    pattern: String,
    /// ASCII-lowercased, `\` as `/`.
    folded: String,
    format: Option<Format>,
    on_error: Option<OnError>,
}

#[derive(Clone, Debug, Default)]
pub struct Attributes {
    rules: Vec<Rule>,
}

/// ASCII-only lowercase (PLUGINS.md 4.1: the same on every OS).
pub fn fold(s: &str) -> String {
    s.replace('\\', "/").to_ascii_lowercase()
}

fn matches(rel: &str, pat: &str) -> bool {
    if py::pure_posix_match(rel, pat).unwrap_or(false) || py::fnmatchcase(rel, pat).unwrap_or(false) {
        return true;
    }
    let name = rel.rsplit('/').next().unwrap_or("");
    py::fnmatchcase(name, pat).unwrap_or(false)
}

/// Whether `rel` is one of the files that say how others are read.
pub fn is_plugin_setup_path(rel: &str) -> bool {
    let r = fold(rel);
    r == fold(FILE) || r.starts_with(&format!("{}/", fold(PLUGIN_DIR)))
}

impl Attributes {
    pub fn parse(text: &str) -> Attributes {
        let mut rules = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace().take_while(|w| !w.starts_with('#'));
            let Some(pattern) = words.next() else { continue };
            let mut rule = Rule { pattern: pattern.to_string(), folded: fold(pattern), format: None, on_error: None };
            for word in words {
                let (key, value) = word.split_once('=').unwrap_or((word, ""));
                match key {
                    "format" => {
                        rule.format = Some(match value {
                            "builtin" => Format::Builtin,
                            "binary" => Format::Binary,
                            v => match v.strip_prefix("plugin:") {
                                Some(id) if valid_id(id) => Format::Plugin(id.to_string()),
                                _ => Format::Invalid(format!("{FILE} line {}: format={value} (use plugin:<id>, binary or builtin)", n + 1)),
                            },
                        })
                    }
                    "on-error" => {
                        rule.on_error = match value {
                            "refuse" => Some(OnError::Refuse),
                            "binary" => Some(OnError::Binary),
                            _ => Some(OnError::Refuse),
                        }
                    }
                    _ => {}
                }
            }
            rules.push(rule);
        }
        Attributes { rules }
    }

    /// The working tree's `.tmiattributes` (none: no rules).
    pub fn load(worktree: Option<&Path>) -> Result<Attributes> {
        let Some(root) = worktree else { return Ok(Attributes::default()) };
        match std::fs::read(root.join(FILE)) {
            Ok(bytes) => Ok(Attributes::parse(&String::from_utf8_lossy(&bytes))),
            Err(_) => Ok(Attributes::default()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// What `rel` (a path from the root, `/`-separated) maps to.
    pub fn lookup(&self, rel: &str) -> Mapping {
        if self.rules.is_empty() || is_plugin_setup_path(rel) {
            return Mapping::BUILTIN;
        }
        let path = fold(rel);
        let mut m = Mapping::BUILTIN;
        for r in &self.rules {
            if !matches(&path, &r.folded) {
                continue;
            }
            if let Some(f) = &r.format {
                m.format = f.clone();
            }
            if let Some(e) = r.on_error {
                m.on_error = e;
            }
        }
        m
    }

    /// Every plugin id the rules name, in order of first mention.
    pub fn plugin_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in &self.rules {
            if let Some(Format::Plugin(id)) = &r.format {
                if !out.contains(id) {
                    out.push(id.clone());
                }
            }
        }
        out
    }

    /// The patterns that map files to plugin `id`.
    pub fn patterns_for(&self, id: &str) -> Vec<String> {
        self.rules.iter().filter(|r| matches!(&r.format, Some(Format::Plugin(p)) if p == id)).map(|r| r.pattern.clone()).collect()
    }

    /// Whether some line maps `pattern` to plugin `id` exactly.
    pub fn has_line(&self, pattern: &str, id: &str) -> bool {
        self.rules.iter().any(|r| r.pattern == pattern && matches!(&r.format, Some(Format::Plugin(p)) if p == id))
    }
}

/// The plugin id of a component path `.tmiplugins/<id>.wasm`, if `rel` is
/// one.
pub fn component_id(rel: &str) -> Option<&str> {
    let rest = rel.strip_prefix(PLUGIN_DIR)?.strip_prefix('/')?;
    let id = rest.strip_suffix(".wasm")?;
    valid_id(id).then_some(id)
}

/// One line `remove_plugin` changed: the line as it was, and what is left
/// of it (None: the whole line went).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineChange {
    pub old: String,
    pub new: Option<String>,
}

/// `text` (a `.tmiattributes`) without `format=plugin:<id>` (`tmiki plugin
/// remove`, issue #50): the attribute is taken out of each line; a line
/// left with no attribute goes, comments and other lines stay as written.
pub fn remove_plugin(text: &str, id: &str) -> (String, Vec<LineChange>) {
    let target = format!("format=plugin:{id}");
    let mut out = String::with_capacity(text.len());
    let mut changes = Vec::new();
    for raw in text.split_inclusive('\n') {
        let body = raw.trim_end_matches(['\n', '\r']);
        let ending = &raw[body.len()..];
        let line = body.trim();
        if line.is_empty() || line.starts_with('#') {
            out.push_str(raw);
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let cut = words.iter().position(|w| w.starts_with('#')).unwrap_or(words.len());
        let (attrs, comment) = words.split_at(cut);
        if !attrs.iter().skip(1).any(|w| *w == target) {
            out.push_str(raw);
            continue;
        }
        let kept: Vec<&str> = attrs.iter().enumerate().filter(|(i, w)| *i == 0 || **w != target).map(|(_, w)| *w).collect();
        if kept.len() <= 1 {
            changes.push(LineChange { old: line.to_string(), new: None });
            continue;
        }
        let new = kept.iter().chain(comment.iter()).copied().collect::<Vec<_>>().join(" ");
        out.push_str(&new);
        out.push_str(ending);
        changes.push(LineChange { old: line.to_string(), new: Some(new) });
    }
    (out, changes)
}

/// A plugin id: `[a-z0-9-]{1,40}` (what `info` must say, PLUGINS.md 3.4).
pub fn valid_id(id: &str) -> bool {
    tmi_plugin_host::is_plugin_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_match_wins_per_attribute_ascii_case_folded() {
        let a = Attributes::parse(
            "# comment\n*.ora format=plugin:ora\n*.PSD   format=binary   # opaque\nscans/**/*.tif format=binary\n*.ora on-error=binary\nkeep/*.ora format=builtin\nodd/* format=what\n",
        );
        let plugin = |on_error| Mapping { format: Format::Plugin("ora".into()), on_error };
        assert_eq!(a.lookup("a/b.ora"), plugin(OnError::Binary));
        assert_eq!(a.lookup("A/B.ORA"), plugin(OnError::Binary), "ASCII case does not count, on any OS");
        assert_eq!(a.lookup("x.psd").format, Format::Binary);
        assert_eq!(a.lookup("scans/2026/x.TIF").format, Format::Binary);
        assert_eq!(a.lookup("x.tif"), Mapping::BUILTIN);
        assert_eq!(a.lookup("keep/x.ora").format, Format::Builtin);
        assert!(matches!(a.lookup("odd/x").format, Format::Invalid(m) if m.contains("line 7")));
        assert_eq!(a.lookup(".tmiplugins/ora.wasm"), Mapping::BUILTIN);
        assert_eq!(a.lookup(".tmiattributes"), Mapping::BUILTIN);
        assert_eq!(a.plugin_ids(), vec!["ora".to_string()]);
        assert_eq!(a.patterns_for("ora"), vec!["*.ora".to_string()]);
        assert!(Attributes::parse("").lookup("x.ora") == Mapping::BUILTIN);
        assert!(matches!(Attributes::parse("*.x format=plugin:Bad").lookup("a.x").format, Format::Invalid(_)));
    }

    #[test]
    fn a_device_name_is_no_plugin_id_so_con_wasm_is_never_opened() {
        for id in ["con", "nul", "aux", "prn", "com1", "lpt9"] {
            let a = Attributes::parse(&format!("*.png format=plugin:{id}\n"));
            assert!(matches!(a.lookup("x.png").format, Format::Invalid(_)), "{id}");
            assert!(a.plugin_ids().is_empty(), "{id}");
        }
        assert!(valid_id("console") && valid_id("com") && valid_id("com10"));
    }

    #[test]
    fn remove_plugin_takes_out_its_mapping_only() {
        let text = "# formats\n*.ora format=plugin:ora\r\n*.kra   format=plugin:krita\nart/*.ora format=plugin:ora on-error=binary  # art\n*.psd format=binary\n*.ora on-error=binary";
        let (out, changes) = remove_plugin(text, "ora");
        assert_eq!(out, "# formats\n*.kra   format=plugin:krita\nart/*.ora on-error=binary # art\n*.psd format=binary\n*.ora on-error=binary");
        assert_eq!(
            changes,
            vec![
                LineChange { old: "*.ora format=plugin:ora".into(), new: None },
                LineChange { old: "art/*.ora format=plugin:ora on-error=binary  # art".into(), new: Some("art/*.ora on-error=binary # art".into()) },
            ]
        );
        assert_eq!(Attributes::parse(&out).lookup("a.ora").plugin_id(), None);
        assert_eq!(remove_plugin(text, "other"), (text.to_string(), vec![]));
        assert_eq!(component_id(".tmiplugins/ora.wasm"), Some("ora"));
        assert_eq!(component_id(".tmiplugins/sub/ora.wasm"), None);
        assert_eq!(component_id("x/ora.wasm"), None);
    }
}
