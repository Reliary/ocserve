//! SQLite one-shot schema-glitch recovery in the writer's `bind()` (found
//! 2026-10-11 during the importer-crash-window work).
//!
//! Reproduction of the original bug: a long-lived writer connection that has
//! used `prepare_cached` (SQLITE_PREPARE_PERSISTENT) afterwards sees its next
//! FRESH prepare fail with a spurious `no such table: msg` when ANOTHER
//! connection on the same file recreated a DDL object in between (the FTS
//! self-heal recreates `part_search_ai`). `sqlite_master` on the same
//! connection clearly contains the table, and a second prepare of the same
//! SQL succeeds — so the failure is one-shot and recovering it with a
//! statement-cache flush + single retry is correct.
//!
//! This test drives the exact sequence through the public writer API. It is
//! the permanent negative-control pair for the retry in `writer.rs`: with the
//! retry removed (returning the first error), `insert` fails with
//! "no such table: msg" and this test goes red.
use ocserve_store::{WriteOp, Writer};

const MSG_INSERT: &str = "INSERT OR REPLACE INTO msg (id, session_id, role, seq, time_created, info) \
     VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(seq),0)+1 FROM msg WHERE session_id=?2), ?4, ?5)";

#[test]
fn writer_recovers_from_foreign_ddl_schema_glitch() {
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());

    // 1. a first writer creates schema + one message (its connection used
    //    prepare_cached for migrate()'s DDL and the inserts).
    let w1 = Writer::spawn(db.clone()).unwrap();
    w1.write(vec![WriteOp::Sql {
        sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_g','global','/w','s','s','t','1',1,1)".into(),
        params: vec![],
    }])
    .unwrap();
    drop(w1);

    // 2. a foreign connection changes the schema the way the crash-window
    //    self-heal does: wipe the index and recreate the trigger.
    {
        let b = ocserve_store::pragma::open_writer(&db).unwrap();
        b.execute_batch(
            "INSERT INTO part_search_fts(part_search_fts) VALUES('delete-all');
             DROP TRIGGER part_search_ai;",
        )
        .unwrap();
    }

    // 3. the long-lived writer (fresh spawn = fresh cache, but the glitch is
    //    triggered by the FOREIGN DDL seen at first prepare) must survive:
    //    first prepared statement on this connection is the msg insert.
    let w2 = Writer::spawn(db.clone()).unwrap();
    {
        let b = ocserve_store::pragma::open_writer(&db).unwrap();
        b.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS part_search_ai AFTER INSERT ON part_search BEGIN
                 INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
             END",
        )
        .unwrap();
    }
    let r = w2.write(vec![WriteOp::Sql {
        sql: MSG_INSERT.into(),
        params: vec![
            "m1".into(),
            "ses_g".into(),
            "user".into(),
            1i64.into(),
            "{}".into(),
        ],
    }]);
    assert!(
        r.is_ok(),
        "writer must recover from the one-shot schema glitch: {r:?}"
    );

    // And the recovered write is durable.
    let c = ocserve_store::pragma::open_reader(&db).unwrap();
    let n: i64 = c
        .query_row("SELECT count(*) FROM msg WHERE id='m1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}
