//! How `tmiki plugin test` compares (PLUGINS.md 7.1: native vs wasm,
//! determinism, an older component, `*.expect.json`): WIT values and records
//! as `tmi_plugin_dev::Value`, the values of `tmiki plugin dev`'s RPC, written
//! the one way the RPC writes them (`tmi_plugin_host::devwire`'s
//! `*_value`), and compared in canonical CBOR (`Value::canonical`, RFC 8949
//! 4.2.1). Two values are equal iff their canonical encodings are.

use tmi_store::json::{Int, Value as Json};

pub use tmi_plugin_dev::Value as V;

/// A map with text keys.
pub fn map(entries: impl IntoIterator<Item = (&'static str, V)>) -> V {
    V::Map(entries.into_iter().map(|(k, v)| (V::text(k), v)).collect())
}

/// A record's JSON as a value (an integer past 64 bits is its decimal text).
pub fn from_json(j: &Json) -> V {
    match j {
        Json::Null => V::Null,
        Json::Bool(b) => V::Bool(*b),
        Json::Int(Int::Small(i)) => V::int(*i),
        Json::Int(Int::Big(s)) => match s.parse::<i128>() {
            Ok(i) if (0..=u64::MAX as i128).contains(&i) => V::Uint(i as u64),
            Ok(i) if (-(u64::MAX as i128) - 1..0).contains(&i) => V::Nint((-1 - i) as u64),
            _ => V::Text(s.clone()),
        },
        Json::Float(f) => V::Float(*f),
        Json::Str(s) => V::Text(s.clone()),
        Json::Array(a) => V::Array(a.iter().map(from_json).collect()),
        Json::Object(m) => V::Map(m.iter().map(|(k, v)| (V::text(k.clone()), from_json(v))).collect()),
    }
}

fn same(a: &V, b: &V) -> bool {
    a.canonical() == b.canonical()
}

/// Where `a` and `b` first differ (a path such as `layers[2].opacity` and
/// the two values, shortened), or None when they are the same value.
pub fn first_difference(a: &V, b: &V) -> Option<String> {
    if same(a, b) {
        return None;
    }
    Some(diff_at(a, b, String::new()))
}

fn short(v: &V) -> String {
    let s = match v {
        V::Null => "null".into(),
        V::Bool(b) => b.to_string(),
        V::Uint(u) => u.to_string(),
        V::Nint(n) => format!("-{}", u128::from(*n) + 1),
        V::Float(f) => format!("{f:?}"),
        V::Text(s) => format!("{s:?}"),
        V::Bytes(b) => format!("{} bytes", b.len()),
        V::Array(a) => format!("[{} items]", a.len()),
        V::Map(m) => format!("{{{}}}", m.iter().map(|(k, _)| key(k)).collect::<Vec<_>>().join(", ")),
    };
    if s.chars().count() > 80 {
        format!("{}...", s.chars().take(77).collect::<String>())
    } else {
        s
    }
}

fn key(k: &V) -> String {
    match k {
        V::Text(t) => t.clone(),
        other => short(other),
    }
}

fn diff_at(a: &V, b: &V, path: String) -> String {
    let here = if path.is_empty() { "(top)".to_string() } else { path.clone() };
    match (a, b) {
        (V::Array(x), V::Array(y)) => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                if !same(p, q) {
                    return diff_at(p, q, format!("{path}[{i}]"));
                }
            }
            format!("{here}: {} items vs {}", x.len(), y.len())
        }
        (V::Map(x), V::Map(y)) => {
            let mut keys: Vec<String> = x.iter().map(|(k, _)| key(k)).chain(y.iter().map(|(k, _)| key(k))).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let p = x.iter().find(|(kk, _)| key(kk) == k).map(|(_, v)| v);
                let q = y.iter().find(|(kk, _)| key(kk) == k).map(|(_, v)| v);
                let sub = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                match (p, q) {
                    (Some(p), Some(q)) if !same(p, q) => return diff_at(p, q, sub),
                    (Some(p), None) => return format!("{sub}: {} vs (missing)", short(p)),
                    (None, Some(q)) => return format!("{sub}: (missing) vs {}", short(q)),
                    _ => {}
                }
            }
            format!("{here}: differs")
        }
        _ => format!("{here}: {} vs {}", short(a), short(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differences_have_a_path() {
        let a = map([("layers", V::Array(vec![map([("name", V::text("x"))])]))]);
        let b = map([("layers", V::Array(vec![map([("name", V::text("y"))])]))]);
        assert_eq!(first_difference(&a, &b).unwrap(), "layers[0].name: \"x\" vs \"y\"");
        assert_eq!(first_difference(&a, &a.clone()), None);
        // the order of a map's entries and a float's width do not matter
        let c = V::Map(vec![(V::text("b"), V::Float(1.0)), (V::text("a"), V::Uint(1))]);
        let d = V::Map(vec![(V::text("a"), V::Uint(1)), (V::text("b"), V::Float(1.0))]);
        assert_eq!(first_difference(&c, &d), None);
    }

    #[test]
    fn json_integers() {
        assert_eq!(from_json(&Json::Int(Int::Small(-3))), V::Nint(2));
        assert_eq!(from_json(&Json::Int(Int::Big("18446744073709551615".into()))), V::Uint(u64::MAX));
        assert_eq!(from_json(&Json::Int(Int::Big("18446744073709551616".into()))), V::text("18446744073709551616"));
    }
}
