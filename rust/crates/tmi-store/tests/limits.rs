//! A hostile remote's chunked objects and oversize objects, as the store reads
//! them: parts must be plain blobs, the running total never passes the declared
//! size, and an object past its type's cap is refused while inflating.
//! The Python side's twins are tests/test_object_limits.py.

use tmi_store::json::{self, Value};
use tmi_store::ObjectStore;

fn store(name: &str) -> ObjectStore {
    let dir = std::env::temp_dir().join(format!("tmi-limits-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    ObjectStore::new(dir.join("objects"))
}

fn manifest(size: i64, parts: &[&str], glue: Option<&str>) -> Value {
    let list: Vec<String> = parts.iter().map(|p| if p.len() == 64 { format!("\"{p}\"") } else { p.to_string() }).collect();
    let glue = glue.map(|g| format!("\"{g}\"")).unwrap_or("null".into());
    json::loads(&format!(r#"{{"size":{size},"glue":{glue},"parts":[{}]}}"#, list.join(","))).unwrap()
}

#[test]
fn repeated_part_stops_at_the_declared_size() {
    let store = store("repeat");
    let part = store.write("blob", &vec![0u8; 100_000]).unwrap();
    let parts: Vec<&str> = std::iter::repeat(part.as_str()).take(5000).collect();
    let oid = store.write_json("chunked", &manifest(100_000, &parts, None)).unwrap();
    let err = store.read_blob(&oid).unwrap_err().to_string();
    assert!(err.contains("more than 100000 bytes"), "{err}");
}

#[test]
fn nested_chunked_part_is_refused() {
    let store = store("nested");
    let leaf = store.write("blob", b"abc").unwrap();
    let inner = store.write_json("chunked", &manifest(3, &[&leaf], None)).unwrap();
    let outer = store.write_json("chunked", &manifest(3, &[&inner], None)).unwrap();
    let err = store.read_blob(&outer).unwrap_err().to_string();
    assert!(err.contains("expected blob"), "{err}");
}

#[test]
fn a_deep_chain_of_chunked_objects_does_not_recurse() {
    let store = store("chain");
    let mut oid = store.write("blob", b"abc").unwrap();
    for _ in 0..2000 {
        oid = store.write_json("chunked", &manifest(3, &[&oid], None)).unwrap();
    }
    assert!(store.read_blob(&oid).is_err());
}

#[test]
fn honest_chunked_blob_reads_back() {
    let store = store("honest");
    let a = store.write("blob", &[b'a'; 10]).unwrap();
    let b = store.write("blob", &[b'b'; 7]).unwrap();
    let glue = store.write("blob", b"ZZ").unwrap();
    let oid = store.write_json("chunked", &manifest(29, &[&a, "1", &b, &a, "1"], Some(&glue))).unwrap();
    let mut want = vec![b'a'; 10];
    want.push(b'Z');
    want.extend([b'b'; 7]);
    want.extend([b'a'; 10]);
    want.push(b'Z');
    assert_eq!(store.read_blob(&oid).unwrap(), want);
}

#[test]
fn write_raw_refuses_deep_json_and_bad_manifests() {
    let store = store("raw");
    let pack = |t: &str, payload: &[u8]| {
        let oid = tmi_store::oid::object_id(t, payload);
        let mut data = t.as_bytes().to_vec();
        data.push(0);
        data.extend_from_slice(payload);
        (oid, tmi_store::zlib::compress(&data, 1))
    };
    let deep = [vec![b'['; 3000], vec![b']'; 3000]].concat();
    let (oid, raw) = pack("commit", &deep);
    assert!(store.write_raw(&oid, &raw).unwrap_err().to_string().contains("nested deeper"));
    assert!(!store.exists(&oid));
    let (oid, raw) = pack("chunked", br#"{"size":1,"parts":["../x"]}"#);
    assert!(store.write_raw(&oid, &raw).is_err());
    assert!(!store.exists(&oid));
}

#[test]
fn oversize_object_on_disk_is_refused_on_read() {
    let store = store("big");
    let payload = vec![b'0'; tmi_store::limits::MAX_META_OBJECT_SIZE + 1];
    // written below the API, as a hostile peer's files would be
    let oid = tmi_store::oid::object_id("tree", &payload);
    let mut data = b"tree\0".to_vec();
    data.extend_from_slice(&payload);
    let path = store.path_for(&oid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, tmi_store::zlib::compress(&data, 1)).unwrap();
    assert!(store.read(&oid).unwrap_err().to_string().contains("larger than"));
}
