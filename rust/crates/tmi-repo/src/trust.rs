//! Trust in a repository's format plugins (PLUGINS.md 5; the user's
//! decision Q1, 2026-10-17).
//!
//! `plugin.trust` (repository config, else the user's) says how a plugin
//! may run:
//! - `notice` (the default): reading through a plugin (add, status, diff,
//!   import) is sandboxed and only told about, once per repository and
//!   component (stderr), never stopped;
//! - `prompt`: reading asks once per repository and component (a terminal;
//!   anywhere else it is refused until `tmiki plugin trust`);
//! - `deny`: plugins do not run (PLUGINS.md 4.4's "not available");
//! - `allow`: no notice, no question, and writing allowed.
//!
//! Writing into the working tree through a plugin (merge write-back,
//! `restore --in-place`; the writers come with P7) always needs the user's
//! consent once per repository and component, under every setting but
//! `allow`: [`require_write_consent`] asks on a terminal, and elsewhere
//! refuses with how to grant it (`tmiki plugin trust <id>`).
//!
//! What was told or granted is kept in the user's `trust.json`, beside the
//! user config (`TMI_CONFIG_HOME`, `%APPDATA%\tmi`, `~/.config/tmi`):
//! `{"version": 1, "repositories": {<repo>: {<component sha256>: {"id",
//! "version", "notified", "read", "write"}}}}`, `<repo>` being the real
//! path of the repository's `.tmi`.

use crate::error::{RepoError, Result};
use crate::repo::Repo;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tmi_store::json::{self, Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Notice,
    Prompt,
    Deny,
    Allow,
}

impl Setting {
    pub fn name(self) -> &'static str {
        match self {
            Setting::Notice => "notice",
            Setting::Prompt => "prompt",
            Setting::Deny => "deny",
            Setting::Allow => "allow",
        }
    }
}

/// `plugin.trust`: the repository's config, else the user's.
pub fn setting(repo: &Repo) -> Result<Setting> {
    let repo_cfg = repo.read_config()?;
    let user_cfg = crate::config::read_user_config()?;
    let raw = match crate::config::get_value(&repo_cfg, "plugin.trust")? {
        Some(v) => Some(v.clone()),
        None => crate::config::get_value(&user_cfg, "plugin.trust")?.cloned(),
    };
    match raw {
        None | Some(Value::Null) => Ok(Setting::Notice),
        Some(Value::Str(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "notice" => Ok(Setting::Notice),
            "prompt" => Ok(Setting::Prompt),
            "deny" => Ok(Setting::Deny),
            "allow" => Ok(Setting::Allow),
            other => Err(RepoError::runtime(format!("plugin.trust is {other:?}: use notice, prompt, deny or allow"))),
        },
        Some(other) => Err(RepoError::runtime(format!("plugin.trust is {}: use notice, prompt, deny or allow", json::dumps(&other, &json::DumpOptions::CANONICAL)))),
    }
}

/// `trust.json`, beside the user config.
pub fn store_path() -> PathBuf {
    let config = crate::config::user_config_path();
    config.parent().map_or_else(|| PathBuf::from("trust.json"), |p| p.join("trust.json"))
}

fn repo_key(repo: &Repo) -> String {
    crate::py::realpath(&repo.repo_dir).display().to_string()
}

/// The component a decision is about.
#[derive(Clone, Debug)]
pub struct Component<'a> {
    pub id: &'a str,
    pub version: &'a str,
    pub sha256: &'a str,
}

/// The whole of `trust.json`: `repositories` (format plugins, above) and
/// `directories` (plugin directories whose code `tmiki plugin build` /
/// `test --native` may run: [`require_dir_consent`]).
fn load_top() -> Map {
    let Ok(bytes) = std::fs::read(store_path()) else { return Map::new() };
    match json::loads(&String::from_utf8_lossy(&bytes)) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn section(key: &str) -> Map {
    match load_top().get(key) {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    }
}

fn load() -> Map {
    section("repositories")
}

fn save(repos: Map) -> Result<()> {
    save_section("repositories", repos)
}

fn save_section(name: &str, entries: Map) -> Result<()> {
    let path = store_path();
    let mut top = load_top();
    top.insert("version".into(), Value::Int(json::Int::Small(1)));
    top.insert(name.into(), Value::Object(entries));
    let mut text = json::dumps(&Value::Object(top), &json::DumpOptions::INDENTED);
    text.push('\n');
    let fail = |e: std::io::Error| RepoError::runtime(format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let tmp = path.with_file_name(format!(".trust.json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text.as_bytes()).map_err(fail)?;
    crate::fsio::rename_over(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        fail(e)
    })
}

/// The record of `c` in `repo`, if any.
fn record(repo: &Repo, c: &Component) -> Option<Map> {
    load().get(&repo_key(repo))?.get(c.sha256)?.as_object().cloned()
}

fn flag(rec: &Option<Map>, key: &str) -> bool {
    rec.as_ref().and_then(|r| r.get(key)).is_some_and(Value::truthy)
}

/// Sets `keys` on `c`'s record in `repo` (and keeps id / version current).
fn update(repo: &Repo, c: &Component, keys: &[(&str, bool)]) -> Result<()> {
    let mut repos = load();
    let key = repo_key(repo);
    let mut comps = repos.get(&key).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut rec = comps.get(c.sha256).and_then(Value::as_object).cloned().unwrap_or_default();
    rec.insert("id".into(), Value::Str(c.id.into()));
    rec.insert("version".into(), Value::Str(c.version.into()));
    for (k, v) in keys {
        rec.insert((*k).into(), Value::Bool(*v));
    }
    comps.insert(c.sha256.into(), Value::Object(rec));
    repos.insert(key, Value::Object(comps));
    save(repos)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// What the executable does around a question (issue #48): `pause` stops
/// and erases its progress spinner, `resume` lets it go on. Without them
/// (a library caller) the question is simply asked.
static PROMPT_HOOKS: OnceLock<(fn(), fn())> = OnceLock::new();

/// One question at a time (imports may run on several threads).
static ASKING: Mutex<()> = Mutex::new(());

/// Sets what runs before and after every question on the terminal.
pub fn set_prompt_hooks(pause: fn(), resume: fn()) {
    let _ = PROMPT_HOOKS.set((pause, resume));
}

/// Asks `question` on the terminal; false when there is none. The progress
/// spinner is held while the question waits (issue #48).
fn ask(question: &str) -> bool {
    // the question holds text a plugin made (its id, version): no terminal controls
    let question = tmi_format::safe_text::message(question);
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return false;
    }
    let _one = ASKING.lock().unwrap_or_else(|e| e.into_inner());
    let hooks = PROMPT_HOOKS.get();
    if let Some((pause, _)) = hooks {
        pause();
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    let read = std::io::stdin().read_line(&mut answer);
    if let Some((_, resume)) = hooks {
        resume();
    }
    read.is_ok() && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// A line on stderr, on a line of its own (the spinner erased first and
/// drawn again after it; issue #48).
fn say(line: &str) {
    let line = tmi_format::safe_text::message(line);
    let _one = ASKING.lock().unwrap_or_else(|e| e.into_inner());
    let hooks = PROMPT_HOOKS.get();
    if let Some((pause, _)) = hooks {
        pause();
    }
    eprintln!("{line}");
    if let Some((_, resume)) = hooks {
        resume();
    }
}

/// What a plugin can and cannot do, said when the user first meets it.
const SANDBOX: &str = "It runs in a sandbox: it reads only the file it is given and cannot reach your other files or the network.";

/// `plugin ora 0.2.0 (sha256 39b43f8fa40b...)`.
fn named(c: &Component) -> String {
    format!("format plugin {} {} (sha256 {}...)", c.id, c.version, short(c.sha256))
}

/// A development plugin (`tmiki plugin dev`, `TMI_PLUGIN_DEV`; PLUGINS.md 5): the
/// user's own program, run because they named it for this command. No
/// notice, question or `plugin.trust` applies to it (nor a record in
/// `trust.json`: there is no component to remember); what keeps its work out
/// of the history is that no commit takes its records.
pub fn is_dev(c: &Component) -> bool {
    c.sha256 == tmi_format::pipeline::DEV_COMPONENT
}

/// Before a plugin reads a file of `repo`: Ok (after the notice or the
/// question the setting asks for), or Err(why it may not run).
pub fn check_read(repo: &Repo, c: &Component) -> std::result::Result<(), String> {
    if is_dev(c) {
        return Ok(());
    }
    let s = setting(repo).map_err(|e| e.to_string())?;
    match s {
        Setting::Allow => Ok(()),
        Setting::Deny => Err("plugin.trust is deny".into()),
        Setting::Notice => {
            let rec = record(repo, c);
            if !flag(&rec, "notified") && !flag(&rec, "read") {
                // what happened and how to change it (issue #51)
                say(&format!(
                    "tmiki: this repository uses {}. {SANDBOX} To be asked before a plugin runs: tmiki config --global plugin.trust prompt (deny turns plugins off).",
                    named(c)
                ));
                // a notice that cannot be recorded is shown again next time
                let _ = update(repo, c, &[("notified", true)]);
            }
            Ok(())
        }
        Setting::Prompt => {
            if flag(&record(repo, c), "read") {
                return Ok(());
            }
            if ask(&format!(
                "tmiki: this repository uses {}. {SANDBOX} Let it read files in this repository (a yes is remembered; 'tmiki plugin trust {}' also allows it)?",
                named(c),
                c.id
            )) {
                update(repo, c, &[("notified", true), ("read", true)]).map_err(|e| e.to_string())?;
                return Ok(());
            }
            Err(format!("plugin.trust is prompt and it was not allowed; run 'tmiki plugin trust {}' to allow it", tmi_format::safe_text::line(c.id)))
        }
    }
}

/// Before a plugin writes `rel` in the working tree (merge write-back,
/// `restore --in-place`; PLUGINS.md P7 calls this): consent once per
/// repository and component, asked on a terminal, refused elsewhere.
pub fn require_write_consent(repo: &Repo, c: &Component, rel: &str) -> Result<()> {
    if is_dev(c) {
        return Ok(());
    }
    let s = setting(repo)?;
    if s == Setting::Allow || flag(&record(repo, c), "write") {
        return Ok(());
    }
    if s == Setting::Deny {
        return Err(RepoError::runtime(format!("{rel}: plugin '{}' may not run here (plugin.trust is deny)", tmi_format::safe_text::line(c.id))));
    }
    if ask(&format!(
        "tmiki: let format plugin {} {} (sha256 {}...) write files in this working tree (first: {rel})?",
        c.id,
        c.version,
        short(c.sha256)
    )) {
        update(repo, c, &[("notified", true), ("read", true), ("write", true)])?;
        return Ok(());
    }
    Err(RepoError::runtime(format!(
        "{rel}: writing through plugin '{}' {} needs your consent once for this repository; run 'tmiki plugin trust {}' (or set plugin.trust=allow)",
        tmi_format::safe_text::line(c.id),
        tmi_format::safe_text::line(c.version),
        tmi_format::safe_text::line(c.id)
    )))
}

/// `tmiki plugin trust <id>`: reading and writing allowed for `c` here.
pub fn grant(repo: &Repo, c: &Component) -> Result<()> {
    update(repo, c, &[("notified", true), ("read", true), ("write", true)])
}

/// `tmiki plugin add`: the user put `c` here, so reading needs no notice or
/// question; writing still asks.
pub fn grant_read(repo: &Repo, c: &Component) -> Result<()> {
    update(repo, c, &[("notified", true), ("read", true)])
}

/// `tmiki plugin trust --revoke <id>`: what was granted for `c` here is gone
/// (the notice stays told).
pub fn revoke(repo: &Repo, c: &Component) -> Result<()> {
    update(repo, c, &[("read", false), ("write", false)])
}

/// The trust column of `tmiki plugin list`.
pub fn describe(repo: &Repo, c: &Component) -> Result<String> {
    let s = setting(repo)?;
    let rec = record(repo, c);
    Ok(match (s, flag(&rec, "write"), flag(&rec, "read"), flag(&rec, "notified")) {
        (Setting::Allow, ..) => "allow".into(),
        (Setting::Deny, ..) => "deny".into(),
        (_, true, ..) => "trusted (read, write)".into(),
        (_, false, true, _) => "trusted (read)".into(),
        (Setting::Prompt, ..) => "prompt (not asked yet)".into(),
        (Setting::Notice, _, _, true) => "notice (told)".into(),
        (Setting::Notice, ..) => "notice".into(),
    })
}

// -- plugin directories: running somebody else's code (PLUGINS.md 11) ---------

/// What a plugin directory asks tmi to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirAction {
    /// `tmiki plugin build`: cargo runs the directory's build scripts, proc
    /// macros and `.cargo/config.toml`.
    Build,
    /// `tmiki plugin test --native` taking `[dev] command` from the manifest:
    /// a program of the directory (a bundled executable even), not sandboxed.
    DevCommand,
}

impl DirAction {
    fn name(self) -> &'static str {
        match self {
            DirAction::Build => "build",
            DirAction::DevCommand => "dev-command",
        }
    }
}

/// The environment variable that says yes for scripts and CI (an
/// interactive user is asked instead): `TMI_PLUGIN_DIR_TRUST=allow`.
pub const DIR_TRUST_ENV: &str = "TMI_PLUGIN_DIR_TRUST";

fn dir_key(dir: &std::path::Path) -> String {
    let real = crate::py::realpath(dir);
    let s = real.display().to_string();
    // the same directory however it is spelled (Windows: case, \\?\ prefix)
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
    if cfg!(windows) { s.to_ascii_lowercase() } else { s }
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut all = Vec::new();
    for p in parts {
        all.extend_from_slice(&(p.len() as u64).to_le_bytes());
        all.extend_from_slice(p);
    }
    tmi_store::oid::sha256_hex(&all)
}

/// The record key of `what` for `content`: its hash, so a changed command,
/// manifest or bundled program is asked about again.
fn dir_record_key(action: DirAction, content: &[u8]) -> String {
    format!("{}:{}", action.name(), sha256_hex(&[content]))
}

fn dir_granted(dir: &std::path::Path, action: DirAction, content: &[u8]) -> bool {
    section("directories")
        .get(&dir_key(dir))
        .and_then(Value::as_object)
        .is_some_and(|m| m.get(&dir_record_key(action, content)).is_some_and(Value::truthy))
}

/// Remembers that the user allowed `action` of `dir` for `content` (what
/// [`require_dir_consent`] was given). `plugin new` and `plugin build` use
/// it for what tmi itself made.
pub fn grant_dir(dir: &std::path::Path, action: DirAction, content: &[u8], detail: &str) -> Result<()> {
    let mut dirs = section("directories");
    let key = dir_key(dir);
    let mut rec = dirs.get(&key).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut entry = Map::new();
    entry.insert("action".into(), Value::Str(action.name().into()));
    entry.insert("what".into(), Value::Str(detail.chars().take(200).collect()));
    entry.insert("allowed".into(), Value::Bool(true));
    rec.insert(dir_record_key(action, content), Value::Object(entry));
    dirs.insert(key, Value::Object(rec));
    save_section("directories", dirs)
}

/// Before tmi runs code that came with a plugin directory (a clone, a
/// download: somebody else's): asks once for this directory (its real path)
/// and this `content` (the command, the bundled program's bytes, the
/// manifest), remembers a yes in `trust.json` (`directories`), and asks again
/// when the content changes. Without a terminal it refuses and says how to
/// allow it: run the command once on a terminal, or set `TMI_PLUGIN_DIR_TRUST=allow`.
pub fn require_dir_consent(dir: &std::path::Path, action: DirAction, content: &[u8], detail: &str) -> Result<()> {
    if std::env::var(DIR_TRUST_ENV).is_ok_and(|v| v.trim().eq_ignore_ascii_case("allow")) || dir_granted(dir, action, content) {
        return Ok(());
    }
    let shown = tmi_format::safe_text::line(&dir.display().to_string()).into_owned();
    let what = match action {
        DirAction::Build => "build it: cargo runs the build scripts and macros of that directory".to_string(),
        DirAction::DevCommand => format!("run its development plugin ({}), a program that is not sandboxed", tmi_format::safe_text::line(detail)),
    };
    let question = format!(
        "tmiki: {shown} asks to {what}. That is running its author's code with your permissions; do this only for a plugin directory you wrote or trust. Run it?"
    );
    if ask(&question) {
        return grant_dir(dir, action, content, detail);
    }
    Err(RepoError::runtime(format!(
        "{shown}: not running {} of a plugin directory without your consent (it is that author's code, not sandboxed). Run the command on a terminal to be asked once, or set {DIR_TRUST_ENV}=allow for this command if you trust the directory",
        match action {
            DirAction::Build => "its build",
            DirAction::DevCommand => "its [dev] command",
        }
    )))
}