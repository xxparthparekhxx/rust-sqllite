//! Helpers shared by the tests that check this engine's files with the real
//! `sqlite3` CLI. If `sqlite3` is not installed, its checks are skipped with a note.

#![allow(dead_code)]

use rust_sqllite::{Database, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A fresh database path in the temp directory, unique to this test process.
pub fn tmp_db(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rustsqlite_{}_{}", std::process::id(), name));
    cleanup(&p);
    p
}

pub fn cleanup(db: &Path) {
    let _ = fs::remove_file(db);
    let _ = fs::remove_file(wal_path(db));
}

pub fn wal_path(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", db.display()))
}

/// Size of the engine's WAL file: a statement that writes anything appends to it.
pub fn wal_len(db: &Path) -> u64 {
    fs::metadata(wal_path(db)).map(|m| m.len()).unwrap_or(0)
}

/// Run `sql` through the sqlite3 CLI and return stdout and stderr, trimmed.
/// None if sqlite3 is not installed.
pub fn sqlite3(db: &Path, sql: &str) -> Option<String> {
    let out = match Command::new("sqlite3").arg(db).arg(sql).output() {
        Ok(out) => out,
        Err(_) => {
            eprintln!("sqlite3 not found; skipping sqlite3 checks");
            return None;
        }
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(text.trim_end().to_string())
}

pub fn assert_integrity_ok(db: &Path, what: &str) {
    if let Some(out) = sqlite3(db, "PRAGMA integrity_check;") {
        assert_eq!(out, "ok", "sqlite3 integrity_check ({})", what);
    }
}

pub fn exec(db: &mut Database, sql: &str) -> u64 {
    db.execute(sql)
        .unwrap_or_else(|e| panic!("{}: {}", sql, e))
        .rows_affected
}

/// Run `sql`, which must fail with exactly `error`.
pub fn assert_error(db: &mut Database, sql: &str, error: &str) {
    match db.execute(sql) {
        Ok(r) => panic!(
            "expected \"{}\" for: {} (got {} rows affected)",
            error, sql, r.rows_affected
        ),
        Err(e) => assert_eq!(e, error, "{}", sql),
    }
}

/// Rows the way the sqlite3 CLI prints them by default: '|' between columns,
/// NULL as nothing.
pub fn as_sqlite_text(rows: &[Vec<Value>]) -> String {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    v => v.to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Close `db`, check that sqlite3 finds the file intact and that its
/// `SELECT * FROM <table> ORDER BY id` matches the engine's rows, and reopen it.
pub fn close_and_check(mut db: Database, path: &Path, table: &str, what: &str) -> Database {
    let ours = db
        .execute(&format!("SELECT * FROM {};", table))
        .unwrap()
        .rows;
    db.close().unwrap();
    assert_integrity_ok(path, what);
    if let Some(out) = sqlite3(path, &format!("SELECT * FROM {} ORDER BY id;", table)) {
        assert_eq!(
            out,
            as_sqlite_text(&ours),
            "{}: sqlite3 and the engine disagree",
            what
        );
    }
    Database::open(path).unwrap()
}

/// A single-number sqlite3 query result, such as a count or a PRAGMA value.
pub fn sqlite3_number(db: &Path, sql: &str) -> Option<u64> {
    sqlite3(db, sql).map(|out| {
        out.parse()
            .unwrap_or_else(|_| panic!("{}: not a number: {}", sql, out))
    })
}
