use rust_sqllite::{Database, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn tmp_db(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rustsqlite_integ_{}", name));
    p
}

fn cleanup(db: &Path) {
    let _ = fs::remove_file(db);
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    let _ = fs::remove_file(&wal);
}

// ───────────── Basic CRUD ─────────────

#[test]
fn test_create_and_insert() {
    let db_path = tmp_db("crud_create");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE users (id INTEGER, name TEXT, age INTEGER);")
        .unwrap();
    let r = db
        .execute("INSERT INTO users VALUES (1, 'Alice', 30);")
        .unwrap();
    assert_eq!(r.rows_affected, 1);

    let r2 = db.execute("SELECT * FROM users;").unwrap();
    assert_eq!(r2.rows.len(), 1);
    assert_eq!(r2.columns.len(), 3);
    assert_eq!(r2.rows[0][0], Value::Integer(1));
    assert_eq!(r2.rows[0][1], Value::Text("Alice".to_string()));
    assert_eq!(r2.rows[0][2], Value::Integer(30));

    db.close().unwrap();
    cleanup(&db_path);
}

#[test]
fn test_multi_row_insert() {
    let db_path = tmp_db("crud_multi");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE items (id INTEGER, name TEXT);")
        .unwrap();
    let r = db
        .execute("INSERT INTO items VALUES (1, 'a'), (2, 'b'), (3, 'c');")
        .unwrap();
    assert_eq!(r.rows_affected, 3);

    let r2 = db.execute("SELECT * FROM items;").unwrap();
    assert_eq!(r2.rows.len(), 3);

    db.close().unwrap();
    cleanup(&db_path);
}

#[test]
fn test_select_where() {
    let db_path = tmp_db("crud_where");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (x INTEGER, y TEXT);").unwrap();
    db.execute("INSERT INTO t VALUES (10, 'ten');").unwrap();
    db.execute("INSERT INTO t VALUES (20, 'twenty');").unwrap();
    db.execute("INSERT INTO t VALUES (30, 'thirty');").unwrap();

    // Greater-than.
    let r = db.execute("SELECT * FROM t WHERE x > 15;").unwrap();
    assert_eq!(r.rows.len(), 2);

    // Equality.
    let r2 = db.execute("SELECT y FROM t WHERE x = 20;").unwrap();
    assert_eq!(r2.rows.len(), 1);
    assert_eq!(r2.rows[0][0], Value::Text("twenty".to_string()));

    // Less-than.
    let r3 = db.execute("SELECT * FROM t WHERE x < 20;").unwrap();
    assert_eq!(r3.rows.len(), 1);
    assert_eq!(r3.rows[0][0], Value::Integer(10));

    db.close().unwrap();
    cleanup(&db_path);
}

#[test]
fn test_select_where_and() {
    let db_path = tmp_db("crud_where_and");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (a INTEGER, b INTEGER);")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10);").unwrap();
    db.execute("INSERT INTO t VALUES (2, 20);").unwrap();
    db.execute("INSERT INTO t VALUES (3, 30);").unwrap();

    let r = db
        .execute("SELECT * FROM t WHERE a >= 2 AND b <= 20;")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Integer(2));

    db.close().unwrap();
    cleanup(&db_path);
}

#[test]
fn test_update() {
    let db_path = tmp_db("crud_update");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER, val TEXT);")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'old');").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'keep');").unwrap();

    let r = db
        .execute("UPDATE t SET val = 'new' WHERE id = 1;")
        .unwrap();
    assert_eq!(r.rows_affected, 1);

    let r2 = db.execute("SELECT val FROM t WHERE id = 1;").unwrap();
    assert_eq!(r2.rows[0][0], Value::Text("new".to_string()));

    // Untouched row.
    let r3 = db.execute("SELECT val FROM t WHERE id = 2;").unwrap();
    assert_eq!(r3.rows[0][0], Value::Text("keep".to_string()));

    db.close().unwrap();
    cleanup(&db_path);
}

#[test]
fn test_delete() {
    let db_path = tmp_db("crud_delete");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER, val TEXT);")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a');").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'b');").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'c');").unwrap();

    let r = db.execute("DELETE FROM t WHERE id = 2;").unwrap();
    assert_eq!(r.rows_affected, 1);

    let r2 = db.execute("SELECT * FROM t;").unwrap();
    assert_eq!(r2.rows.len(), 2);

    // Delete all remaining.
    let r3 = db.execute("DELETE FROM t;").unwrap();
    assert_eq!(r3.rows_affected, 2);

    let r4 = db.execute("SELECT * FROM t;").unwrap();
    assert_eq!(r4.rows.len(), 0);

    db.close().unwrap();
    cleanup(&db_path);
}

// ───────────── Multiple Tables ─────────────

#[test]
fn test_multiple_tables() {
    let db_path = tmp_db("multi_tables");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE users (id INTEGER, name TEXT);")
        .unwrap();
    db.execute("CREATE TABLE products (id INTEGER, title TEXT, price INTEGER);")
        .unwrap();

    db.execute("INSERT INTO users VALUES (1, 'Alice');")
        .unwrap();
    db.execute("INSERT INTO users VALUES (2, 'Bob');").unwrap();
    db.execute("INSERT INTO products VALUES (1, 'Widget', 999);")
        .unwrap();

    let ru = db.execute("SELECT * FROM users;").unwrap();
    let rp = db.execute("SELECT * FROM products;").unwrap();
    assert_eq!(ru.rows.len(), 2);
    assert_eq!(rp.rows.len(), 1);

    db.close().unwrap();
    cleanup(&db_path);
}

// ───────────── Transactions ─────────────

#[test]
fn test_transaction_commit() {
    let db_path = tmp_db("txn_commit");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER);").unwrap();

    db.execute("BEGIN;").unwrap();
    db.execute("INSERT INTO t VALUES (1);").unwrap();
    db.execute("INSERT INTO t VALUES (2);").unwrap();
    db.execute("COMMIT;").unwrap();

    let r = db.execute("SELECT * FROM t;").unwrap();
    assert_eq!(r.rows.len(), 2);

    db.close().unwrap();
    cleanup(&db_path);
}

#[test]
fn test_transaction_rollback() {
    let db_path = tmp_db("txn_rollback");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER);").unwrap();

    // Insert one row outside transaction.
    db.execute("INSERT INTO t VALUES (0);").unwrap();

    // Start transaction, insert, then rollback.
    db.execute("BEGIN;").unwrap();
    db.execute("INSERT INTO t VALUES (1);").unwrap();
    db.execute("INSERT INTO t VALUES (2);").unwrap();
    db.execute("ROLLBACK;").unwrap();

    // Only the pre-transaction row should remain.
    let r = db.execute("SELECT * FROM t;").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Integer(0));

    db.close().unwrap();
    cleanup(&db_path);
}

// ───────────── Persistence ─────────────

#[test]
fn test_persistence() {
    let db_path = tmp_db("persist");
    cleanup(&db_path);

    // Write data and close.
    {
        let mut db = Database::open(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER, msg TEXT);")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'hello');").unwrap();
        db.execute("INSERT INTO t VALUES (2, 'world');").unwrap();
        db.close().unwrap();
    }

    // Reopen and verify data.
    {
        let mut db = Database::open(&db_path).unwrap();
        let r = db.execute("SELECT * FROM t;").unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][1], Value::Text("hello".to_string()));
        assert_eq!(r.rows[1][1], Value::Text("world".to_string()));
        db.close().unwrap();
    }

    cleanup(&db_path);
}

// ───────────── WAL Checkpoint ─────────────

#[test]
fn test_wal_checkpoint_integration() {
    let db_path = tmp_db("wal_cp_integ");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (v INTEGER);").unwrap();
    db.execute("INSERT INTO t VALUES (42);").unwrap();

    // Force checkpoint.
    db.checkpoint().unwrap();

    // Verify data survives.
    let r = db.execute("SELECT * FROM t;").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Integer(42));

    db.close().unwrap();
    cleanup(&db_path);
}

// ───────────── Column-specified INSERT ─────────────

#[test]
fn test_insert_with_columns() {
    let db_path = tmp_db("insert_cols");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER, name TEXT, score INTEGER);")
        .unwrap();
    db.execute("INSERT INTO t (name, id) VALUES ('Alice', 1);")
        .unwrap();

    let r = db.execute("SELECT * FROM t;").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Integer(1));
    assert_eq!(r.rows[0][1], Value::Text("Alice".to_string()));
    assert_eq!(r.rows[0][2], Value::Null);

    db.close().unwrap();
    cleanup(&db_path);
}

// ───────────── Drop Table ─────────────

#[test]
fn test_drop_table() {
    let db_path = tmp_db("drop_tbl");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE temp (x INTEGER);").unwrap();
    db.execute("INSERT INTO temp VALUES (1);").unwrap();

    db.execute("DROP TABLE temp;").unwrap();

    // Accessing dropped table should error.
    let err = db.execute("SELECT * FROM temp;");
    assert!(err.is_err());

    db.close().unwrap();
    cleanup(&db_path);
}

// ───────────── Large dataset (B-Tree splits) ─────────────

#[test]
fn test_large_insert() {
    let db_path = tmp_db("large_insert");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE big (id INTEGER, data TEXT);")
        .unwrap();

    for i in 0..100 {
        db.execute(&format!(
            "INSERT INTO big VALUES ({}, 'row_data_{}');",
            i, i
        ))
        .unwrap();
    }

    let r = db.execute("SELECT * FROM big;").unwrap();
    assert_eq!(r.rows.len(), 100);

    // Spot-check.
    let r2 = db.execute("SELECT data FROM big WHERE id = 50;").unwrap();
    assert_eq!(r2.rows.len(), 1);
    assert_eq!(r2.rows[0][0], Value::Text("row_data_50".to_string()));

    db.close().unwrap();
    cleanup(&db_path);
}

// ───────────── String with special chars ─────────────

#[test]
fn test_string_with_quotes() {
    let db_path = tmp_db("str_quotes");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (msg TEXT);").unwrap();
    db.execute("INSERT INTO t VALUES ('it''s a test');")
        .unwrap();

    let r = db.execute("SELECT * FROM t;").unwrap();
    assert_eq!(r.rows[0][0], Value::Text("it's a test".to_string()));

    db.close().unwrap();
    cleanup(&db_path);
}

// ───────────── Real / Float values ─────────────

#[test]
fn test_real_values() {
    let db_path = tmp_db("real_vals");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER, val REAL);")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 3.14);").unwrap();
    db.execute("INSERT INTO t VALUES (2, 2.71);").unwrap();

    let r = db.execute("SELECT * FROM t WHERE val > 3.0;").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Integer(1));

    db.close().unwrap();
    cleanup(&db_path);
}

// ───────────── NULL handling ─────────────

#[test]
fn test_null_values() {
    let db_path = tmp_db("null_vals");
    cleanup(&db_path);

    let mut db = Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER, name TEXT);")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, NULL);").unwrap();

    let r = db.execute("SELECT * FROM t;").unwrap();
    assert_eq!(r.rows[0][1], Value::Null);

    db.close().unwrap();
    cleanup(&db_path);
}
