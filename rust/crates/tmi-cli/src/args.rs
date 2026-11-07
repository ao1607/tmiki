//! argparse-equivalent parsing for the native commands, on clap.
//!
//! clap and argparse agree on ordinary command lines; where they could
//! differ the command goes to Python instead: abbreviated long options
//! (argparse accepts unique prefixes), "--", help, repeated options (clap
//! refuses, argparse keeps the last), positionals split by an option
//! (argparse refuses), integers argparse's `int()` reads differently, and
//! anything clap rejects.

use clap::{Arg, ArgAction, ArgMatches, Command};
use tmi_repo::RepoError;

pub fn flag(id: &'static str, short: Option<char>, long: Option<&'static str>) -> Arg {
    let mut a = Arg::new(id).action(ArgAction::SetTrue).num_args(0);
    if let Some(s) = short {
        a = a.short(s);
    }
    if let Some(l) = long {
        a = a.long(l);
    }
    a
}

pub fn opt(id: &'static str, short: Option<char>, long: Option<&'static str>) -> Arg {
    let mut a = Arg::new(id).action(ArgAction::Set).num_args(1);
    if let Some(s) = short {
        a = a.short(s);
    }
    if let Some(l) = long {
        a = a.long(l);
    }
    a
}

/// `nargs="?", const=...`: the value is optional.
pub fn opt_const(id: &'static str, short: Option<char>, long: Option<&'static str>, constant: &'static str) -> Arg {
    opt(id, short, long).num_args(0..=1).default_missing_value(constant)
}

pub fn positional(id: &'static str, many: bool) -> Arg {
    let a = Arg::new(id).action(if many { ArgAction::Append } else { ArgAction::Set });
    if many {
        a.num_args(0..)
    } else {
        a.num_args(1)
    }
}

fn fwd(why: impl Into<String>) -> RepoError {
    RepoError::forward(why)
}

/// Parse `args` with `cmd` (whose positionals are `positionals`), or Forward.
pub fn parse(cmd: Command, args: &[String], positionals: &[&str]) -> Result<ArgMatches, RepoError> {
    let cmd = cmd.no_binary_name(true).disable_help_flag(true).disable_version_flag(true);
    let mut longs: Vec<String> = Vec::new();
    for a in cmd.get_arguments() {
        if let Some(l) = a.get_long() {
            longs.push(l.to_string());
        }
        if let Some(aliases) = a.get_all_aliases() {
            longs.extend(aliases.into_iter().map(str::to_string));
        }
    }
    // "--" (stage 4b): what follows is positional, in argparse as in clap.
    // One "--", in a parser with positionals, not where an option expects
    // its value; the positionals around it then form one run (argparse
    // drops that "--" from them).
    let dash = args.iter().position(|a| a == "--");
    if let Some(at) = dash {
        if positionals.is_empty() || args.iter().filter(|a| *a == "--").count() > 1 {
            return Err(fwd("-- where argparse reports an error or keeps a later --"));
        }
        if at > 0 && takes_value(&cmd, &args[at - 1]) {
            return Err(fwd("-- as an option's value (argparse's error)"));
        }
    }
    let scan_end = dash.unwrap_or(args.len());
    for a in &args[..scan_end] {
        if a == "-h" || a == "--help" || a.starts_with("--help=") {
            return Err(fwd("help"));
        }
        if let Some(rest) = a.strip_prefix("--") {
            let name = rest.split('=').next().unwrap_or("");
            if !longs.iter().any(|l| l == name) {
                return Err(fwd(format!("option {a} is not one this parser knows exactly")));
            }
        }
    }
    let matches = cmd.try_get_matches_from(args).map_err(|e| fwd(format!("argument error: {}", e.kind())))?;
    // argparse matches positionals in one run; ones split by options fail there.
    let mut indices: Vec<usize> = Vec::new();
    for id in positionals {
        if let Some(ix) = matches.indices_of(id) {
            indices.extend(ix);
        }
    }
    if let Some(at) = dash {
        // clap's indices skip the "--" (and start at 1): the argument
        // before it has index `at`, the one after it `at + 1`. The "--"
        // must sit inside the run of positionals or at either end of it.
        if let (Some(&lo), Some(&hi)) = (indices.iter().min(), indices.iter().max()) {
            if hi < at || lo > at + 1 {
                return Err(fwd("-- apart from the positionals"));
            }
        }
    }
    indices.sort_unstable();
    if indices.windows(2).any(|w| w[1] != w[0] + 1) {
        return Err(fwd("positionals separated by options"));
    }
    Ok(matches)
}

/// Whether `arg` is an option of `cmd` that takes a value from the next
/// argument (`-b`, `--branch`; not `--branch=x` or `-bx`).
fn takes_value(cmd: &Command, arg: &str) -> bool {
    cmd.get_arguments().any(|a| {
        let takes = a.get_num_args().is_some_and(|n| n.takes_values()) && a.get_action().takes_values();
        let short = a.get_short().is_some_and(|s| arg == format!("-{s}"));
        let long = a.get_long().is_some_and(|l| arg == format!("--{l}"));
        takes && (short || long)
    })
}

pub fn get_str(m: &ArgMatches, id: &str) -> Option<String> {
    m.get_one::<String>(id).cloned()
}

pub fn get_flag(m: &ArgMatches, id: &str) -> bool {
    m.get_flag(id)
}

pub fn get_many(m: &ArgMatches, id: &str) -> Vec<String> {
    m.get_many::<String>(id).map(|v| v.cloned().collect()).unwrap_or_default()
}

/// argparse `type=int` for plain decimal text; Forward for what Python's
/// `int()` would also take ("+5", " 5", "1_0", other digits) or refuse.
pub fn get_int(m: &ArgMatches, id: &str) -> Result<Option<i64>, RepoError> {
    let Some(text) = m.get_one::<String>(id) else { return Ok(None) };
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 18 {
        return Err(fwd(format!("integer {text:?}")));
    }
    Ok(Some(text.parse().unwrap()))
}

/// Index of the last occurrence of an option (for "last one wins" pairs).
pub fn last_index(m: &ArgMatches, id: &str) -> Option<usize> {
    if m.value_source(id) != Some(clap::parser::ValueSource::CommandLine) {
        return None;
    }
    m.indices_of(id).and_then(|ix| ix.max())
}

/// `choices=[...]`: values outside are left to argparse's error.
pub fn check_choice(value: &Option<String>, choices: &[&str]) -> Result<(), RepoError> {
    match value {
        Some(v) if !choices.contains(&v.as_str()) => Err(fwd("invalid choice")),
        _ => Ok(()),
    }
}
