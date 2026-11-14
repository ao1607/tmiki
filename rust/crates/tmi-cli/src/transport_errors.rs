//! An HTTPS remote's failures in tmi's words (issue #29), as
//! tmi/transport/https.py words them: a reply that is not the JSON tmihub
//! sends, a download that is not a readable zip archive, no answer at all,
//! a reply that stopped coming (TMIHUB_TIMEOUT) or broke off.
//!
//! tmi-transport reports these with the texts of the Python libraries it
//! mirrors (json's "Expecting value", zipfile's "File is not a zip file",
//! an AttributeError's "'list' object has no attribute 'get'", urllib's
//! "<urlopen error ...>"); the front door rewords them here for an http(s)
//! remote. A failure before anything is written still goes to Python,
//! which words it the same way.

use tmi_repo::RepoError;

/// https.py `UNEXPECTED_REPLY`.
pub const UNEXPECTED_REPLY: &str = "tmihub sent a reply tmi cannot read (not the JSON it expects); the server may have a problem, \
     or the remote URL may not point at a tmihub repository";

/// https.py `NO_ANSWER`.
pub const NO_ANSWER: &str = "tmihub stopped answering (TMIHUB_TIMEOUT sets how many seconds tmi waits, 300 by default); \
     check your network connection and run the command again";

/// https.py `BROKEN_DOWNLOAD`.
pub const BROKEN_DOWNLOAD: &str = "tmihub sent a broken download (not a readable zip archive); run the command again, \
     and if it keeps failing the server may have a problem";

/// The scheme and the rest of a URL (`urlsplit`).
fn split_scheme(url: &str) -> Option<(String, &str)> {
    let (scheme, rest) = url.split_once(':')?;
    let ok = scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    ok.then(|| (scheme.to_ascii_lowercase(), rest))
}

/// Whether `open_transport(url)` gives an `HttpsTransport`.
pub fn is_http(url: &str) -> bool {
    matches!(split_scheme(url), Some((s, _)) if s == "http" || s == "https")
}

/// `urlsplit(url).netloc or url` (of `url.rstrip('/')`).
fn host(url: &str) -> String {
    let url = url.trim_end_matches('/');
    let netloc = split_scheme(url)
        .and_then(|(_, rest)| rest.strip_prefix("//"))
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    if netloc.is_empty() { url.to_string() } else { netloc.to_string() }
}

/// json's decode errors (tmi_store::json): "<what> at <offset>".
fn is_json_error(msg: &str) -> bool {
    const WHAT: [&str; 10] = [
        "Expecting value",
        "Expecting ',' delimiter",
        "Expecting ':' delimiter",
        "Expecting property name enclosed in double quotes",
        "Extra data",
        "Unterminated string",
        "Invalid \\escape",
        "Invalid control character",
        "Invalid \\uXXXX escape",
        "invalid utf-8",
    ];
    if msg.starts_with("invalid utf-8: ") || msg.starts_with("'utf-8' codec can't decode") || msg == "Invalid \\uXXXX escape" {
        return true;
    }
    WHAT.iter().any(|w| {
        msg.strip_prefix(w)
            .and_then(|rest| rest.strip_prefix(" at "))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// The Python errors tmi-transport names for a reply of the wrong shape.
fn is_shape_error(msg: &str) -> bool {
    let typed = |suffix: &str| {
        msg.strip_prefix('\'')
            .and_then(|m| m.split_once("' object "))
            .is_some_and(|(ty, rest)| !ty.is_empty() && ty.bytes().all(|b| b.is_ascii_alphabetic()) && rest == suffix)
    };
    typed("has no attribute 'get'")
        || typed("has no attribute 'items'")
        || typed("is not subscriptable")
        || typed("is not iterable")
        || (msg.starts_with("can only concatenate str (not \"") && msg.ends_with("\") to str"))
}

/// zipfile's errors as tmi-transport's zip reader gives them.
fn is_zip_error(msg: &str) -> bool {
    matches!(
        msg,
        "File is not a zip file"
            | "Corrupt zip64 end of central directory locator"
            | "Truncated central directory"
            | "Bad magic number for central directory"
            | "Truncated file header"
            | "Bad magic number for file header"
    ) || msg.starts_with("Error -3 while decompressing data")
        || msg.starts_with("compression type ")
        || msg.starts_with("Bad CRC-32 for file ")
        || msg.starts_with("Bad size for file ")
        || msg.starts_with("Bad size or offset for file ")
        || (msg.starts_with("File '") && msg.ends_with(" is encrypted, password required for extraction"))
}

/// `err` in tmi's words when the remote at `url` is a tmihub one.
pub fn reword(err: RepoError, url: Option<&str>) -> RepoError {
    let Some(url) = url.filter(|u| is_http(u)) else { return err };
    let RepoError::Runtime(msg) = &err else { return err };
    if is_json_error(msg) || is_shape_error(msg) {
        return RepoError::runtime(UNEXPECTED_REPLY);
    }
    if is_zip_error(msg) {
        return RepoError::runtime(BROKEN_DOWNLOAD);
    }
    // a reply that stopped coming (tmi-transport `receive_text`, `read_all`)
    if msg == "timed out" {
        return RepoError::runtime(NO_ANSWER);
    }
    if msg == "Remote end closed connection without response" || (msg.starts_with("IncompleteRead(") && msg.ends_with(')')) {
        return RepoError::runtime(format!("the connection to tmihub broke off ({msg}); run the command again"));
    }
    if let Some(reason) = msg.strip_prefix("<urlopen error ").and_then(|m| m.strip_suffix('>')) {
        return RepoError::runtime(format!(
            "cannot reach tmihub at {}: {reason}; check the remote URL and your network connection",
            host(url)
        ));
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(e: RepoError) -> String {
        e.message().unwrap_or_default().to_string()
    }

    #[test]
    fn rewords_only_tmihub_failures() {
        let url = Some("http://127.0.0.1:18999/api/u/r/");
        assert_eq!(text(reword(RepoError::runtime("Expecting value at 0"), url)), UNEXPECTED_REPLY);
        assert_eq!(text(reword(RepoError::runtime("'list' object has no attribute 'get'"), url)), UNEXPECTED_REPLY);
        assert_eq!(text(reword(RepoError::runtime("'NoneType' object is not iterable"), url)), UNEXPECTED_REPLY);
        assert_eq!(text(reword(RepoError::runtime("can only concatenate str (not \"int\") to str"), url)), UNEXPECTED_REPLY);
        assert_eq!(text(reword(RepoError::runtime("File is not a zip file"), url)), BROKEN_DOWNLOAD);
        assert_eq!(text(reword(RepoError::runtime("Bad CRC-32 for file 'ab/cd'"), url)), BROKEN_DOWNLOAD);
        assert_eq!(
            text(reword(RepoError::runtime("<urlopen error [Errno 111] Connection refused>"), url)),
            "cannot reach tmihub at 127.0.0.1:18999: [Errno 111] Connection refused; check the remote URL and your network connection"
        );
        // other failures, and local remotes, keep their words
        assert_eq!(text(reword(RepoError::runtime("timed out"), url)), NO_ANSWER);
        assert_eq!(
            text(reword(RepoError::runtime("IncompleteRead(3 bytes read, 7 more expected)"), url)),
            "the connection to tmihub broke off (IncompleteRead(3 bytes read, 7 more expected)); run the command again"
        );
        assert_eq!(text(reword(RepoError::runtime("tmihub request failed (500)"), url)), "tmihub request failed (500)");
        assert_eq!(text(reword(RepoError::runtime("Expecting value at 0"), Some("../bare.tmi"))), "Expecting value at 0");
        assert_eq!(text(reword(RepoError::runtime("Expecting value at 0"), Some("C:\\x\\r"))), "Expecting value at 0");
        assert!(reword(RepoError::forward("x"), url).is_forward());
    }
}
