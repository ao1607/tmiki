//! CPython behaviours the read side reproduces: `str.splitlines`,
//! `fnmatch` (with Windows' `ntpath.normcase`), `PurePosixPath.match`,
//! `str.lower` ordering of Windows paths, `os.path.realpath` and
//! `os.stat` times.

use std::path::{Component, Path, PathBuf};

/// `str.splitlines()`.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let is_break = matches!(
            c,
            '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        );
        if !is_break {
            continue;
        }
        out.push(&s[start..i]);
        let mut next = i + c.len_utf8();
        if c == '\r' {
            if let Some(&(j, '\n')) = chars.peek() {
                chars.next();
                next = j + 1;
            }
        }
        start = next;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// cli.py `first_line`.
pub fn first_line(s: &str) -> &str {
    splitlines(s).first().copied().unwrap_or("")
}

/// `str.isdigit()` limited to what the callers accept natively: ASCII
/// digits. None when the text has other characters `isdigit` might accept.
pub fn ascii_isdigit(s: &str) -> Option<bool> {
    if !s.is_ascii() {
        return None;
    }
    Some(!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

/// `os.path.normcase` on Windows: `LCMapStringEx(LOCALE_NAME_INVARIANT,
/// LCMAP_LOWERCASE, s.replace("/", "\\"))`; the identity elsewhere.
pub fn normcase(s: &str) -> String {
    #[cfg(windows)]
    {
        win_lowercase(&s.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        s.to_string()
    }
}

#[cfg(windows)]
fn win_lowercase(s: &str) -> String {
    use windows_sys::Win32::Globalization::{LCMapStringEx, LCMAP_LOWERCASE};
    if s.is_empty() {
        return String::new();
    }
    let src: Vec<u16> = s.encode_utf16().collect();
    let invariant: [u16; 1] = [0];
    // SAFETY: plain buffers; the call is made twice, sizing then mapping.
    unsafe {
        let n = LCMapStringEx(
            invariant.as_ptr(),
            LCMAP_LOWERCASE,
            src.as_ptr(),
            src.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        if n <= 0 {
            return s.to_lowercase();
        }
        let mut dst = vec![0u16; n as usize];
        let m = LCMapStringEx(
            invariant.as_ptr(),
            LCMAP_LOWERCASE,
            src.as_ptr(),
            src.len() as i32,
            dst.as_mut_ptr(),
            n,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        dst.truncate(m.max(0) as usize);
        String::from_utf16(&dst).unwrap_or_else(|_| s.to_lowercase())
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Lit(char),
    Any,
    Star,
    Never,
    Class { negate: bool, items: Vec<(char, char)> },
}

/// fnmatch.py `_translate` turned into tokens. Err where Python's `re`
/// would reject the class it builds (a bad range left after the
/// empty-range removal): the caller falls back to Python.
fn translate(pat: &str) -> Result<Vec<Tok>, ()> {
    let p: Vec<char> = pat.chars().collect();
    let n = p.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let c = p[i];
        i += 1;
        match c {
            '*' => {
                if out.last() != Some(&Tok::Star) {
                    out.push(Tok::Star);
                }
            }
            '?' => out.push(Tok::Any),
            '[' => {
                let mut j = i;
                if j < n && p[j] == '!' {
                    j += 1;
                }
                if j < n && p[j] == ']' {
                    j += 1;
                }
                while j < n && p[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    out.push(Tok::Lit('['));
                    continue;
                }
                // The regex class body Python builds, as (char, escaped) units.
                let body: Vec<(char, bool)>;
                let stuff: Vec<char> = p[i..j].to_vec();
                if !stuff.contains(&'-') {
                    body = stuff.iter().map(|&c| (c, c == '\\')).collect();
                } else {
                    let mut chunks: Vec<Vec<char>> = Vec::new();
                    let mut ii = i;
                    let mut k = if p[ii] == '!' { ii + 2 } else { ii + 1 };
                    loop {
                        let found = (k..j).find(|&x| p[x] == '-');
                        let Some(kk) = found else { break };
                        chunks.push(p[ii..kk].to_vec());
                        ii = kk + 1;
                        k = kk + 3;
                    }
                    let chunk = p[ii..j].to_vec();
                    if !chunk.is_empty() {
                        chunks.push(chunk);
                    } else {
                        chunks.last_mut().unwrap().push('-');
                    }
                    let mut k = chunks.len().saturating_sub(1);
                    while k > 0 {
                        let prev_last = *chunks[k - 1].last().ok_or(())?;
                        let cur_first = *chunks[k].first().ok_or(())?;
                        if prev_last > cur_first {
                            let mut merged = chunks[k - 1][..chunks[k - 1].len() - 1].to_vec();
                            merged.extend_from_slice(&chunks[k][1..]);
                            chunks[k - 1] = merged;
                            chunks.remove(k);
                        }
                        k -= 1;
                    }
                    let mut b = Vec::new();
                    for (idx, chunk) in chunks.iter().enumerate() {
                        if idx > 0 {
                            b.push(('-', false)); // a range operator
                        }
                        for &c in chunk {
                            b.push((c, c == '\\' || c == '-'));
                        }
                    }
                    body = b;
                }
                i = j + 1;
                if body.is_empty() {
                    out.push(Tok::Never);
                    continue;
                }
                if body.len() == 1 && body[0] == ('!', false) {
                    out.push(Tok::Any);
                    continue;
                }
                let (negate, rest) = if body[0] == ('!', false) { (true, &body[1..]) } else { (false, &body[..]) };
                // Parse the class: units, "a-b" ranges where '-' is an
                // unescaped operator between two units. A '-' operator
                // never ends the body (chunks are joined between items).
                let mut items = Vec::new();
                let mut u = 0;
                while u < rest.len() {
                    let (lo, _) = rest[u];
                    if u + 2 < rest.len() && rest[u + 1] == ('-', false) {
                        let (hi, _) = rest[u + 2];
                        if hi < lo {
                            return Err(()); // re.error: bad character range
                        }
                        items.push((lo, hi));
                        u += 3;
                    } else if rest[u] == ('-', false) {
                        items.push(('-', '-'));
                        u += 1;
                    } else {
                        items.push((lo, lo));
                        u += 1;
                    }
                }
                out.push(Tok::Class { negate, items });
            }
            c => out.push(Tok::Lit(c)),
        }
    }
    Ok(out)
}

fn tok_matches(t: &Tok, c: char) -> bool {
    match t {
        Tok::Lit(l) => *l == c,
        Tok::Any => true,
        Tok::Never => false,
        Tok::Class { negate, items } => items.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *negate,
        Tok::Star => unreachable!(),
    }
}

fn match_tokens(toks: &[Tok], s: &[char]) -> bool {
    // Classic wildcard matching with single-star backtracking: every
    // non-star token takes exactly one character.
    let (mut ti, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if ti < toks.len() && toks[ti] == Tok::Star {
            star = Some((ti, si));
            ti += 1;
        } else if ti < toks.len() && tok_matches(&toks[ti], s[si]) {
            ti += 1;
            si += 1;
        } else if let Some((st, ss)) = star {
            ti = st + 1;
            si = ss + 1;
            star = Some((st, ss + 1));
        } else {
            return false;
        }
    }
    while ti < toks.len() && toks[ti] == Tok::Star {
        ti += 1;
    }
    ti == toks.len()
}

/// `fnmatch.fnmatchcase(name, pat)`; Err when Python's re would raise.
pub fn fnmatchcase(name: &str, pat: &str) -> Result<bool, ()> {
    let toks = translate(pat)?;
    let chars: Vec<char> = name.chars().collect();
    Ok(match_tokens(&toks, &chars))
}

/// `fnmatch.fnmatch(name, pat)`: both through `os.path.normcase`.
pub fn fnmatch(name: &str, pat: &str) -> Result<bool, ()> {
    fnmatchcase(&normcase(name), &normcase(pat))
}

/// `PurePosixPath(path).match(pattern)` (CPython 3.13): match from the
/// right, one segment per pattern part, case-sensitive. Err for the
/// "empty pattern" ValueError.
pub fn pure_posix_match(path: &str, pattern: &str) -> Result<bool, ()> {
    let path_parts = posix_parts(path);
    let pattern_parts = posix_parts(pattern);
    if pattern_parts.is_empty() {
        return Err(());
    }
    let anchored = pattern.starts_with('/');
    if path_parts.len() < pattern_parts.len() {
        return Ok(false);
    }
    if path_parts.len() > pattern_parts.len() && anchored {
        return Ok(false);
    }
    for (pp, pat) in path_parts.iter().rev().zip(pattern_parts.iter().rev()) {
        // glob.translate of one segment: "*" alone needs one character
        // (path parts are never empty), the rest is fnmatch's translation.
        if !fnmatchcase(pp, pat)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `PurePosixPath(s).parts`: the anchor ("/" or "//") then the names,
/// without empty and "." segments.
fn posix_parts(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    if s.starts_with('/') {
        let n = s.chars().take_while(|&c| c == '/').count();
        parts.push(if n == 2 { "//".to_string() } else { "/".to_string() });
    }
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        parts.push(seg.to_string());
    }
    parts
}

/// `os.stat(path).st_mtime_ns`.
pub fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        (meta.last_write_time() as i128 - 116_444_736_000_000_000) * 100
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
    }
}

/// Strip Windows' verbatim prefix as `ntpath.realpath` does.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p
}

/// `os.path.abspath`: joined to the cwd, "." and ".." folded.
pub fn abspath(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `Path(p).resolve()` (strict=False): the existing prefix canonicalized,
/// the rest appended.
pub fn realpath(p: &Path) -> PathBuf {
    let abs = abspath(p);
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = abs.clone();
    loop {
        if let Ok(c) = std::fs::canonicalize(&cur) {
            let mut out = strip_verbatim(c);
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (cur.file_name().map(|f| f.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent.to_path_buf();
            }
            _ => return abs,
        }
    }
}

/// `PurePath.relative_to(root)` as a posix string, comparing case
/// insensitively on Windows; None when `full` is not under `root`.
pub fn relative_posix(full: &Path, root: &Path) -> Option<String> {
    let f: Vec<String> = full.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let r: Vec<String> = root.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if f.len() < r.len() {
        return None;
    }
    for (a, b) in f.iter().zip(&r) {
        let same = if cfg!(windows) { a.to_lowercase() == b.to_lowercase() } else { a == b };
        if !same {
            return None;
        }
    }
    Some(f[r.len()..].join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_like_python() {
        assert_eq!(splitlines("a\nb\r\nc\rd\u{2028}e\n"), vec!["a", "b", "c", "d", "e"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("\n"), vec![""]);
        assert_eq!(splitlines("x\r\r\ny"), vec!["x", "", "y"]);
        assert_eq!(first_line("\nb"), "");
    }

    #[test]
    fn fnmatchcase_like_python() {
        let t = |n: &str, p: &str| fnmatchcase(n, p).unwrap();
        assert!(t("a/b.png", "*.png"));
        assert!(t("abc", "a?c"));
        assert!(!t("ab", "a?c"));
        assert!(t("b", "[abc]"));
        assert!(!t("d", "[abc]"));
        assert!(t("d", "[!abc]"));
        assert!(t("m", "[a-z]"));
        assert!(t("-", "[a-]"));
        assert!(t("]", "[]]"));
        assert!(t("[", "["));
        assert!(t("x[", "x["));
        assert!(!t("a", "[z-a]"));
        assert!(t("c", "[a-b-c]"));
        assert!(t("-", "[a-b-c]"));
        assert!(t("aXbYc", "a*b*c"));
        assert!(!t("aXbYd", "a*b*c"));
        assert!(t("", "*"));
    }

    #[test]
    fn pure_posix_match_like_python() {
        let t = |p: &str, pat: &str| pure_posix_match(p, pat).unwrap();
        assert!(t("a/b/c.png", "*.png"));
        assert!(t("a/b/c.png", "b/*.png"));
        assert!(!t("a/b/c.png", "a/*.png"));
        assert!(!t("c.png", "b/*.png"));
        assert!(!t("a/c.png", "/c.png"));
        assert!(t(".tmi/x", ".tmi/*"));
        assert!(pure_posix_match("a", ".").is_err());
    }
}
