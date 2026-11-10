//! `tmiki --version` and `-h` (the help texts in help_text.rs, argparse's
//! 80-column layout as it was recorded from the Python front end).

use crate::cmd_log::fwd;
use crate::help_text::HELP;
use crate::out::Out;
use crate::CmdResult;

/// `tmiki -h`, `tmiki --help`, `tmiki --version` (alone).
pub fn top(argv: &[String], out: &mut Out) -> CmdResult {
    if argv.len() != 1 {
        return Err(fwd("top-level option with more arguments"));
    }
    if argv[0] == "--version" {
        out.println(format!("tmiki {}", env!("CARGO_PKG_VERSION")));
        return Ok(0);
    }
    help(argv, out)
}

/// The help for `tmiki [<command> [<subcommand>]] -h`.
pub fn help(argv: &[String], out: &mut Out) -> CmdResult {
    let Some((last, path)) = argv.split_last() else { return Err(fwd("help")) };
    if last != "-h" && last != "--help" {
        return Err(fwd("-h not at the end"));
    }
    let Some((_, text)) = HELP.iter().find(|(p, _)| p.len() == path.len() && p.iter().zip(path).all(|(a, b)| a == b)) else {
        return Err(fwd("help for an unknown parser path"));
    };
    // print_help writes the text as is (it ends with "\n").
    out.println(text.strip_suffix('\n').unwrap_or(text));
    Ok(0)
}
