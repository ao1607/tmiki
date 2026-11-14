//! A1: layer trees whose layers share one subtree (objects are
//! content-addressed, so this is a few small objects): walking them visited
//! the shared subtree once per reference, a billion layers for three
//! levels of a thousand, until memory ran out (merge, pull, diff, restore).

use tmi_repo::repo::Repo;
use tmi_store::json::{self, Value};

fn layer_tree(layers: &[String]) -> Value {
    json::loads(&format!(r#"{{"version":1,"kind":"layer-tree","layers":[{}]}}"#, layers.join(","))).unwrap()
}

#[test]
fn a_layer_tree_sharing_one_subtree_many_times_is_refused_not_walked_for_ever() {
    let dir = std::env::temp_dir().join(format!("tmi-repo-wide-tree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".tmi").join("objects")).unwrap();
    let repo = Repo::new(None, dir.join(".tmi"));
    let leaves: Vec<String> = (0..1100).map(|i| format!(r#"{{"name":"l{i}","layer_id":"{i}"}}"#)).collect();
    let mut sub = repo.store.write_json("tree", &layer_tree(&leaves)).unwrap();
    for _ in 0..2 {
        let folders: Vec<String> = (0..1100).map(|i| format!(r#"{{"name":"f{i}","layer_id":"{i}","subtree_ref":"{sub}"}}"#)).collect();
        sub = repo.store.write_json("tree", &layer_tree(&folders)).unwrap();
    }
    let mut flat = Vec::new();
    let err = tmi_repo::stage::flatten_layers(&repo, &sub, &mut flat, 0).unwrap_err();
    assert!(err.to_string().contains("layer tree too large"), "{err}");
    let mut walked = Vec::new();
    let err = tmi_repo::layer_changes::walk_tree(&repo, &sub, (Value::Null, String::new()), &mut walked, 0).unwrap_err();
    assert!(err.to_string().contains("layer tree too large"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
