//! The templates' small language: `{{name}}` is replaced by a value; a
//! line that is just `{{#name}}` or `{{/name}}` opens or closes a block kept
//! only when `name` is on (`{{^name}}` ... `{{/name}}`: kept when off); the
//! same pair within one line is an inline block.

use std::collections::BTreeMap;

pub struct Vars {
    pub values: BTreeMap<&'static str, String>,
    pub flags: BTreeMap<&'static str, bool>,
}

/// `text` with its blocks decided and its names replaced. Panics on an
/// unknown name (the templates are tmi's own, checked by the tests).
pub fn render(text: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(text.len());
    let mut keep: Vec<bool> = Vec::new();
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if let Some(name) = t.strip_prefix("{{#").and_then(|s| s.strip_suffix("}}")) {
            keep.push(*vars.flags.get(name).unwrap_or_else(|| panic!("template flag {name}")));
            continue;
        }
        if let Some(name) = t.strip_prefix("{{^").and_then(|s| s.strip_suffix("}}")) {
            keep.push(!*vars.flags.get(name).unwrap_or_else(|| panic!("template flag {name}")));
            continue;
        }
        if t.starts_with("{{/") && t.ends_with("}}") {
            keep.pop().expect("a block closed that was not opened");
            continue;
        }
        if keep.iter().all(|k| *k) {
            out.push_str(&substitute(line, vars));
        }
    }
    assert!(keep.is_empty(), "a block left open");
    out
}

fn substitute(line: &str, vars: &Vars) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find("{{") {
        out.push_str(&rest[..at]);
        let end = rest[at..].find("}}").map(|e| at + e).expect("an unclosed {{");
        let name = &rest[at + 2..end];
        if let Some((on, flag)) = name.strip_prefix('#').map(|f| (true, f)).or_else(|| name.strip_prefix('^').map(|f| (false, f))) {
            // an inline block: {{#flag}}...{{/flag}} within the line
            let close = format!("{{{{/{flag}}}}}");
            let body_start = end + 2;
            let body_end = rest[body_start..].find(&close).map(|e| body_start + e).unwrap_or_else(|| panic!("an inline {{{{{name}}}}} not closed on its line"));
            let keep = *vars.flags.get(flag).unwrap_or_else(|| panic!("template flag {flag}")) == on;
            if keep {
                out.push_str(&substitute(&rest[body_start..body_end], vars));
            }
            rest = &rest[body_end + close.len()..];
            continue;
        }
        out.push_str(vars.values.get(name).unwrap_or_else(|| panic!("template value {name}")));
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_and_values() {
        let vars = Vars {
            values: [("id", "kra".to_string())].into_iter().collect(),
            flags: [("writer", false), ("python", true)].into_iter().collect(),
        };
        let t = "a {{id}}\n{{#writer}}\nw\n{{/writer}}\n{{^writer}}\nr\n{{/writer}}\n{{#python}}\np {{id}}{{id}}\n{{/python}}\n";
        assert_eq!(render(t, &vars), "a kra\nr\np krakra\n");
        assert_eq!(render("x {{#python}}py {{id}}{{/python}}{{^python}}rs{{/python}} y\n", &vars), "x py kra y\n");
    }
}
