//! `tmiki --version` (cli.py `_version_text`) and argparse's `-h` (the texts
//! tools/gen_cli_help.py recorded in help_text.rs).

use crate::cmd_log::fwd;
use crate::help_text::{HELP, PYTHON_VERSION};
use crate::out::{self, Out, Stream};
use crate::CmdResult;

/// `tmiki -h`, `tmiki --help`, `tmiki --version` (alone).
pub fn top(argv: &[String], out: &mut Out) -> CmdResult {
    if argv.len() != 1 {
        return Err(fwd("top-level option with more arguments"));
    }
    if argv[0] == "--version" {
        let text = crate::pyenv::version_text().ok_or_else(|| fwd("--version: installed versions not certain"))?;
        out.println(text);
        return Ok(0);
    }
    help(argv, out)
}

/// argparse's help for `tmi [<command> [<subcommand>]] -h`, when argv is
/// exactly that and Python would lay it out for 80 columns.
pub fn help(argv: &[String], out: &mut Out) -> CmdResult {
    let Some((last, path)) = argv.split_last() else { return Err(fwd("help")) };
    if last != "-h" && last != "--help" {
        return Err(fwd("-h not at the end"));
    }
    let width_80 = match std::env::var("COLUMNS").ok().as_deref() {
        None | Some("") => !out::is_console(Stream::Stdout),
        Some("80") => true,
        Some(_) => false,
    };
    if !width_80 {
        return Err(fwd("help for a width other than 80 columns"));
    }
    let version = crate::pyenv::python_version().ok_or_else(|| fwd("help: Python version unknown"))?;
    if version.split('.').take(2).collect::<Vec<_>>().join(".") != PYTHON_VERSION {
        return Err(fwd("help: argparse of another Python version"));
    }
    let Some((_, text)) = HELP.iter().find(|(p, _)| p.len() == path.len() && p.iter().zip(path).all(|(a, b)| a == b)) else {
        return Err(fwd("help for an unknown parser path"));
    };
    // print_help writes the text as is (it ends with "\n").
    out.println(text.strip_suffix('\n').unwrap_or(text));
    Ok(0)
}
