//! A push sends what the pushed commit reaches, not the whole store: other
//! branches, stashes and files added but never committed stay local.

use tmi_store::json;
use tmi_transport::ops;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tmi-push-closure-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_push_does_not_publish_objects_the_pushed_commit_does_not_reach() {
    let case = scratch("local");
    let repo = ops::create(&case.join("work"), false).unwrap();
    let hub = case.join("hub.tmi");
    ops::create(&hub, true).unwrap();
    repo.add_remote("origin", hub.to_str().unwrap()).unwrap();

    let public = repo.store.write("blob", b"public picture").unwrap();
    let secret = repo.store.write("blob", b"staged by mistake: password=hunter2").unwrap();
    let tree = repo
        .store
        .write_json("tree", &json::loads(&format!(r#"{{"kind":"repository","entries":[{{"path":"a.png","source_blob_ref":"{public}"}}]}}"#)).unwrap())
        .unwrap();
    let commit = repo
        .store
        .write_json("commit", &json::loads(&format!(r#"{{"parents":[],"tree_ref":"{tree}","message":"m"}}"#)).unwrap())
        .unwrap();
    // a second branch nobody pushes
    let other = repo
        .store
        .write_json("commit", &json::loads(&format!(r#"{{"parents":["{commit}"],"tree_ref":"{secret}","message":"x"}}"#)).unwrap())
        .unwrap();
    repo.write_ref("refs/heads/main", &commit).unwrap();
    repo.write_ref("refs/heads/private", &other).unwrap();

    let pushed = ops::push(&repo, "origin", Some("main"), false, false, &|_: &str| {}, &mut || {}).unwrap();
    assert_eq!(pushed.objects, 3);
    let remote = tmi_store::ObjectStore::new(hub.join("objects"));
    for oid in [&public, &tree, &commit] {
        assert!(remote.exists(oid), "{oid} was not pushed");
    }
    assert!(!remote.exists(&secret), "an object the pushed commit does not reach was published");
    assert!(!remote.exists(&other));
    let _ = std::fs::remove_dir_all(&case);
}
