//! The one way a CLIP file's SQLite database is opened (the importer's
//! reader, `ClipDoc` and the write-back): its schema is the file's, so it is
//! not trusted. A view would run the file's SQL inside every query (a
//! recursive one never ends, `zeroblob` makes gigabytes), and so would a
//! trigger on the write-back's statements; CSP writes no views and only
//! the bookkeeping triggers of research/clip-format.md §7, which are the
//! triggers let through. The connection is also defensive, keeps its
//! temporary data in memory, holds one value to `MAX_VALUE_BYTES` and stops
//! after `MAX_VM_STEPS` virtual-machine steps.

use rusqlite::config::DbConfig;
use rusqlite::limits::Limit;
use rusqlite::{Connection, MAIN_DB};

/// The largest value (a blob, a string, one row) a query may make. CSP's
/// blobs in the database are attributes, tables and the canvas preview
/// (a PNG no larger than the canvas): far below this.
pub const MAX_VALUE_BYTES: i32 = 256 << 20;

/// Virtual-machine steps a connection may run in all. Reading a CLIP is a
/// few index lookups a layer (millions of steps for thousands of layers);
/// this only ends a statement that would not.
pub const MAX_VM_STEPS: u64 = 1 << 34;

fn err(e: rusqlite::Error) -> String {
    format!("sqlite3: {e}")
}

/// Whether `sql` is one of CSP's bookkeeping triggers (whitespace aside):
/// `CREATE TRIGGER ExternalUpdateOf<C>On<T> UPDATE OF <C> ON <T> BEGIN INSERT
/// INTO RemovedExternal(ExternalID) VALUES(OLD.<C>); END`, or its `Delete`
/// twin on `DELETE ON <T>`.
pub fn is_csp_trigger(sql: &str) -> bool {
    let s = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let ident = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let Some(c) = s.strip_suffix("); END").and_then(|r| r.rsplit_once("VALUES(OLD.")).map(|(_, c)| c) else { return false };
    let Some(t) = s.split_once(" ON ").and_then(|(_, r)| r.split_once(' ')).map(|(t, _)| t) else { return false };
    if !ident(c) || !ident(t) {
        return false;
    }
    let tail = format!("BEGIN INSERT INTO RemovedExternal(ExternalID) VALUES(OLD.{c}); END");
    s == format!("CREATE TRIGGER ExternalUpdateOf{c}On{t} UPDATE OF {c} ON {t} {tail}")
        || s == format!("CREATE TRIGGER ExternalDeleteOf{c}On{t} DELETE ON {t} {tail}")
}

/// A connection with the limits above and none of the file's SQL that could
/// run, empty.
pub fn locked_connection() -> Result<Connection, String> {
    let con = Connection::open_in_memory().map_err(err)?;
    con.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true).map_err(err)?;
    con.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false).map_err(err)?;
    con.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE_BYTES).map_err(err)?;
    con.pragma_update(None, "temp_store", "MEMORY").map_err(err)?;
    const EVERY: i32 = 1000;
    let mut steps = 0u64;
    con.progress_handler(
        EVERY,
        Some(move || {
            steps += EVERY as u64;
            steps > MAX_VM_STEPS
        }),
    )
    .map_err(err)?;
    Ok(con)
}

/// The SQLite image `sqlite` (a SQLi chunk's payload) deserialized into a
/// [`locked_connection`]; Err when its schema has a view or a trigger CSP
/// does not write.
pub fn open(sqlite: &[u8], read_only: bool) -> Result<Connection, String> {
    let mut con = locked_connection()?;
    con.deserialize_read_exact(MAIN_DB, sqlite, sqlite.len(), read_only).map_err(err)?;
    let mut stmt = con.prepare("SELECT type, sql FROM sqlite_schema WHERE type IN ('view', 'trigger')").map_err(err)?;
    let odd: Vec<(String, Option<String>)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)?.collect::<Result<_, _>>().map_err(err)?;
    drop(stmt);
    for (kind, sql) in odd {
        if kind == "view" || !sql.as_deref().is_some_and(is_csp_trigger) {
            return Err(format!("the CLIP database has a {kind} CSP does not write"));
        }
    }
    Ok(con)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_triggers_pass_and_others_do_not() {
        let update = "CREATE TRIGGER ExternalUpdateOfBlockDataOnOffscreen UPDATE OF BlockData ON Offscreen\n   BEGIN INSERT INTO RemovedExternal(ExternalID) VALUES(OLD.BlockData); END";
        let delete = "CREATE TRIGGER ExternalDeleteOfBlockDataOnOffscreen DELETE ON Offscreen BEGIN INSERT INTO RemovedExternal(ExternalID) VALUES(OLD.BlockData); END";
        assert!(is_csp_trigger(update) && is_csp_trigger(delete));
        for bad in [
            update.replace("VALUES(OLD.BlockData)", "SELECT count(*) FROM Layer, Layer"),
            update.replace("ON Offscreen\n", "ON Layer "),
            delete.replace("RemovedExternal", "Layer"),
            update.replace("BlockData); END", "BlockData); DELETE FROM Layer; END"),
        ] {
            assert!(!is_csp_trigger(&bad), "{bad}");
        }
    }

    /// A file whose Canvas is a view that never ends (a recursive CTE), one
    /// whose Offscreen is a view making a gigabyte per row, and one with a
    /// trigger of its own on the write-back's UPDATE: refused when opened.
    /// The same file with CSP's triggers only (`minimal_clip` has them) opens.
    #[test]
    fn views_and_foreign_triggers_in_a_clip_database_are_refused() {
        use crate::testing::{minimal_clip, offscreen_attribute, outer_id, raster_layer_sql};
        let layer = raster_layer_sql(2, 0, &outer_id(1), &offscreen_attribute(4, 4, 1, 1));
        let honest = minimal_clip(4, 4, &layer, vec![]).unwrap();
        crate::ClipDoc::open(&honest).expect("CSP's triggers are let through");
        let hostile = [
            "ALTER TABLE Canvas RENAME TO C0; CREATE VIEW Canvas AS WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM r) \
             SELECT CanvasUnit, CanvasWidth, CanvasHeight, CanvasResolution, CanvasRootFolder FROM C0, r WHERE x < 0",
            "ALTER TABLE Offscreen RENAME TO O0; CREATE VIEW Offscreen AS SELECT MainId, zeroblob(999999999) AS Attribute, BlockData FROM O0",
            "CREATE TRIGGER t UPDATE OF Attribute ON Offscreen BEGIN SELECT count(*) FROM Layer a, Layer b, Layer c; END",
        ];
        for sql in hostile {
            let raw = minimal_clip(4, 4, &format!("{layer} {sql};"), vec![]).unwrap();
            let e = crate::ClipDoc::open(&raw).err().expect("refused");
            assert!(e.contains("CSP does not write"), "{e}");
        }
    }
}
