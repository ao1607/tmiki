//! What `tmiki plugin test` found: one entry per check and file, printed
//! grouped (an `ok` line per check, every failure and warning on its own).

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// The plugin breaks a rule: `test` ends with 1.
    Fail(String),
    /// Worth knowing, not a failure (PLUGINS.md 7.1: a trap in fuzzing,
    /// bytes that differ although the records are the same).
    Warn(String),
    /// A check that did not run, and why.
    Skip(String),
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// `1 native = wasm`, `2 determinism: ...` (PLUGINS.md 7.1's numbers).
    pub check: String,
    /// The corpus file, if the check is about one.
    pub file: Option<String>,
    /// What in the file (a layer).
    pub detail: Option<String>,
    pub outcome: Outcome,
}

#[derive(Default)]
pub struct Report {
    pub entries: Vec<Entry>,
    /// Lines printed before the summary (`--dump`, `--against`).
    pub notes: Vec<String>,
}

impl Report {
    pub fn add(&mut self, check: &str, file: Option<&str>, outcome: Outcome) {
        self.entries.push(Entry { check: check.to_string(), file: file.map(str::to_string), detail: None, outcome });
    }

    pub fn ok(&mut self, check: &str, file: Option<&str>) {
        self.add(check, file, Outcome::Ok);
    }

    pub fn fail(&mut self, check: &str, file: Option<&str>, why: impl Into<String>) {
        self.add(check, file, Outcome::Fail(why.into()));
    }

    pub fn warn(&mut self, check: &str, file: Option<&str>, why: impl Into<String>) {
        self.add(check, file, Outcome::Warn(why.into()));
    }

    pub fn skip(&mut self, check: &str, file: Option<&str>, why: impl Into<String>) {
        self.add(check, file, Outcome::Skip(why.into()));
    }

    /// `Ok` when `result` is, else a failure with its message.
    pub fn check(&mut self, check: &str, file: Option<&str>, result: Result<(), String>) -> bool {
        match result {
            Ok(()) => {
                self.ok(check, file);
                true
            }
            Err(e) => {
                self.fail(check, file, e);
                false
            }
        }
    }

    pub fn failures(&self) -> usize {
        self.entries.iter().filter(|e| matches!(e.outcome, Outcome::Fail(_))).count()
    }

    pub fn warnings(&self) -> usize {
        self.entries.iter().filter(|e| matches!(e.outcome, Outcome::Warn(_))).count()
    }

    /// The same as [`Report::check`], about `detail` (a layer) of `file`.
    pub fn check_in(&mut self, check: &str, file: &str, detail: &str, result: Result<(), String>) {
        let outcome = match result {
            Ok(()) => Outcome::Ok,
            Err(e) => Outcome::Fail(e),
        };
        self.add_in(check, file, detail, outcome);
    }

    pub fn add_in(&mut self, check: &str, file: &str, detail: &str, outcome: Outcome) {
        self.entries.push(Entry { check: check.to_string(), file: Some(file.to_string()), detail: Some(detail.to_string()), outcome });
    }

    /// The lines to print: notes, then each check (by the number it starts
    /// with, then in the order first seen) with its failures, warnings and
    /// skips.
    pub fn lines(&self) -> Vec<String> {
        let mut out = self.notes.clone();
        let mut order: Vec<&str> = Vec::new();
        let mut by: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
        for e in &self.entries {
            if !by.contains_key(e.check.as_str()) {
                order.push(&e.check);
            }
            by.entry(&e.check).or_default().push(e);
        }
        let number = |c: &str| c.split(' ').next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(u32::MAX);
        order.sort_by_key(|c| number(c));
        for check in order {
            let entries = &by[check];
            let oks: Vec<&&Entry> = entries.iter().filter(|e| e.outcome == Outcome::Ok).collect();
            let bad = entries.iter().any(|e| matches!(e.outcome, Outcome::Fail(_)));
            let mut files: Vec<&str> = oks.iter().filter_map(|e| e.file.as_deref()).collect();
            files.sort_unstable();
            files.dedup();
            let count = match files.len() {
                0 | 1 => String::new(),
                n => format!(" ({n} files)"),
            };
            if !oks.is_empty() && !bad {
                out.push(format!("ok    {check}{count}"));
            } else if !oks.is_empty() {
                out.push(format!("ok    {check} ({} of {})", oks.len(), entries.len()));
            }
            for e in entries {
                let at = match (&e.file, &e.detail) {
                    (Some(f), Some(d)) => format!("{f}, {d}: "),
                    (Some(f), None) => format!("{f}: "),
                    _ => String::new(),
                };
                match &e.outcome {
                    Outcome::Ok => {}
                    Outcome::Fail(m) => out.push(format!("FAIL  {check}: {at}{m}")),
                    Outcome::Warn(m) => out.push(format!("warn  {check}: {at}{m}")),
                    Outcome::Skip(m) => out.push(format!("skip  {check}: {at}{m}")),
                }
            }
        }
        let checks = self.entries.iter().filter(|e| !matches!(e.outcome, Outcome::Skip(_))).count();
        out.push(format!("{checks} checks: {} failed, {} warnings", self.failures(), self.warnings()));
        out
    }
}