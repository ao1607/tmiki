//! Whether `urllib.request.urlopen` would send a request through a proxy:
//! the default opener's `ProxyHandler()` with `getproxies()` and
//! `proxy_bypass()` (CPython 3.10.11 to 3.13 read them alike). The Rust
//! transport connects directly only, so a command whose requests urllib
//! would proxy goes to Python, and every other one runs here.
//!
//! - The environment (`getproxies_environment`): every variable named
//!   `<name>_proxy` in any case with a non-empty value, then the lower-case
//!   spellings again (an empty one removes its name); `REQUEST_METHOD` (a CGI
//!   script) drops the non-lower-case `HTTP_PROXY`. On Windows `os.environ`
//!   has upper-case names, so only the first pass applies there. urllib
//!   reads only the entry named after the URL's scheme (`http` / `https`)
//!   and `no` (`no_proxy`); `all_proxy` and the like are not urllib's.
//! - When the environment names no proxy at all (the dict is empty), the
//!   system settings: the Windows Internet Settings (`getproxies_registry`)
//!   and macOS's SystemConfiguration, which is not read here.
//! - `no_proxy` (`proxy_bypass_environment`) is evaluated exactly; the
//!   registry's `ProxyOverride` is not (CPython 3.10 resolves the host to
//!   match it), so a registry proxy always means Python.

use indexmap::IndexMap;
use std::ffi::OsString;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum System {
    Posix,
    MacOs,
    Windows,
}

impl System {
    pub fn current() -> System {
        if cfg!(windows) {
            System::Windows
        } else if cfg!(target_os = "macos") {
            System::MacOs
        } else {
            System::Posix
        }
    }
}

/// `getproxies_environment()`: name -> value (None: a value that is not
/// UTF-8), in Python's order.
pub fn env_proxies(vars: &[(OsString, OsString)], system: System) -> IndexMap<String, Option<String>> {
    let mut proxies = IndexMap::new();
    let mut environment: Vec<(String, Option<String>, String, bool)> = Vec::new();
    let mut cgi = false;
    for (name, value) in vars {
        let name = name.to_string_lossy();
        // os.environ upper-cases names on Windows (and `in` with them)
        let name = if system == System::Windows { name.to_uppercase() } else { name.into_owned() };
        if name == "REQUEST_METHOD" {
            cgi = true;
        }
        let chars: Vec<char> = name.chars().collect();
        let n = chars.len();
        if n > 5 && chars[n - 6] == '_' && chars[n - 5..].iter().collect::<String>().to_lowercase() == "proxy" {
            let proxy_name = chars[..n - 6].iter().collect::<String>().to_lowercase();
            let text = value.to_str().map(str::to_string);
            let truthy = !value.is_empty();
            if truthy {
                proxies.insert(proxy_name.clone(), text.clone());
            }
            environment.push((name, text, proxy_name, truthy));
        }
    }
    if cgi {
        proxies.shift_remove("http");
    }
    for (name, text, proxy_name, truthy) in environment {
        if name.ends_with("_proxy") {
            if truthy {
                proxies.insert(proxy_name, text);
            } else {
                proxies.shift_remove(&proxy_name);
            }
        }
    }
    proxies
}

/// `_splitport(host)[0]`.
fn split_port(host: &str) -> &str {
    match host.rfind(':') {
        Some(i) if host[i + 1..].bytes().all(|b| b.is_ascii_digit()) => &host[..i],
        _ => host,
    }
}

/// `proxy_bypass_environment(host, proxies)` for the `no` entry. Text
/// outside ASCII (where Python's `lower` and `strip` are not reproduced)
/// counts as not bypassed: such a command goes to Python.
fn bypass_environment(host: &str, no_proxy: Option<&Option<String>>) -> bool {
    let Some(Some(no_proxy)) = no_proxy else { return false };
    if no_proxy == "*" {
        return true;
    }
    if !host.is_ascii() || !no_proxy.is_ascii() {
        return false;
    }
    let host = host.to_ascii_lowercase();
    let hostonly = split_port(&host);
    for name in no_proxy.split(',') {
        let name = name.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b' || ('\x1c'..='\x1f').contains(&c));
        if name.is_empty() {
            continue;
        }
        let name = name.trim_start_matches('.').to_ascii_lowercase();
        if hostonly == name || host == name {
            return true;
        }
        let name = format!(".{name}");
        if hostonly.ends_with(&name) || host.ends_with(&name) {
            return true;
        }
    }
    false
}

/// `_parse_proxy(proxy)` raises ValueError ("proxy URL with no authority").
fn parse_proxy_raises(proxy: &str) -> bool {
    // _splittype: "([^/:]+):(.*)" at the start
    let rest = match proxy.find(['/', ':']) {
        Some(i) if i > 0 && proxy[i..].starts_with(':') => &proxy[i + 1..],
        _ => proxy,
    };
    rest.starts_with('/') && !rest.starts_with("//")
}

/// The protocols `getproxies_registry()` returns (lower-cased, as
/// `ProxyHandler` uses them) for a ProxyServer value read as `str(value)`;
/// `server` None: a value of another type (its `str()` is not
/// reproduced: taken as a proxy for everything).
pub fn registry_protocols(server: Option<&str>) -> Vec<String> {
    let Some(server) = server else { return vec!["http".into(), "https".into()] };
    let server = if !server.contains('=') && !server.contains(';') {
        format!("http={server};https={server};ftp={server}")
    } else {
        server.to_string()
    };
    let has_type = |address: &str| matches!(address.find(['/', ':']), Some(i) if i > 0 && address[i..].starts_with("://"));
    let mut proxies: IndexMap<String, String> = IndexMap::new();
    for p in server.split(';') {
        // `protocol, address = p.split('=', 1)`: a ValueError ends the loop
        // with what it has, and skips the SOCKS step
        let Some((protocol, address)) = p.split_once('=') else {
            return proxies.keys().map(|k| k.to_lowercase()).collect();
        };
        let mut address = address.to_string();
        if !has_type(&address) {
            if matches!(protocol, "http" | "https" | "ftp") {
                address = format!("http://{address}");
            } else if protocol == "socks" {
                address = format!("socks://{address}");
            }
        }
        proxies.insert(protocol.to_string(), address);
    }
    if proxies.get("socks").is_some_and(|a| !a.is_empty()) {
        for scheme in ["http", "https"] {
            if !proxies.get(scheme).is_some_and(|a| !a.is_empty()) {
                proxies.insert(scheme.to_string(), "socks4://".into());
            }
        }
    }
    proxies.keys().map(|k| k.to_lowercase()).collect()
}

/// The registry's Internet Settings as `getproxies_registry()` reads them.
#[cfg(windows)]
pub fn system_registry_protocols() -> Vec<String> {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, REG_DWORD, REG_EXPAND_SZ, REG_QWORD, REG_SZ, RRF_NOEXPAND, RRF_RT_ANY,
    };
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
    // (type, data) of a value, None when it cannot be read (OSError)
    let read = |name: &str| -> Option<(u32, Vec<u8>)> {
        let name = wide(name);
        let mut kind = 0u32;
        let mut size = 0u32;
        // SAFETY: a size query (null data pointer); the strings are NUL-terminated.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_ANY | RRF_NOEXPAND,
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if rc != 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize + 8];
        let mut size = data.len() as u32;
        // SAFETY: `data` holds `size` bytes.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_ANY | RRF_NOEXPAND,
                &mut kind,
                data.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if rc != 0 {
            return None;
        }
        data.truncate(size as usize);
        Some((kind, data))
    };
    // winreg's str for REG_SZ / REG_EXPAND_SZ: up to the first NUL
    let text = |data: &[u8]| -> String {
        let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        String::from_utf16_lossy(&units)
    };
    let Some((kind, data)) = read("ProxyEnable") else { return Vec::new() };
    let enabled = match kind {
        REG_DWORD | REG_QWORD => data.iter().any(|&b| b != 0),
        REG_SZ | REG_EXPAND_SZ => !text(&data).is_empty(),
        _ => !data.is_empty(),
    };
    if !enabled {
        return Vec::new();
    }
    match read("ProxyServer") {
        None => Vec::new(),
        Some((REG_SZ | REG_EXPAND_SZ, data)) => registry_protocols(Some(&text(&data))),
        Some(_) => registry_protocols(None),
    }
}

/// Why urllib would send requests for `url` through a proxy (None: it
/// connects directly). `registry` gives the Windows registry's protocols.
pub fn urllib_proxy(
    url: &str,
    vars: &[(OsString, OsString)],
    system: System,
    registry: &dyn Fn() -> Vec<String>,
) -> Option<String> {
    let env = env_proxies(vars, system);
    // Request(url): _splittype, then _splithost on the rest
    let (scheme, rest) = match url.find(['/', ':']) {
        Some(i) if i > 0 && url[i..].starts_with(':') => (url[..i].to_lowercase(), &url[i + 1..]),
        _ => return Some("a URL urllib reads differently".into()),
    };
    let host = rest.strip_prefix("//").map(|r| r.split(['/', '?', '#']).next().unwrap_or(""));
    if env.is_empty() {
        return match system {
            System::Posix => None,
            System::MacOs => Some("macOS's proxy settings are not read here".into()),
            System::Windows => {
                registry().iter().any(|p| *p == scheme).then(|| "the Windows proxy settings name a proxy".into())
            }
        };
    }
    let Some(proxy) = env.get(&scheme) else { return None };
    let what = format!("{scheme}_proxy is set");
    match proxy {
        Some(p) if !parse_proxy_raises(p) => {}
        _ => return Some(what),
    }
    match host {
        // `if req.host and proxy_bypass(req.host)`; unquote() is not reproduced
        Some(h) if !h.is_empty() && !h.contains('%') && bypass_environment(h, env.get("no")) => None,
        _ => Some(what),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))).collect()
    }

    fn decide(url: &str, pairs: &[(&str, &str)], system: System) -> bool {
        urllib_proxy(url, &vars(pairs), system, &|| Vec::new()).is_some()
    }

    const URL: &str = "http://127.0.0.1:18765/api/u/r";

    #[test]
    fn unrelated_variables_do_not_count() {
        let unrelated = [
            ("NO_PROXY", "localhost"),
            ("no_proxy", "localhost"),
            ("npm_config_https_proxy", "http://p:1"),
            ("YARN_HTTPS_PROXY", "http://p:1"),
            ("ELECTRON_GET_USE_PROXY", "1"),
            ("GLOBAL_AGENT_NO_PROXY", "x"),
            ("DOCKER_HTTPS_PROXY", "http://p:1"),
            ("ALL_PROXY", "socks5://p:1"),
            ("all_proxy", "http://p:1"),
            ("https_proxy", "http://p:1"),
        ];
        for system in [System::Posix, System::Windows] {
            assert!(!decide(URL, &unrelated, system), "{system:?}");
        }
        assert!(decide("https://h/api/u/r", &unrelated, System::Posix));
    }

    #[test]
    fn scheme_proxies_and_case() {
        for system in [System::Posix, System::Windows, System::MacOs] {
            assert!(decide(URL, &[("http_proxy", "http://p:1")], system));
            assert!(decide(URL, &[("HTTP_PROXY", "p:1")], system));
            assert!(decide(URL, &[("Http_Proxy", "p:1")], system));
            assert!(!decide(URL, &[("HTTP_PROXY", "")], system) || system == System::MacOs);
        }
        // an empty lower-case spelling removes the name (not on Windows,
        // where os.environ's names are upper case)
        let pairs = [("HTTP_PROXY", "p:1"), ("http_proxy", "")];
        assert!(!decide(URL, &pairs, System::Posix));
        assert!(decide(URL, &pairs, System::Windows));
    }

    #[test]
    fn cgi_drops_upper_case_http_proxy() {
        assert!(!decide(URL, &[("HTTP_PROXY", "p:1"), ("REQUEST_METHOD", "GET")], System::Posix));
        assert!(decide(URL, &[("http_proxy", "p:1"), ("REQUEST_METHOD", "GET")], System::Posix));
        assert!(decide(URL, &[("HTTP_PROXY", "p:1"), ("request_method", "GET")], System::Posix));
        assert!(!decide(URL, &[("HTTP_PROXY", "p:1"), ("request_method", "GET")], System::Windows));
        assert!(decide("https://h/x", &[("HTTPS_PROXY", "p:1"), ("REQUEST_METHOD", "GET")], System::Posix));
    }

    #[test]
    fn no_proxy() {
        fn p(no: &str) -> [(&str, &str); 2] {
            [("http_proxy", "http://p:1"), ("no_proxy", no)]
        }
        for (no, bypass) in [
            ("*", true),
            ("127.0.0.1", true),
            (" .0.0.1 ", true),
            ("localhost, 127.0.0.1:18765", true),
            ("127.0.0.1:1", false),
            ("localhost", false),
            ("*.0.0.1", false),
            ("27.0.0.1", false),
            ("", false),
            (",, ,", false),
            (".", false),
        ] {
            assert_eq!(!decide(URL, &p(no), System::Posix), bypass, "{no:?}");
        }
        assert!(!decide("http://Hub.Example.COM/api", &p("example.com"), System::Posix));
        assert!(decide("http://notexample.com/api", &p("example.com"), System::Posix));
        assert!(!decide("http://h.example.com./api", &p("."), System::Posix));
        // NO_PROXY alone makes the environment non-empty: the registry is not read
        assert!(urllib_proxy(URL, &vars(&[("NO_PROXY", "x")]), System::Windows, &|| vec!["http".into()]).is_none());
        assert!(urllib_proxy(URL, &vars(&[]), System::Windows, &|| vec!["http".into()]).is_some());
        assert!(urllib_proxy(URL, &vars(&[]), System::Windows, &|| vec!["ftp".into()]).is_none());
        assert!(urllib_proxy(URL, &vars(&[]), System::MacOs, &|| Vec::new()).is_some());
        assert!(urllib_proxy(URL, &vars(&[("NO_PROXY", "x")]), System::MacOs, &|| Vec::new()).is_none());
    }

    #[test]
    fn malformed_proxy_values_go_to_python() {
        let bypassed = |value: &str| [("http_proxy", value.to_string()), ("no_proxy", "*".to_string())];
        for (value, raises) in [("http:/p", true), ("/p", true), ("http://p:1", false), ("p:1", false), ("p/x:/y", false)] {
            let pairs = bypassed(value);
            let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert_eq!(decide(URL, &pairs, System::Posix), raises, "{value:?}");
        }
    }

    #[test]
    fn registry_values() {
        let has = |server: &str, scheme: &str| registry_protocols(Some(server)).iter().any(|p| p == scheme);
        assert!(has("proxy:8080", "http") && has("proxy:8080", "https"));
        assert!(has("http=p:1;https=p:2", "https"));
        assert!(!has("ftp=p:1", "http"));
        assert!(has("socks=p:1", "http") && has("socks=p:1", "https"));
        assert!(has("HTTP=p:1;ftp=x", "http"));
        // a part without '=' ends the parse there (ValueError)
        assert!(!has("ftp=p:1;bogus;http=p:2", "http"));
        assert!(!has("ftp=p:1;bogus;socks=p:2", "https"));
        assert!(has("", "http"));
        assert!(registry_protocols(None).contains(&"https".to_string()));
    }
}
