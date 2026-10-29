//! Deletes from table B-trees, checked against the real `sqlite3` CLI.
//!
//! Deleting a contiguous range of rowids used to leave empty leaf pages behind
//! (and interior cells pointing at them), which sqlite3 reports as "database disk
//! image is malformed". Every scenario runs on a plain table and on one with a
//! UNIQUE column, so that the table and its index are deleted from together, and
//! after every step sqlite3 must pass `PRAGMA integrity_check` and return the same
//! rows as the engine.

mod common;

use common::*;
use rust_sqllite::Database;
use std::path::Path;

const SCHEMAS: [(&str, &str); 2] = [
    ("plain", "CREATE TABLE p (id INTEGER PRIMARY KEY, v TEXT);"),
    (
        "unique",
        "CREATE TABLE p (id INTEGER PRIMARY KEY, v TEXT UNIQUE);",
    ),
];

/// Insert rows `from..=to`, one statement each (as typed into the REPL).
fn insert_rows(db: &mut Database, from: usize, to: usize) {
    for i in from..=to {
        exec(
            db,
            &format!(
                "INSERT INTO p (v) VALUES ('value {} padded out a little');",
                i
            ),
        );
    }
}

/// Create `p` from `schema` and insert `n` rows.
fn create(path: &Path, schema: &str, n: usize) -> Database {
    let mut db = Database::open(path).unwrap();
    exec(&mut db, schema);
    insert_rows(&mut db, 1, n);
    db
}

/// For each schema: insert `n` rows, then run each delete (checking the number of
/// rows it removed) with a sqlite3 check after each one.
fn run_deletes(name: &str, n: usize, deletes: &[(&str, u64)]) {
    for (kind, schema) in SCHEMAS {
        let path = tmp_db(&format!("{}_{}", name, kind));
        let mut db = create(&path, schema, n);
        let mut left = n as u64;
        for (sql, removed) in deletes {
            assert_eq!(exec(&mut db, sql), *removed, "{} ({} table)", sql, kind);
            left -= removed;
            db = close_and_check(db, &path, "p", &format!("{} ({} table)", sql, kind));
            if let Some(count) = sqlite3_number(&path, "SELECT count(*) FROM p;") {
                assert_eq!(count, left, "{} ({} table)", sql, kind);
            }
        }
        db.close().unwrap();
        cleanup(&path);
    }
}

#[test]
fn deleting_the_lower_half_keeps_the_file_valid() {
    // The reported reproduction.
    run_deletes("lower", 2000, &[("DELETE FROM p WHERE id <= 1000;", 1000)]);
}

#[test]
fn deleting_the_upper_half_keeps_the_file_valid() {
    run_deletes("upper", 2000, &[("DELETE FROM p WHERE id > 1000;", 1000)]);
}

#[test]
fn deleting_a_middle_range_keeps_the_file_valid() {
    run_deletes(
        "middle",
        2000,
        &[("DELETE FROM p WHERE id >= 500 AND id <= 1500;", 1001)],
    );
}

#[test]
fn deleting_everything_frees_the_pages_and_inserting_again_reuses_them() {
    for (kind, schema) in SCHEMAS {
        let what = format!("{} table", kind);
        let path = tmp_db(&format!("everything_{}", kind));
        let mut db = create(&path, schema, 2000);
        db = close_and_check(db, &path, "p", &what);
        let pages = sqlite3_number(&path, "PRAGMA page_count;");

        assert_eq!(exec(&mut db, "DELETE FROM p;"), 2000);
        db = close_and_check(db, &path, "p", &what);
        if let (Some(pages), Some(free)) = (pages, sqlite3_number(&path, "PRAGMA freelist_count;"))
        {
            // Everything but page 1 and the root pages (table, and index if any)
            // is on the freelist.
            let roots = if kind == "plain" { 1 } else { 2 };
            assert_eq!(free, pages - 1 - roots, "{}", what);
        }

        insert_rows(&mut db, 1, 2000);
        db = close_and_check(db, &path, "p", &what);
        if let (Some(before), Some(after)) = (pages, sqlite3_number(&path, "PRAGMA page_count;")) {
            assert!(
                after <= before + 2,
                "{}: freed pages not reused ({} pages, then {})",
                what,
                before,
                after
            );
        }
        if let Some(count) = sqlite3_number(&path, "SELECT count(*) FROM p;") {
            assert_eq!(count, 2000);
        }
        db.close().unwrap();
        cleanup(&path);
    }
}

#[test]
fn mass_deletes_on_10000_rows_keep_the_file_valid() {
    for (kind, schema) in SCHEMAS {
        let path = tmp_db(&format!("mass_{}", kind));
        let mut db = create(&path, schema, 10_000);
        let step = |db: Database, sql: &str, removed: u64| -> Database {
            let mut db = db;
            let what = format!("{} ({} table)", sql, kind);
            assert_eq!(exec(&mut db, sql), removed, "{}", what);
            close_and_check(db, &path, "p", &what)
        };
        db = step(db, "DELETE FROM p WHERE id <= 2500;", 2500);
        db = step(db, "DELETE FROM p WHERE id > 9000;", 1000);
        db = step(db, "DELETE FROM p WHERE id >= 4000 AND id < 6000;", 2000);
        // Partly overlaps what is already gone: 2501..2999 and 8501..9000.
        db = step(db, "DELETE FROM p WHERE id < 3000 OR id > 8500;", 999);
        // Every third row of 6000..8500, one statement each.
        for id in (6000..=8500).step_by(3) {
            assert_eq!(
                exec(&mut db, &format!("DELETE FROM p WHERE id = {};", id)),
                1
            );
        }
        db = close_and_check(db, &path, "p", &format!("every third row ({} table)", kind));
        db = step(db, "DELETE FROM p WHERE id >= 3000 AND id <= 3999;", 1000);
        if let Some(count) = sqlite3_number(&path, "SELECT count(*) FROM p;") {
            assert_eq!(count, 2501 - 834);
        }
        db = step(db, "DELETE FROM p;", 2501 - 834);
        insert_rows(&mut db, 1, 3000);
        db = close_and_check(db, &path, "p", &format!("insert after ({} table)", kind));
        if let Some(count) = sqlite3_number(&path, "SELECT count(*) FROM p;") {
            assert_eq!(count, 3000);
        }
        db.close().unwrap();
        cleanup(&path);
    }
}
