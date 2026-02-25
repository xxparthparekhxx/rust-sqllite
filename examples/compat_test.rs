use rust_sqllite::Database;

fn main() {
    // Clean up
    let _ = std::fs::remove_file("test_compat.db");
    let _ = std::fs::remove_file("test_compat.db-wal");

    let mut db = Database::open("test_compat.db").unwrap();
    db.execute("CREATE TABLE users (id INTEGER, name TEXT, age INTEGER);")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'Alice', 28);")
        .unwrap();
    db.execute("INSERT INTO users VALUES (2, 'Bob', 35);")
        .unwrap();
    db.execute("INSERT INTO users VALUES (3, 'Charlie', 22);")
        .unwrap();

    // Verify our engine can read it back
    let result = db.execute("SELECT * FROM users;").unwrap();
    println!("Our engine reads {} rows:", result.rows.len());
    for row in &result.rows {
        println!("  {:?}", row);
    }

    // Checkpoint to flush WAL to main DB
    db.checkpoint().unwrap();
    db.close().unwrap();

    println!("\nDatabase written to test_compat.db");
    println!("Try: sqlite3 test_compat.db \".tables\"");
    println!("Try: sqlite3 test_compat.db \"SELECT * FROM users;\"");
}
