//! Automatic indexes (sqlite_autoindex_*) checked against the real `sqlite3` CLI.
//!
//! Each test writes a database through this engine, closes it (which checkpoints
//! the WAL into the main file), and then asks `sqlite3` to validate and query it.
//! If `sqlite3` is not installed the sqlite3 checks are skipped with a note.

use rust_sqllite::{Database, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmp_db(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rustsqlite_autoindex_{}_{}",
        std::process::id(),
        name
    ));
    cleanup(&p);
    p
}

fn cleanup(db: &Path) {
    let _ = fs::remove_file(db);
    let _ = fs::remove_file(format!("{}-wal", db.display()));
}

/// Run `sql` through the sqlite3 CLI. None if sqlite3 is not installed.
fn sqlite3(db: &Path, sql: &str) -> Option<String> {
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

fn assert_integrity_ok(db: &Path) {
    if let Some(out) = sqlite3(db, "PRAGMA integrity_check;") {
        assert_eq!(out, "ok", "sqlite3 integrity_check on {}", db.display());
    }
}

fn exec(db: &mut Database, sql: &str) -> u64 {
    db.execute(sql)
        .unwrap_or_else(|e| panic!("{}: {}", sql, e))
        .rows_affected
}

fn assert_unique_error(db: &mut Database, sql: &str, column: &str) {
    match db.execute(sql) {
        Ok(_) => panic!("expected UNIQUE failure for: {}", sql),
        Err(e) => assert_eq!(
            e,
            format!("UNIQUE constraint failed: {}", column),
            "{}",
            sql
        ),
    }
}

/// Deterministic xorshift64* so the random workloads need no dependencies.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn unique_column_passes_integrity_check_at_1_500_and_5000_rows() {
    for n in [1usize, 500, 5000] {
        let path = tmp_db(&format!("unique_{}", n));
        let mut db = Database::open(&path).unwrap();
        exec(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE, v INTEGER);",
        );
        for i in 0..n {
            exec(
                &mut db,
                &format!(
                    "INSERT INTO t (email, v) VALUES ('user{}@x.com', {});",
                    i, i
                ),
            );
        }
        db.close().unwrap();

        assert_integrity_ok(&path);
        if let Some(out) = sqlite3(&path, "SELECT count(*) FROM t;") {
            assert_eq!(out, n.to_string());
        }
        cleanup(&path);
    }
}

#[test]
fn lookups_by_unique_value_use_the_autoindex() {
    let path = tmp_db("lookup");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE, v INTEGER);",
    );
    exec(&mut db, "BEGIN;");
    for i in 0..5000 {
        exec(
            &mut db,
            &format!(
                "INSERT INTO t (email, v) VALUES ('user{}@x.com', {});",
                i,
                i * 2
            ),
        );
    }
    exec(&mut db, "COMMIT;");
    db.close().unwrap();

    assert_integrity_ok(&path);
    let q = "SELECT * FROM t WHERE email = 'user4242@x.com';";
    if let Some(out) = sqlite3(&path, &format!("SELECT count(*) FROM t; {}", q)) {
        assert_eq!(out, "5000\n4243|user4242@x.com|8484");
    }
    if let Some(plan) = sqlite3(&path, &format!("EXPLAIN QUERY PLAN {}", q)) {
        assert!(
            plan.contains("USING INDEX sqlite_autoindex_t_1 (email=?)"),
            "plan: {}",
            plan
        );
    }
    cleanup(&path);
}

#[test]
fn text_primary_key_gets_an_index_and_rejects_duplicates() {
    let path = tmp_db("text_pk");
    let mut db = Database::open(&path).unwrap();
    exec(&mut db, "CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER);");
    for i in 0..3000 {
        exec(
            &mut db,
            &format!(
                "INSERT INTO kv VALUES ('key-{}', {});",
                (i * 7919) % 100003,
                i
            ),
        );
    }
    assert_unique_error(&mut db, "INSERT INTO kv VALUES ('key-0', 1);", "kv.k");
    assert_unique_error(&mut db, "UPDATE kv SET k = 'key-0' WHERE v = 5;", "kv.k");
    db.close().unwrap();

    assert_integrity_ok(&path);
    if let Some(plan) = sqlite3(
        &path,
        "EXPLAIN QUERY PLAN SELECT v FROM kv WHERE k = 'key-7919';",
    ) {
        assert!(plan.contains("sqlite_autoindex_kv_1"), "plan: {}", plan);
    }
    if let Some(out) = sqlite3(&path, "SELECT v FROM kv WHERE k = 'key-7919';") {
        assert_eq!(out, "1");
    }
    cleanup(&path);
}

#[test]
fn two_unique_columns_each_get_an_index() {
    let path = tmp_db("two_unique");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT UNIQUE, b INTEGER UNIQUE, c TEXT);",
    );
    for i in 0..2000 {
        exec(
            &mut db,
            &format!(
                "INSERT INTO t (a, b, c) VALUES ('a{}', {}, 'c{}');",
                i,
                2000 - i,
                i % 10
            ),
        );
    }
    assert_unique_error(
        &mut db,
        "INSERT INTO t (a, b, c) VALUES ('a1', -1, 'x');",
        "t.a",
    );
    assert_unique_error(
        &mut db,
        "INSERT INTO t (a, b, c) VALUES ('new', 2000, 'x');",
        "t.b",
    );
    db.close().unwrap();

    assert_integrity_ok(&path);
    if let Some(out) = sqlite3(
        &path,
        "SELECT name FROM pragma_index_list('t') ORDER BY name;",
    ) {
        assert_eq!(out, "sqlite_autoindex_t_1\nsqlite_autoindex_t_2");
    }
    if let Some(plan) = sqlite3(&path, "EXPLAIN QUERY PLAN SELECT * FROM t WHERE b = 7;") {
        assert!(
            plan.contains("sqlite_autoindex_t_2 (b=?)"),
            "plan: {}",
            plan
        );
    }
    cleanup(&path);
}

#[test]
fn schema_rows_match_what_sqlite_creates() {
    let tables = [
        "CREATE TABLE t1 (a TEXT UNIQUE, b TEXT PRIMARY KEY, c INTEGER UNIQUE);",
        "CREATE TABLE t2 (id INTEGER PRIMARY KEY UNIQUE, x TEXT);",
        "CREATE TABLE t3 (k TEXT UNIQUE PRIMARY KEY, y TEXT UNIQUE);",
        "CREATE TABLE t4 (id INT PRIMARY KEY, z TEXT);",
        "CREATE TABLE t5 (id integer primary key, w TEXT);",
        "CREATE TABLE t6 (id INTEGER(10) PRIMARY KEY, w TEXT);",
    ];
    let path = tmp_db("schema_ours");
    let mut db = Database::open(&path).unwrap();
    for sql in tables {
        exec(&mut db, sql);
    }
    db.close().unwrap();
    assert_integrity_ok(&path);

    let reference = tmp_db("schema_reference");
    let q = "SELECT type, name, tbl_name, quote(sql) FROM sqlite_schema ORDER BY rowid;";
    if sqlite3(&reference, &tables.concat()).is_some() {
        let ours = sqlite3(&path, q).unwrap();
        let theirs = sqlite3(&reference, q).unwrap();
        // The stored CREATE TABLE text differs only by the trailing ';' we keep.
        assert_eq!(ours.replace(";'", "'"), theirs);
        // Each index covers the same column as SQLite's.
        let cols = "SELECT il.name, ii.name FROM sqlite_schema s, pragma_index_list(s.name) il, \
                    pragma_index_info(il.name) ii WHERE s.type = 'table' ORDER BY 1;";
        assert_eq!(sqlite3(&path, cols), sqlite3(&reference, cols));
    }
    cleanup(&path);
    cleanup(&reference);
}

#[test]
fn duplicate_insert_and_update_are_rejected_atomically() {
    let path = tmp_db("duplicates");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE);",
    );
    exec(&mut db, "INSERT INTO t (email) VALUES ('a');");
    exec(&mut db, "INSERT INTO t (email) VALUES ('b');");
    // NULLs are distinct.
    exec(&mut db, "INSERT INTO t (email) VALUES (NULL);");
    exec(&mut db, "INSERT INTO t (email) VALUES (NULL);");

    assert_unique_error(&mut db, "INSERT INTO t (email) VALUES ('a');", "t.email");
    assert_unique_error(
        &mut db,
        "UPDATE t SET email = 'a' WHERE email = 'b';",
        "t.email",
    );
    // The second row of a multi-row statement conflicts: nothing is written.
    assert_unique_error(
        &mut db,
        "INSERT INTO t (email) VALUES ('c'), ('a');",
        "t.email",
    );
    assert_unique_error(&mut db, "UPDATE t SET email = 'same';", "t.email");
    // Setting a row to its own value is fine.
    assert_eq!(
        exec(&mut db, "UPDATE t SET email = 'a' WHERE email = 'a';"),
        1
    );

    let rows = db.execute("SELECT email FROM t;").unwrap().rows;
    let emails: Vec<Value> = rows.into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        emails,
        vec![
            Value::Text("a".into()),
            Value::Text("b".into()),
            Value::Null,
            Value::Null
        ]
    );

    // A non-INTEGER primary key is enforced through its index too.
    exec(&mut db, "CREATE TABLE p (id INT PRIMARY KEY, v TEXT);");
    exec(&mut db, "INSERT INTO p VALUES (10, 'x');");
    assert_unique_error(&mut db, "INSERT INTO p VALUES (10, 'y');", "p.id");
    db.close().unwrap();
    assert_integrity_ok(&path);
    cleanup(&path);
}

#[test]
fn random_updates_and_deletes_keep_the_index_valid() {
    let path = tmp_db("churn");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE, score INTEGER UNIQUE, note TEXT);",
    );
    let n = 5000;
    exec(&mut db, "BEGIN;");
    for i in 0..n {
        exec(
            &mut db,
            &format!(
                "INSERT INTO t (email, score, note) VALUES ('user{}@example.com', {}, 'n{}');",
                i,
                i * 3,
                i % 97
            ),
        );
    }
    exec(&mut db, "COMMIT;");

    let mut rng = Rng(0xC0FFEE);
    let mut live: Vec<usize> = (1..=n).collect();
    let mut rejected = 0;
    for j in 0..1000 {
        let id = live[rng.below(live.len())];
        let sql = match rng.below(4) {
            // Longer values: rows grow, table and index pages split.
            0 => format!(
                "UPDATE t SET email = 'renamed{}_{}{}@example.org' WHERE id = {};",
                j,
                id,
                "z".repeat(rng.below(30)),
                id
            ),
            1 => format!("UPDATE t SET score = {} WHERE id = {};", 1_000_000 + j, id),
            2 => format!("UPDATE t SET email = NULL WHERE id = {};", id),
            // Usually another row's score: must be rejected.
            _ => format!(
                "UPDATE t SET score = {} WHERE id = {};",
                rng.below(n) * 3,
                id
            ),
        };
        if let Err(e) = db.execute(&sql) {
            assert!(
                e.starts_with("UNIQUE constraint failed: t."),
                "{}: {}",
                sql,
                e
            );
            rejected += 1;
        }
    }
    assert!(
        rejected > 100,
        "expected many rejected updates, got {}",
        rejected
    );
    for _ in 0..1000 {
        let id = live.swap_remove(rng.below(live.len()));
        assert_eq!(
            exec(&mut db, &format!("DELETE FROM t WHERE id = {};", id)),
            1
        );
    }

    let ours = db.execute("SELECT * FROM t;").unwrap().rows;
    assert_eq!(ours.len(), n - 1000);
    db.close().unwrap();

    assert_integrity_ok(&path);
    if let Some(out) = sqlite3(&path, "SELECT * FROM t ORDER BY id;") {
        let ours: Vec<String> = ours
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| {
                        if *v == Value::Null {
                            String::new()
                        } else {
                            v.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        assert_eq!(out, ours.join("\n"), "sqlite3 and the engine disagree");
    }
    cleanup(&path);
}

#[test]
fn heavy_deletes_merge_index_pages_and_reuse_them() {
    let path = tmp_db("merge");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE);",
    );
    let n = 3000;
    exec(&mut db, "BEGIN;");
    for i in 0..n {
        exec(
            &mut db,
            &format!(
                "INSERT INTO t (u) VALUES ('{:05}{}');",
                (i * 37) % n,
                "p".repeat(i % 60)
            ),
        );
    }
    exec(&mut db, "COMMIT;");
    // Delete 70% at random: index leaves run low and merge, pages go to the freelist.
    let mut rng = Rng(7);
    let mut ids: Vec<usize> = (1..=n).collect();
    for i in (1..ids.len()).rev() {
        ids.swap(i, rng.below(i + 1));
    }
    for id in &ids[..n * 7 / 10] {
        exec(&mut db, &format!("DELETE FROM t WHERE id = {};", id));
    }
    db.close().unwrap();
    assert_integrity_ok(&path);
    let freed = sqlite3(&path, "PRAGMA freelist_count;");
    if let Some(freed) = &freed {
        assert!(freed.parse::<u32>().unwrap() > 0, "expected freed pages");
    }

    // Grow again: freed pages are reused.
    let mut db = Database::open(&path).unwrap();
    for i in 0..n / 2 {
        exec(
            &mut db,
            &format!("INSERT INTO t (u) VALUES ('again{}');", i),
        );
    }
    db.close().unwrap();
    assert_integrity_ok(&path);
    if let (Some(before), Some(after)) = (freed, sqlite3(&path, "PRAGMA freelist_count;")) {
        assert!(after.parse::<u32>().unwrap() < before.parse::<u32>().unwrap());
    }
    if let Some(out) = sqlite3(&path, "SELECT count(*) FROM t;") {
        assert_eq!(out, (n - n * 7 / 10 + n / 2).to_string());
    }
    cleanup(&path);
}

#[test]
fn indexes_survive_transactions_and_reopen() {
    let path = tmp_db("txn_reopen");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE);",
    );
    exec(&mut db, "BEGIN;");
    exec(&mut db, "INSERT INTO t (u) VALUES ('in-txn');");
    // A duplicate of a row written earlier in the same transaction.
    assert_unique_error(&mut db, "INSERT INTO t (u) VALUES ('in-txn');", "t.u");
    assert_eq!(
        exec(&mut db, "UPDATE t SET u = 'renamed' WHERE u = 'in-txn';"),
        1
    );
    exec(&mut db, "COMMIT;");

    exec(&mut db, "BEGIN;");
    exec(&mut db, "INSERT INTO t (u) VALUES ('rolled-back');");
    exec(&mut db, "ROLLBACK;");
    exec(&mut db, "INSERT INTO t (u) VALUES ('rolled-back');");
    for i in 0..1000 {
        exec(&mut db, &format!("INSERT INTO t (u) VALUES ('{}');", i));
    }
    db.close().unwrap();

    // Reopen: the index is found again and still enforced.
    let mut db = Database::open(&path).unwrap();
    assert_unique_error(&mut db, "INSERT INTO t (u) VALUES ('500');", "t.u");
    let mut rng = Rng(99);
    for _ in 0..400 {
        let i = rng.below(1000);
        exec(&mut db, &format!("DELETE FROM t WHERE u = '{}';", i));
    }
    // Drop without close: the next open must recover from the WAL.
    drop(db);
    let mut db = Database::open(&path).unwrap();
    for i in 0..300 {
        exec(
            &mut db,
            &format!("INSERT INTO t (u) VALUES ('again{}');", i),
        );
    }
    db.close().unwrap();
    assert_integrity_ok(&path);
    cleanup(&path);
}

#[test]
fn drop_table_removes_index_rows_and_frees_pages() {
    let path = tmp_db("drop");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT UNIQUE);",
    );
    exec(
        &mut db,
        "CREATE TABLE b (id INTEGER PRIMARY KEY, name TEXT);",
    );
    exec(&mut db, "BEGIN;");
    for i in 0..2000 {
        exec(&mut db, &format!("INSERT INTO a (name) VALUES ('n{}');", i));
    }
    exec(&mut db, "COMMIT;");
    exec(&mut db, "DROP TABLE a;");
    for i in 0..500 {
        exec(&mut db, &format!("INSERT INTO b (name) VALUES ('b{}');", i));
    }
    // Recreating the table reuses the autoindex name.
    exec(
        &mut db,
        "CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT UNIQUE);",
    );
    exec(&mut db, "INSERT INTO a (name) VALUES ('fresh');");
    db.close().unwrap();

    assert_integrity_ok(&path);
    if let Some(out) = sqlite3(
        &path,
        "SELECT type, name FROM sqlite_schema ORDER BY rowid;",
    ) {
        assert_eq!(out, "table|b\ntable|a\nindex|sqlite_autoindex_a_1");
    }
    cleanup(&path);
}

#[test]
fn table_without_unique_columns_is_unchanged() {
    let path = tmp_db("plain");
    let mut db = Database::open(&path).unwrap();
    exec(
        &mut db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v INTEGER);",
    );
    for i in 0..5000 {
        exec(
            &mut db,
            &format!(
                "INSERT INTO t (name, v) VALUES ('name_{:06}', {});",
                i,
                i * 7
            ),
        );
    }
    db.close().unwrap();

    assert_integrity_ok(&path);
    if let Some(out) = sqlite3(
        &path,
        "SELECT count(*) FROM t; SELECT count(*) FROM sqlite_schema;",
    ) {
        assert_eq!(out, "5000\n1");
    }
    cleanup(&path);
}
