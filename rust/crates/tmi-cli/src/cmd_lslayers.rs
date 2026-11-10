//! `tmiki ls-layers <path> [<rev>]` (cli.py `cmd_ls_layers`) from the stored
//! layer tree: of a commit, or of the working file when its bytes are the
//! ones HEAD or the index stored. Anything else is imported here.

use crate::args::{self, positional};
use crate::cmd_log::fwd;
use crate::out::Out;
use crate::CmdResult;
use clap::Command;
use tmi_repo::write::{entries_of, str_of};
use tmi_repo::{Repo, RepoError, Result};
use tmi_store::json::{Int, Value};

/// `flatten_layers(tree_ref, prefix)`: (layer, path_name).
fn flatten(repo: &Repo, tree_ref: &str, prefix: Option<&str>, out: &mut Vec<(Value, Value)>) -> Result<()> {
    let tree = repo.store.read_json(tree_ref, Some("tree"))?;
    let layers = match tree.get("layers") {
        None => return Ok(()),
        Some(Value::Array(l)) => l.clone(),
        Some(_) => return Err(fwd("layers is not a list")),
    };
    for layer in layers {
        let name = layer.get("name").ok_or_else(|| fwd("layer without a name (KeyError)"))?;
        let path_name = match prefix {
            Some(p) if !p.is_empty() => Value::Str(format!("{p}/{}", name.py_str().ok_or_else(|| fwd("container name"))?)),
            _ => name.clone(),
        };
        let sub = match layer.get("subtree_ref") {
            Some(v) if v.truthy() => Some(v.as_str().ok_or_else(|| fwd("subtree_ref is not a str"))?.to_string()),
            _ => None,
        };
        out.push((layer, path_name.clone()));
        if let Some(sub) = sub {
            let p = path_name.py_str().ok_or_else(|| fwd("container path"))?;
            flatten(repo, &sub, Some(&p), out)?;
        }
    }
    Ok(())
}

/// layer_props `format_opacity(value)`.
fn format_opacity(v: Option<&Value>) -> Result<String> {
    let x = match v {
        None | Some(Value::Null) => return Ok(String::new()),
        Some(Value::Int(Int::Small(n))) => *n as f64,
        Some(Value::Float(f)) => *f,
        Some(Value::Bool(_)) | Some(Value::Str(_)) => return v.unwrap().py_str().ok_or_else(|| fwd("opacity")),
        _ => return Err(fwd("opacity of an unusual type")),
    };
    let r = (x * 100.0).round_ties_even();
    if !r.is_finite() || r.abs() > 1e15 {
        return Err(fwd("opacity out of range"));
    }
    Ok(format!("{}%", r as i64))
}

fn text(v: Option<&Value>) -> Result<String> {
    match v {
        None => Ok("None".into()),
        Some(x) => x.py_str().ok_or_else(|| fwd("container layer field")),
    }
}

/// `str(x or '')`.
fn text_or_empty(v: Option<&Value>) -> Result<String> {
    match v {
        Some(x) if x.truthy() => text(Some(x)),
        _ => Ok(String::new()),
    }
}

pub fn ls_layers(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("ls-layers").arg(positional("path", false)).arg(positional("rev", false));
    let m = args::parse(cmd, argv, &["path", "rev"])?;
    let Some(path) = args::get_str(&m, "path") else { return Err(fwd("ls-layers without a path")) };
    let rev = args::get_str(&m, "rev").filter(|r| !r.is_empty());
    let repo = Repo::open()?;
    let mut layers = Vec::new();
    if let Some(rev) = rev {
        let mut warnings = Vec::new();
        let commit = repo.resolve_rev(Some(&rev), &mut warnings);
        for w in warnings {
            out.eprintln(w);
        }
        let entries = repo.entries_for_commit(commit?.as_deref())?;
        let rel = repo.pathspec_relpath(&path, &|p| entries.contains_key(p))?;
        let Some(entry) = entries.get(&rel) else {
            return Err(RepoError::runtime(format!("path not found in {rev}: {rel}")));
        };
        let tree = str_of(entry, "tree_ref")?.ok_or_else(|| fwd("entry without a tree_ref"))?;
        flatten(&repo, &tree, None, &mut layers)?;
    } else {
        let full = repo.pathspec_full_path(&path)?;
        if !full.is_file() {
            return Err(RepoError::runtime(format!("no such file: {path}")));
        }
        let rel = repo.relpath(&full)?;
        let mut known = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
        for (p, e) in entries_of(&repo.read_index()?)? {
            if e.get("tree_ref").is_some_and(Value::truthy) {
                known.insert(p, e);
            }
        }
        let digest = tmi_repo::fsio::sha256_file(&full)?;
        match known.get(&rel) {
            Some(entry) if entry.truthy() && Some(&digest) == str_of(entry, "source_digest")?.as_ref() => {
                let tree = str_of(entry, "tree_ref")?.ok_or_else(|| fwd("entry without a tree_ref"))?;
                flatten(&repo, &tree, None, &mut layers)?;
            }
            _ => {
                // a working file the store does not hold: imported here
                let ctx = tmi_repo::stage::ImportContext::for_repo(&repo)?;
                // without the plugin: where the recorded layers are (issue #50)
                if let Some((id, u)) = ctx.formats.unavailable(&repo, &rel) {
                    let committed = repo.entries_for_commit(repo.current_commit()?.as_deref())?.contains_key(&rel);
                    let message = tmi_repo::formats::not_available_message(&rel, &id, &u);
                    return Err(RepoError::runtime(if committed {
                        format!("{message}; 'tmiki ls-layers {rel} HEAD' lists its layers as last committed")
                    } else {
                        message
                    }));
                }
                let bytes = tmi_repo::fsio::read_bytes(&full)?;
                let none = tmi_import::LayerBlobs::Only(Default::default());
                let opts = tmi_import::Options {
                    render_composite: false,
                    cache: None,
                    reuse_pixels: None,
                    reuse_decoded: None,
                    layer_blobs: &none,
                    composite_base: None,
                    limits: ctx.limits,
                    verify_partial: false,
                    progress: None,
                };
                let name = tmi_repo::stage::file_name(&full);
                let mut warns = tmi_import::Warnings::default();
                let imported = ctx.formats.import(&repo, &rel, &name, &bytes, &digest, &opts, &mut warns)?;
                for w in warns.lines {
                    out.eprintln(w);
                }
                fn walk(layers: &[tmi_import::ImportedLayer], prefix: &str, out: &mut Vec<(Value, Value)>) {
                    for l in layers {
                        let path_name = if prefix.is_empty() { l.name.clone() } else { format!("{prefix}/{}", l.name) };
                        let mut m = tmi_store::json::Map::new();
                        for key in ["layer_id", "name", "kind", "visible", "opacity", "blend_mode"] {
                            m.insert(key.into(), l.field(key));
                        }
                        out.push((Value::Object(m), Value::Str(path_name.clone())));
                        walk(&l.children, &path_name, out);
                    }
                }
                walk(&imported.layers, "", &mut layers);
            }
        }
    }
    out.println(format!("{:14} {:10} {:5} {:>7}  {:12} path", "id", "kind", "shown", "opacity", "blend"));
    for (layer, path_name) in layers {
        let shown = if layer.get("visible").is_some_and(Value::truthy) { "yes" } else { "no" };
        out.println(format!(
            "{:14} {:10} {:5} {:>7}  {:12} {}",
            text(layer.get("layer_id"))?,
            text_or_empty(layer.get("kind"))?,
            shown,
            format_opacity(layer.get("opacity"))?,
            text_or_empty(layer.get("blend_mode"))?,
            text(Some(&path_name))?
        ));
    }
    Ok(0)
}
