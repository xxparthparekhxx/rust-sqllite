//! INTEGER PRIMARY KEY uniqueness and NOT NULL on UPDATE, checked against the real
//! `sqlite3` CLI. The expected errors are the messages sqlite3 3.50 gives for the
//! same statements. A rejected statement must write nothing: the WAL, where every
//! committed page goes, must not grow.

mod common;

use common::*;
use rust_sqllite::Database;

#[test]
fn duplicate_integer_primary_key_is_rejected_and_nothing_is_written() {
    let path = tmp_db("dup_ipk");
    let mut db = Database::open(&path).unwrap();
    exec(&mut db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);");
    exec(&mut db, "INSERT INTO t VALUES (1, 'one');");
    exec(&mut db, "INSERT INTO t (id, v) VALUES (2, 'two');");

    let wal = wal_len(&path);
    let dup = "UNIQUE constraint failed: t.id";
    assert_error(&mut db, "INSERT INTO t VALUES (1, 'dup');", dup);
    assert_error(&mut db, "INSERT INTO t (id, v) VALUES (2, 'dup');", dup);
    // A later row of the statement conflicts: the rows before it are not kept.
    assert_error(
        &mut db,
        "INSERT INTO t VALUES (3, 'three'), (1, 'dup');",
        dup,
    );
    // Two rows of one statement with the same id.
    assert_error(
        &mut db,
        "INSERT INTO t VALUES (4, 'four'), (4, 'dup');",
        dup,
    );
    assert_eq!(wal_len(&path), wal, "a rejected INSERT wrote pages");

    // A row inserted without an id gets the next free rowid, not a taken one.
    exec(&mut db, "INSERT INTO t (v) VALUES ('auto');");
    // Inside a transaction, the failed statement is undone and the rest kept.
    exec(&mut db, "BEGIN;");
    exec(&mut db, "INSERT INTO t VALUES (10, 'ten');");
    assert_error(&mut db, "INSERT INTO t VALUES (10, 'dup');", dup);
    exec(&mut db, "COMMIT;");
    let rows = db.execute("SELECT * FROM t;").unwrap().rows;
    assert_eq!(as_sqlite_text(&rows), "1|one\n2|two\n3|auto\n10|ten");

    // With a UNIQUE column too: the rowid is checked first, as SQLite does, and
    // the rejected row's value does not reach the index.
    exec(
        &mut db,
        "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT UNIQUE);",
    );
    exec(&mut db, "INSERT INTO u VALUES (1, 'a@x');");
    let wal = wal_len(&path);
    assert_error(
        &mut db,
        "INSERT INTO u VALUES (1, 'a@x');",
        "UNIQUE constraint failed: u.id",
    );
    assert_error(
        &mut db,
        "INSERT INTO u VALUES (1, 'b@x');",
        "UNIQUE constraint failed: u.id",
    );
    assert_eq!(wal_len(&path), wal, "a rejected INSERT wrote pages");
    exec(&mut db, "INSERT INTO u VALUES (2, 'b@x');");

    db = close_and_check(db, &path, "t", "table t");
    db = close_and_check(db, &path, "u", "table u");
    if let Some(out) = sqlite3(
        &path,
        "SELECT v FROM t WHERE id = 1; SELECT count(*) FROM u;",
    ) {
        assert_eq!(out, "one\n2");
    }
    db.close().unwrap();
    cleanup(&path);
}

#[test]
fn update_to_a_taken_integer_primary_key_is_rejected() {
    let path = tmp_db("update_ipk");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT UNIQUE, n INTEGER);",
    );
    exec(
        &mut db,
        "INSERT INTO u VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30);",
    );

    let wal = wal_len(&path);
    let dup = "UNIQUE constraint failed: u.id";
    assert_error(&mut db, "UPDATE u SET id = 2 WHERE id = 1;", dup);
    // Row 2 can move to 9, but then row 3 conflicts with it: neither moves.
    assert_error(&mut db, "UPDATE u SET id = 9 WHERE id >= 2;", dup);
    // The rowid is checked before the UNIQUE column.
    assert_error(
        &mut db,
        "UPDATE u SET id = 3, email = 'b' WHERE id = 1;",
        dup,
    );
    assert_error(
        &mut db,
        "UPDATE u SET id = NULL WHERE id = 1;",
        "datatype mismatch",
    );
    assert_eq!(wal_len(&path), wal, "a rejected UPDATE wrote pages");

    // A free id moves the row to that rowid; its index entry follows.
    assert_eq!(exec(&mut db, "UPDATE u SET id = 7 WHERE id = 2;"), 1);
    assert_eq!(exec(&mut db, "UPDATE u SET id = 1 WHERE id = 1;"), 1);
    let rows = db.execute("SELECT * FROM u;").unwrap().rows;
    assert_eq!(as_sqlite_text(&rows), "1|a|10\n3|c|30\n7|b|20");

    db = close_and_check(db, &path, "u", "table u");
    if let Some(out) = sqlite3(&path, "SELECT rowid, n FROM u WHERE email = 'b';") {
        assert_eq!(out, "7|20");
    }
    db.close().unwrap();
    cleanup(&path);
}

#[test]
fn update_rejects_null_in_a_not_null_column_before_writing() {
    let path = tmp_db("update_not_null");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT);",
    );
    exec(
        &mut db,
        "INSERT INTO t VALUES (1, 'a', 'x'), (2, 'b', 'y'), (3, 'c', NULL);",
    );

    let wal = wal_len(&path);
    let null = "NOT NULL constraint failed: t.name";
    assert_error(&mut db, "UPDATE t SET name = NULL WHERE id = 2;", null);
    assert_error(&mut db, "UPDATE t SET name = NULL;", null);
    // Only the last row gets a NULL: the rows before it are not changed either.
    assert_error(&mut db, "UPDATE t SET name = note;", null);
    // NOT NULL is checked before the rowid, as SQLite does.
    assert_error(
        &mut db,
        "UPDATE t SET name = NULL, id = 1 WHERE id = 2;",
        null,
    );
    assert_eq!(wal_len(&path), wal, "a rejected UPDATE wrote pages");

    // NULL in a column without NOT NULL is fine.
    assert_eq!(exec(&mut db, "UPDATE t SET note = NULL WHERE id = 1;"), 1);
    // Inside a transaction, the failed statement is undone and the rest kept.
    exec(&mut db, "BEGIN;");
    exec(&mut db, "UPDATE t SET name = 'B' WHERE id = 2;");
    assert_error(&mut db, "UPDATE t SET name = NULL WHERE id = 1;", null);
    exec(&mut db, "COMMIT;");
    let rows = db.execute("SELECT * FROM t;").unwrap().rows;
    assert_eq!(as_sqlite_text(&rows), "1|a|\n2|B|y\n3|c|");

    // A NOT NULL column with an index: neither the row nor the index changes.
    exec(
        &mut db,
        "CREATE TABLE w (id INTEGER PRIMARY KEY, email TEXT UNIQUE NOT NULL);",
    );
    exec(&mut db, "INSERT INTO w VALUES (1, 'a'), (2, 'b');");
    let wal = wal_len(&path);
    assert_error(
        &mut db,
        "UPDATE w SET email = NULL WHERE id = 2;",
        "NOT NULL constraint failed: w.email",
    );
    assert_eq!(wal_len(&path), wal, "a rejected UPDATE wrote pages");

    db = close_and_check(db, &path, "t", "table t");
    db = close_and_check(db, &path, "w", "table w");
    if let Some(out) = sqlite3(&path, "SELECT id FROM w WHERE email = 'b';") {
        assert_eq!(out, "2");
    }
    db.close().unwrap();
    cleanup(&path);
}

#[test]
fn not_null_integer_primary_key_in_a_file_written_by_sqlite3() {
    // sqlite3 stores NULL in the record for an INTEGER PRIMARY KEY, whose value is
    // the rowid, so NOT NULL must not be applied to that column on UPDATE.
    let path = tmp_db("sqlite3_written");
    let created = sqlite3(
        &path,
        "PRAGMA page_size = 4096;
         CREATE TABLE x (id INTEGER PRIMARY KEY NOT NULL, v TEXT NOT NULL);
         INSERT INTO x (v) VALUES ('a'), ('b');",
    );
    if created.is_none() {
        return;
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(exec(&mut db, "UPDATE x SET v = 'z' WHERE v = 'b';"), 1);
    assert_error(
        &mut db,
        "UPDATE x SET v = NULL;",
        "NOT NULL constraint failed: x.v",
    );
    // As in sqlite3, NULL for the INTEGER PRIMARY KEY is a datatype mismatch.
    assert_error(
        &mut db,
        "UPDATE x SET id = NULL WHERE v = 'a';",
        "datatype mismatch",
    );
    db.close().unwrap();
    assert_integrity_ok(&path, "table x");
    if let Some(out) = sqlite3(&path, "SELECT * FROM x ORDER BY id;") {
        assert_eq!(out, "1|a\n2|z");
    }
    cleanup(&path);
}
