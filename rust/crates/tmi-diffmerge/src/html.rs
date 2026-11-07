//! tmi/diff_html.py `render_report`: the HTML report of `diff --html`,
//! byte for byte (the template text is generated from Python's,
//! html_text.rs).

use crate::diff::{FileResult, LayerResult};
use crate::html_text::{CSS, EMPTY_SECTION, JS, PAGE};
use crate::pix::Rect;
use tmi_repo::layer_changes::PropChange;

/// `html.escape(s)` (quote=True).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// `string.Template(template).safe_substitute(mapping)`.
pub fn safe_substitute(template: &str, mapping: &[(&str, &str)]) -> String {
    let ident = |s: &str| -> usize {
        let b = s.as_bytes();
        if b.is_empty() || !(b[0].is_ascii_alphabetic() || b[0] == b'_') {
            return 0;
        }
        1 + b[1..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count()
    };
    let lookup = |name: &str| mapping.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$');
            rest = stripped;
            continue;
        }
        let n = ident(after);
        if n > 0 {
            let name = &after[..n];
            match lookup(name) {
                Some(v) => out.push_str(v),
                None => {
                    out.push('$');
                    out.push_str(name);
                }
            }
            rest = &after[n..];
            continue;
        }
        if let Some(inner) = after.strip_prefix('{') {
            let n = ident(inner);
            if n > 0 && inner[n..].starts_with('}') {
                let name = &inner[..n];
                match lookup(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push_str("${");
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &inner[n + 1..];
                continue;
            }
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    out
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// `_data_uri(data)`.
fn data_uri(data: &[u8]) -> String {
    let mime = if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp"
    } else if data.len() >= 3 && data[..3] == [0xFF, 0xD8, 0xFF] {
        "image/jpeg"
    } else {
        "image/png"
    };
    format!("data:{mime};base64,{}", base64(data))
}

/// `str(list)` of a bbox.
pub fn bbox_text(b: &Rect) -> String {
    format!("[{}, {}, {}, {}]", b.0, b.1, b.2, b.3)
}

fn summary_counts(results: &[FileResult]) -> [usize; 6] {
    // files added / changed / deleted, layers added / changed / deleted
    let mut c = [0usize; 6];
    for fr in results {
        match fr.state {
            "added" => c[0] += 1,
            "deleted" => c[2] += 1,
            _ => c[1] += 1,
        }
        if fr.raster {
            continue;
        }
        for layer in &fr.layers {
            match layer.state {
                "added" => c[3] += 1,
                "changed" => c[4] += 1,
                "deleted" => c[5] += 1,
                _ => {}
            }
        }
    }
    c
}

fn file_states(fr: &FileResult) -> Vec<&'static str> {
    let mut states: Vec<&'static str> = Vec::new();
    if fr.state == "deleted" {
        states.push("deleted");
    }
    if fr.bbox.is_some() {
        states.push("changed");
    }
    for layer in &fr.layers {
        states.push(layer.state);
    }
    states.sort();
    states.dedup();
    states
}

fn render_file_section(fr: &FileResult) -> String {
    let path = escape(&fr.path);
    let states = file_states(fr);
    let states_attr = if states.is_empty() { "changed".to_string() } else { states.join(",") };
    if fr.state == "deleted" {
        return safe_substitute(
            "<section class=\"file\" data-states=\"$states\">\n  <h2><span class=\"state deleted\">deleted</span> $path</h2>\n  <p class=\"note\">file removed from working tree</p>\n</section>",
            &[("states", &states_attr), ("path", &path)],
        );
    }
    let composite = render_composite(fr);
    let layers_table = if fr.raster { String::new() } else { render_layers_table(&fr.layers) };
    let bbox_label = if fr.raster { "changed pixels bbox" } else { "composite bbox" };
    let bbox_line = match &fr.bbox {
        Some(b) => format!("<p class=\"bbox\">{bbox_label}: {}</p>", escape(&bbox_text(b))),
        None => String::new(),
    };
    safe_substitute(
        "<section class=\"file\" data-states=\"$states\">\n  <h2>$path</h2>\n  $bbox_line\n  $composite\n  $layers\n</section>",
        &[("states", &states_attr), ("path", &path), ("bbox_line", &bbox_line), ("composite", &composite), ("layers", &layers_table)],
    )
}

fn render_composite(fr: &FileResult) -> String {
    let Some(h) = &fr.html else { return String::new() };
    let sbs = data_uri(&h.side_by_side_png);
    let ovl = data_uri(&h.overlay_png);
    let pl = data_uri(&h.diff_preview_png);
    let old = data_uri(&h.old_png);
    let new = data_uri(&h.new_png);
    let (w, hgt) = h.size;
    let aspect = format!("{} / {}", w.max(1), hgt.max(1));
    let mut full = String::new();
    if !h.full_old.is_empty() && !h.full_new.is_empty() {
        full = format!(
            "<p class=\"note\">full size: <a href=\"{}\">old</a> / <a href=\"{}\">new</a> ({w}&times;{hgt})</p>\n",
            escape(&h.full_old),
            escape(&h.full_new)
        );
    }
    full + &safe_substitute(
        concat!(
            "<div class=\"composite\">\n",
            "  <div class=\"tabs\" role=\"tablist\">\n",
            "    <button data-tab=\"sbs\" aria-selected=\"true\">side-by-side</button>\n",
            "    <button data-tab=\"ovl\" aria-selected=\"false\">overlay</button>\n",
            "    <button data-tab=\"pl\" aria-selected=\"false\">per-layer</button>\n",
            "    <button data-tab=\"sw\" aria-selected=\"false\">swipe</button>\n",
            "  </div>\n",
            "  <div class=\"panels\">\n",
            "    <div data-panel=\"sbs\" class=\"panel active\"><img src=\"$sbs\" alt=\"side-by-side\"></div>\n",
            "    <div data-panel=\"ovl\" class=\"panel\"><img src=\"$ovl\" alt=\"overlay\"></div>\n",
            "    <div data-panel=\"pl\" class=\"panel\"><img src=\"$pl\" alt=\"per-layer\"></div>\n",
            "    <div data-panel=\"sw\" class=\"panel swipe\">\n",
            "      <div class=\"swipe-wrap\" style=\"aspect-ratio: $aspect;\">\n",
            "        <img class=\"swipe-old\" src=\"$old\" alt=\"old\">\n",
            "        <img class=\"swipe-new\" src=\"$new\" alt=\"new\" style=\"clip-path: inset(0 0 0 50%);\">\n",
            "        <div class=\"swipe-handle\" style=\"left:50%;\"></div>\n",
            "      </div>\n",
            "      <input type=\"range\" min=\"0\" max=\"100\" value=\"50\" class=\"swipe-range\" aria-label=\"swipe position\">\n",
            "    </div>\n",
            "  </div>\n",
            "</div>"
        ),
        &[("sbs", &sbs), ("ovl", &ovl), ("pl", &pl), ("old", &old), ("new", &new), ("aspect", &aspect)],
    )
}

fn render_layers_table(layers: &[LayerResult]) -> String {
    if layers.is_empty() {
        return String::new();
    }
    let rows: Vec<String> = layers.iter().map(render_layer_row).collect();
    format!(
        "<div class=\"table-wrap\">\n<table class=\"layers\">\n  <thead><tr><th>state</th><th>layer</th><th>property changes</th><th class=\"bbox-col\">bbox</th><th>preview</th></tr></thead>\n  <tbody>\n{}\n  </tbody>\n</table>\n</div>",
        rows.join("\n")
    )
}

fn render_layer_row(layer: &LayerResult) -> String {
    let name = escape(&layer.name);
    let bbox = match &layer.bbox {
        Some(b) => escape(&bbox_text(b)),
        None => "&mdash;".into(),
    };
    let props = render_prop_changes(&layer.prop_changes);
    let preview = render_layer_preview(layer);
    safe_substitute(
        concat!(
            "    <tr class=\"layer\" data-state=\"$state\">\n",
            "      <td><span class=\"state $state\">$state</span></td>\n",
            "      <td>$name</td>\n",
            "      <td>$props</td>\n",
            "      <td class=\"bbox-col\">$bbox</td>\n",
            "      <td>$preview</td>\n",
            "    </tr>"
        ),
        &[("state", layer.state), ("name", &name), ("props", &props), ("bbox", &bbox), ("preview", &preview)],
    )
}

fn render_prop_changes(changes: &[PropChange]) -> String {
    if changes.is_empty() {
        return "&mdash;".into();
    }
    let parts: Vec<String> = changes
        .iter()
        .map(|(key, old, new)| {
            let key = escape(key);
            if old.is_none() && new.is_none() {
                return format!("<div class=\"prop\"><code>{key}</code> changed</div>");
            }
            let s = |v: &Option<String>| escape(v.as_deref().unwrap_or("None"));
            format!("<div class=\"prop\"><code>{key}</code>: {} &rarr; {}</div>", s(old), s(new))
        })
        .collect();
    parts.join("\n")
}

fn render_layer_preview(layer: &LayerResult) -> String {
    let Some((sbs, ovl)) = &layer.html else { return "&mdash;".into() };
    safe_substitute(
        concat!(
            "<details>\n",
            "  <summary>show</summary>\n",
            "  <div class=\"layer-preview\">\n",
            "    <figure><figcaption>side-by-side</figcaption><img src=\"$sbs\" alt=\"side-by-side\"></figure>\n",
            "    <figure><figcaption>overlay</figcaption><img src=\"$ovl\" alt=\"overlay\"></figure>\n",
            "  </div>\n",
            "</details>"
        ),
        &[("sbs", &data_uri(sbs)), ("ovl", &data_uri(ovl))],
    )
}

/// `render_report(results, out_path)` with `ts` (`%Y-%m-%d %H:%M:%S`):
/// the page text (written with `write_text`, so "\n" is the platform's).
pub fn render_report(results: &[FileResult], ts: &str) -> String {
    let c = summary_counts(results);
    let sections: Vec<String> = results.iter().map(render_file_section).collect();
    let sections = if sections.is_empty() { EMPTY_SECTION.to_string() } else { sections.join("\n") };
    let counts: Vec<String> = c.iter().map(|n| n.to_string()).collect();
    let title = escape(&format!("tmiki diff @ {ts}"));
    let ts_text = escape(ts);
    safe_substitute(
        PAGE,
        &[
            ("title", &title),
            ("css", CSS),
            ("js", JS),
            ("ts", &ts_text),
            ("files_added", &counts[0]),
            ("files_changed", &counts[1]),
            ("files_deleted", &counts[2]),
            ("layers_added", &counts[3]),
            ("layers_changed", &counts[4]),
            ("layers_deleted", &counts[5]),
            ("sections", &sections),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_and_escape_as_python() {
        assert_eq!(escape("<a href=\"x\">'&'</a>"), "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;&#x27;&lt;/a&gt;");
        let m = [("a", "1"), ("b_c", "$x")];
        assert_eq!(safe_substitute("$a ${a} $$ $b_c $nope ${nope} $ $1 ${a", &m), "1 1 $ $x $nope ${nope} $ $1 ${a");
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(bbox_text(&(1, 2, 30, 40)), "[1, 2, 30, 40]");
    }
}
