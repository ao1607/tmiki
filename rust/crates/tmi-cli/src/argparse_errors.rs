//! argparse's errors for the command lines it refuses (stage 4b): Python
//! 3.13's `_parse_known_args` run over the parser table
//! (argparse_table.rs), far enough to know whether it raises and
//! with which text. Then the front door prints what `parser.error()` and
//! `parse_args()` print (the usage, "<prog>: error: <message>") and exits
//! with 2, instead of starting Python to say it.
//!
//! The port follows argparse's own steps: the 'O' / 'A' / '-' pattern of
//! the arguments (`_parse_optional`, prefixes and "-xVALUE" included),
//! positionals and optionals consumed alternately with the same regular
//! patterns (`_match_argument`, `_match_arguments_partial`), the value
//! conversions that can fail here (`type=int`, `choices`), mutually
//! exclusive groups, required arguments, and the extras the top parser
//! reports. A help action met on the way, a value whose `int()` or
//! `repr()` is not reproduced, or a parser outside the table: `Unsure`
//! (the command goes on as before: natively, or to Python).

use crate::argparse_table::{PAct, PParser, NESTED, PARSERS, TOP_CHOICES, TOP_USAGE};

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// argparse accepts the arguments.
    Ok,
    /// What argparse prints to stderr before exiting with 2.
    Error(String),
    /// Not decided here.
    Unsure,
}

struct Unsure;

enum Fail {
    /// `ArgumentError(action, msg)` or `ArgumentError(None, msg)` raised in
    /// the command's parser.
    Arg(String),
    Unsure,
}

impl From<Unsure> for Fail {
    fn from(_: Unsure) -> Fail {
        Fail::Unsure
    }
}

/// One classified argument: `_parse_optional`'s option tuples.
#[derive(Clone)]
struct OptTuple {
    action: Option<usize>,
    option_string: String,
    /// None, Some("") ("-xVALUE"), or Some("=")
    sep: Option<String>,
    explicit: Option<String>,
}

fn arg_error(action: &PAct, msg: &str) -> Fail {
    if action.name.is_empty() {
        Fail::Arg(msg.to_string())
    } else {
        Fail::Arg(format!("argument {}: {msg}", action.name))
    }
}

/// `repr(s)` for the texts reproduced (see tmi_repo::layer_ignore::py_repr).
fn repr(s: &str) -> Result<String, Unsure> {
    tmi_repo::layer_ignore::py_repr(s).map_err(|_| Unsure)
}

/// `int(s)` succeeds (Some(true)), raises ValueError (Some(false)), or is
/// not decided here (non-ASCII text: other digits, other spaces).
fn int_ok(s: &str) -> Option<bool> {
    if !s.is_ascii() {
        return None;
    }
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    let ok = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__");
    Some(ok)
}

struct Parser {
    p: &'static PParser,
}

impl Parser {
    fn action_for(&self, opt: &str) -> Option<usize> {
        self.p.actions.iter().position(|a| a.opts.contains(&opt))
    }

    fn all_option_strings(&self) -> impl Iterator<Item = (usize, &'static str)> + '_ {
        self.p.actions.iter().enumerate().flat_map(|(i, a)| a.opts.iter().map(move |o| (i, *o)))
    }

    /// `_parse_optional(arg)`: None for a positional.
    fn parse_optional(&self, arg: &str) -> Result<Option<Vec<OptTuple>>, Unsure> {
        if arg.is_empty() || !arg.starts_with('-') {
            return Ok(None);
        }
        if let Some(i) = self.action_for(arg) {
            return Ok(Some(vec![OptTuple { action: Some(i), option_string: arg.into(), sep: None, explicit: None }]));
        }
        if arg.chars().count() == 1 {
            return Ok(None);
        }
        if let Some((opt, explicit)) = arg.split_once('=') {
            if let Some(i) = self.action_for(opt) {
                return Ok(Some(vec![OptTuple {
                    action: Some(i),
                    option_string: opt.into(),
                    sep: Some("=".into()),
                    explicit: Some(explicit.into()),
                }]));
            }
        }
        let tuples = self.option_tuples(arg);
        if !tuples.is_empty() {
            return Ok(Some(tuples));
        }
        // _negative_number_matcher: r'^-\d+$|^-\d*\.\d+$' (\d: any decimal digit)
        let body = &arg[1..];
        if !body.is_ascii() && body.chars().any(|c| c.is_numeric()) {
            return Err(Unsure);
        }
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let negative = digits(body)
            || body.split_once('.').is_some_and(|(a, b)| (a.is_empty() || digits(a)) && digits(b) && !b.contains('.'));
        let has_negative_like = self.all_option_strings().any(|(_, o)| {
            let b = &o[1..];
            digits(b) || b.split_once('.').is_some_and(|(x, y)| (x.is_empty() || digits(x)) && digits(y))
        });
        if negative && !has_negative_like {
            return Ok(None);
        }
        if arg.contains(' ') {
            return Ok(None);
        }
        Ok(Some(vec![OptTuple { action: None, option_string: arg.into(), sep: None, explicit: None }]))
    }

    /// `_get_option_tuples(option_string)` (allow_abbrev on).
    fn option_tuples(&self, arg: &str) -> Vec<OptTuple> {
        let mut out = Vec::new();
        let second_dash = arg[1..].starts_with('-');
        let (prefix, sep, explicit) = match arg.split_once('=') {
            Some((p, e)) => (p, Some("=".to_string()), Some(e.to_string())),
            None => (arg, None, None),
        };
        if second_dash {
            for (i, o) in self.all_option_strings() {
                if o.starts_with(prefix) {
                    out.push(OptTuple { action: Some(i), option_string: o.into(), sep: sep.clone(), explicit: explicit.clone() });
                }
            }
        } else {
            let split = arg.char_indices().nth(2).map_or(arg.len(), |(i, _)| i);
            let (short, short_explicit) = (&arg[..split], &arg[split..]);
            for (i, o) in self.all_option_strings() {
                if o == short {
                    out.push(OptTuple {
                        action: Some(i),
                        option_string: o.into(),
                        sep: Some(String::new()),
                        explicit: Some(short_explicit.into()),
                    });
                } else if o.starts_with(prefix) {
                    out.push(OptTuple { action: Some(i), option_string: o.into(), sep: sep.clone(), explicit: explicit.clone() });
                }
            }
        }
        out
    }

    /// `_get_nargs_pattern(action)` matched at the start of `pattern`
    /// (`_match_argument`): the count, or Err(message).
    fn match_argument(&self, a: &PAct, pattern: &[u8]) -> Result<usize, Fail> {
        let count_a = pattern.iter().take_while(|&&c| c == b'A').count();
        let n = match a.nargs {
            "0" => Some(0),
            "1" => (count_a >= 1).then_some(1),
            "?" => Some(count_a.min(1)),
            "*" => Some(count_a),
            "+" => (count_a >= 1).then_some(count_a),
            _ => return Err(Fail::Unsure),
        };
        n.ok_or_else(|| {
            let msg = match a.nargs {
                "1" => "expected one argument",
                "+" => "expected at least one argument",
                _ => "expected at most one argument",
            };
            arg_error(a, msg)
        })
    }

    /// `_get_values(action, args)`'s checks: the conversions that fail.
    fn check_values(&self, a: &PAct, args: &[String]) -> Result<(), Fail> {
        if args.is_empty() && (a.nargs == "?" || a.nargs == "*") {
            return if a.empty_ok { Ok(()) } else { Err(Fail::Unsure) };
        }
        for v in args {
            if a.int {
                match int_ok(v) {
                    None => return Err(Fail::Unsure),
                    Some(false) => return Err(arg_error(a, &format!("invalid int value: {}", repr(v)?))),
                    Some(true) => continue, // an int is never one of string choices
                }
            }
            if let Some(choices) = a.choices {
                if !choices.contains(&v.as_str()) {
                    let msg = format!("invalid choice: {} (choose from {})", repr(v)?, choices.join(", "));
                    return Err(arg_error(a, &msg));
                }
            }
        }
        Ok(())
    }

    /// `_match_arguments_partial(actions, pattern)`.
    fn match_partial(&self, actions: &[usize], pattern: &[u8]) -> Result<Vec<usize>, Fail> {
        for i in (1..=actions.len()).rev() {
            if let Some(mut counts) = match_positionals(&actions[..i].iter().map(|&k| self.p.actions[k].nargs).collect::<Vec<_>>(), pattern)? {
                let end: usize = counts.iter().sum();
                if end < pattern.len() && pattern[end] == b'O' {
                    while counts.last() == Some(&0) {
                        counts.pop();
                    }
                }
                return Ok(counts);
            }
        }
        Ok(Vec::new())
    }

    /// `_parse_known_args(args)`: the extras, or the error.
    fn parse(&self, args: &[String]) -> Result<Vec<String>, Fail> {
        let mut pattern: Vec<u8> = Vec::new();
        let mut options: std::collections::HashMap<usize, Vec<OptTuple>> = Default::default();
        let mut after_dash = false;
        for (i, arg) in args.iter().enumerate() {
            if after_dash {
                pattern.push(b'A');
            } else if arg == "--" {
                pattern.push(b'-');
                after_dash = true;
            } else {
                match self.parse_optional(arg)? {
                    None => pattern.push(b'A'),
                    Some(t) => {
                        if t.iter().any(|t| t.action.is_some_and(|a| self.p.actions[a].help)) {
                            return Err(Fail::Unsure); // help would print and exit
                        }
                        options.insert(i, t);
                        pattern.push(b'O');
                    }
                }
            }
        }
        let mut seen: Vec<bool> = vec![false; self.p.actions.len()];
        let mut seen_non_default: Vec<bool> = vec![false; self.p.actions.len()];
        let mut extras: Vec<String> = Vec::new();
        let mut positionals: Vec<usize> =
            (0..self.p.actions.len()).filter(|&i| self.p.actions[i].opts.is_empty()).collect();

        let take = |k: usize, vals: &[String], has_option: bool, seen: &mut Vec<bool>, nd: &mut Vec<bool>| -> Result<(), Fail> {
            seen[k] = true;
            let a = &self.p.actions[k];
            self.check_values(a, vals)?;
            if has_option || !vals.is_empty() {
                nd[k] = true;
                for &c in a.conflicts {
                    if nd[c] {
                        return Err(arg_error(a, &format!("not allowed with argument {}", self.p.actions[c].name)));
                    }
                }
            }
            Ok(())
        };

        let consume_positionals = |start: usize,
                                   positionals: &mut Vec<usize>,
                                   seen: &mut Vec<bool>,
                                   nd: &mut Vec<bool>|
         -> Result<usize, Fail> {
            let counts = self.match_partial(positionals, &pattern[start..])?;
            let mut at = start;
            for (&k, &n) in positionals.iter().zip(&counts) {
                let mut vals: Vec<String> = args[at..at + n].to_vec();
                if pattern[at..at + n].contains(&b'-') {
                    if let Some(p) = vals.iter().position(|v| v == "--") {
                        vals.remove(p);
                    }
                }
                at += n;
                take(k, &vals, false, seen, nd)?;
            }
            positionals.drain(..counts.len());
            Ok(at)
        };

        let consume_optional = |start: usize, extras: &mut Vec<String>, seen: &mut Vec<bool>, nd: &mut Vec<bool>| -> Result<usize, Fail> {
            let tuples = &options[&start];
            if tuples.len() > 1 {
                let names: Vec<&str> = tuples.iter().map(|t| t.option_string.as_str()).collect();
                return Err(Fail::Arg(format!("ambiguous option: {} could match {}", args[start], names.join(", "))));
            }
            let OptTuple { action, mut option_string, mut sep, mut explicit } = tuples[0].clone();
            let mut action = action;
            let mut taken: Vec<(usize, Vec<String>)> = Vec::new();
            let stop;
            loop {
                let Some(k) = action else {
                    extras.push(args[start].clone());
                    return Ok(start + 1);
                };
                let a = &self.p.actions[k];
                if let Some(ex) = explicit.clone() {
                    let count = self.match_argument(a, b"A")?;
                    let single_dash = !option_string[1..].starts_with('-');
                    if count == 0 && single_dash && !ex.is_empty() {
                        if sep.as_deref().is_some_and(|s| !s.is_empty()) || ex.starts_with('-') {
                            return Err(arg_error(a, &format!("ignored explicit argument {}", repr(&ex)?)));
                        }
                        taken.push((k, Vec::new()));
                        let first: String = ex.chars().next().unwrap().to_string();
                        option_string = format!("-{first}");
                        if let Some(next) = self.action_for(&option_string) {
                            action = Some(next);
                            let rest = &ex[first.len()..];
                            if rest.is_empty() {
                                sep = None;
                                explicit = None;
                            } else if let Some(r) = rest.strip_prefix('=') {
                                sep = Some("=".into());
                                explicit = Some(r.to_string());
                            } else {
                                sep = Some(String::new());
                                explicit = Some(rest.to_string());
                            }
                        } else {
                            extras.push(format!("-{ex}"));
                            stop = start + 1;
                            break;
                        }
                    } else if count == 1 {
                        stop = start + 1;
                        taken.push((k, vec![ex]));
                        break;
                    } else {
                        return Err(arg_error(a, &format!("ignored explicit argument {}", repr(&ex)?)));
                    }
                } else {
                    let from = start + 1;
                    let count = self.match_argument(a, &pattern[from..])?;
                    stop = from + count;
                    taken.push((k, args[from..stop].to_vec()));
                    break;
                }
            }
            for (k, vals) in taken {
                take(k, &vals, true, seen, nd)?;
            }
            Ok(stop)
        };

        let max_option = options.keys().copied().max();
        let mut start = 0usize;
        while max_option.is_some_and(|m| start <= m) {
            let max = max_option.unwrap();
            let mut next = start;
            while next <= max && !options.contains_key(&next) {
                next += 1;
            }
            if start != next {
                let end = consume_positionals(start, &mut positionals, &mut seen, &mut seen_non_default)?;
                if end > start {
                    start = end;
                    continue;
                }
                start = end;
            }
            if !options.contains_key(&start) {
                extras.extend(args[start..next].iter().cloned());
                start = next;
            }
            start = consume_optional(start, &mut extras, &mut seen, &mut seen_non_default)?;
        }
        let stop = consume_positionals(start, &mut positionals, &mut seen, &mut seen_non_default)?;
        extras.extend(args[stop..].iter().cloned());
        let missing: Vec<&str> = self
            .p
            .actions
            .iter()
            .enumerate()
            .filter(|(i, a)| !seen[*i] && a.required)
            .map(|(_, a)| a.name)
            .collect();
        if !missing.is_empty() {
            return Err(Fail::Arg(format!("the following arguments are required: {}", missing.join(", "))));
        }
        Ok(extras)
    }
}

/// The regular match of positionals' nargs patterns at the start of
/// `pattern` ('(-*A-*)', '(-*A?-*)', '(-*[A-]*)', '(-*A[A-]*)' joined):
/// each group's length, or None. Greedy with backtracking, as `re`.
fn match_positionals(nargs: &[&str], pattern: &[u8]) -> Result<Option<Vec<usize>>, Fail> {
    fn go(nargs: &[&str], pattern: &[u8], at: usize, out: &mut Vec<usize>) -> bool {
        let Some((first, rest)) = nargs.split_first() else { return true };
        // every way this group can match from `at`, longest first (greedy)
        let mut ends: Vec<usize> = Vec::new();
        let dashes = |mut i: usize| {
            while i < pattern.len() && pattern[i] == b'-' {
                i += 1;
            }
            i
        };
        let lead = dashes(at);
        match *first {
            "1" | "?" => {
                // -*A-* / -*A?-*: leading dashes (greedy, may give back)
                for d in (at..=lead).rev() {
                    let has_a = d < pattern.len() && pattern[d] == b'A';
                    if has_a {
                        let after = dashes(d + 1);
                        for e in (d + 1..=after).rev() {
                            ends.push(e);
                        }
                    }
                    if *first == "?" {
                        // A? matched empty: then -* continues from d
                        let after = dashes(d);
                        for e in (d..=after).rev() {
                            ends.push(e);
                        }
                    }
                }
            }
            "*" | "+" => {
                for d in (at..=lead).rev() {
                    let body_start = if *first == "+" {
                        if d < pattern.len() && pattern[d] == b'A' {
                            d + 1
                        } else {
                            continue;
                        }
                    } else {
                        d
                    };
                    let mut e = body_start;
                    while e < pattern.len() && (pattern[e] == b'A' || pattern[e] == b'-') {
                        e += 1;
                    }
                    for x in (body_start..=e).rev() {
                        ends.push(x);
                    }
                }
            }
            _ => return false,
        }
        let mut tried = std::collections::HashSet::new();
        for e in ends {
            if !tried.insert(e) {
                continue;
            }
            out.push(e - at);
            if go(rest, pattern, e, out) {
                return true;
            }
            out.pop();
        }
        false
    }
    if nargs.iter().any(|n| !matches!(*n, "1" | "?" | "*" | "+")) {
        return Err(Fail::Unsure);
    }
    let mut out = Vec::new();
    Ok(go(nargs, pattern, 0, &mut out).then_some(out))
}

/// What argparse prints for the whole command line `argv` (without the
/// program name), when it refuses it and that is decided here.
pub fn for_argv(argv: &[String]) -> Option<String> {
    let Some(first) = argv.first() else {
        return Some(format!("{TOP_USAGE}tmiki: error: the following arguments are required: command\n"));
    };
    if first.starts_with('-') {
        return None; // top-level options: --version, -h, or argparse's own reading
    }
    if !TOP_CHOICES.contains(&first.as_str()) {
        let shown = repr(first).ok()?;
        return Some(format!(
            "{TOP_USAGE}tmiki: error: argument command: invalid choice: {shown} (choose from {})\n",
            TOP_CHOICES.join(", ")
        ));
    }
    let (path, rest): (Vec<&str>, &[String]) = match NESTED.iter().find(|(name, _)| name == first) {
        Some((_, subs)) => match argv.get(1) {
            Some(sub) if subs.contains(&sub.as_str()) => (vec![first.as_str(), sub.as_str()], &argv[2..]),
            _ => return None, // the parser with subparsers itself
        },
        None => (vec![first.as_str()], &argv[1..]),
    };
    match check(&path, rest) {
        Verdict::Error(text) => Some(text),
        _ => None,
    }
}

/// What argparse does with `tmiki <path...> <args...>`: `path` names a
/// parser without subparsers (`["mv"]`, `["remote", "add"]`).
pub fn check(path: &[&str], args: &[String]) -> Verdict {
    let Some(p) = PARSERS.iter().find(|p| p.path == path) else { return Verdict::Unsure };
    match (Parser { p }).parse(args) {
        Ok(extras) if extras.is_empty() => Verdict::Ok,
        Ok(extras) => Verdict::Error(format!("{TOP_USAGE}tmiki: error: unrecognized arguments: {}\n", extras.join(" "))),
        Err(Fail::Arg(msg)) => Verdict::Error(format!("{}{}: error: {msg}\n", p.usage, p.prog)),
        Err(Fail::Unsure) => Verdict::Unsure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &[&str], args: &[&str]) -> Result<Vec<String>, String> {
        let p = PARSERS.iter().find(|p| p.path == path).unwrap();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match (Parser { p }).parse(&args) {
            Ok(e) => Ok(e),
            Err(Fail::Arg(m)) => Err(m),
            Err(Fail::Unsure) => Err("UNSURE".into()),
        }
    }

    #[test]
    fn errors_as_argparse() {
        assert_eq!(run(&["mv"], &["a"]), Err("the following arguments are required: dest".into()));
        assert_eq!(run(&["status"], &["--bogus", "x"]), Ok(vec!["--bogus".into(), "x".into()]));
        assert_eq!(run(&["commit"], &["-m"]), Err("argument -m/--message: expected one argument".into()));
        assert_eq!(run(&["commit"], &["-m", "--amend"]), Err("argument -m/--message: expected one argument".into()));
        assert_eq!(
            run(&["log"], &["--date", "foo"]),
            Err("argument --date: invalid choice: 'foo' (choose from iso, local, short)".into())
        );
        assert_eq!(run(&["log"], &["-n", "x"]), Err("argument -n/--max-count: invalid int value: 'x'".into()));
        assert_eq!(
            run(&["push"], &["-f", "--force-with-lease"]),
            Err("argument --force-with-lease: not allowed with argument -f/--force".into())
        );
        assert_eq!(run(&["switch"], &["a", "b", "c"]), Ok(vec!["c".into()]));
        assert_eq!(run(&["mv"], &["a", "b", "c", "d"]), Ok(vec!["c".into(), "d".into()]));
        assert_eq!(
            run(&["log"], &["--a"]),
            Err("ambiguous option: --a could match --all, --author, --after, --abbrev, --abbrev-commit".into())
        );
        assert_eq!(run(&["checkout"], &["HEAD~1", "--", "a.png"]), Ok(vec![]));
        assert_eq!(run(&["log"], &["-n2", "--oneline"]), Ok(vec![]));
        assert_eq!(run(&["commit"], &["-am", "x"]), Ok(vec![]));
        assert_eq!(run(&["status"], &["--porcelain=v2"]), Err("argument --porcelain: invalid choice: 'v2' (choose from v1)".into()));
        assert_eq!(run(&["status"], &["-h"]), Err("UNSURE".into()));
    }
}
