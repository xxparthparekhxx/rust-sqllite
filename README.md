# 🗄️ RustSQLite

A SQLite database engine implemented from scratch in pure Rust — **zero dependencies**, just the standard library.

Database files are fully compatible with the official SQLite 3 format and can be opened by `sqlite3`, DB Browser for SQLite, or any SQLite-compatible tool.

## ✨ Features

- **SQLite 3 File Format** — Standard page-based storage with 100-byte file header, B-Tree pages, and varint encoding
- **Record Format** — Rows serialized using SQLite's standard serial type system (big-endian, variable-width integers)
- **`sqlite_schema` Table** — Standard system catalog on Page 1 for tool compatibility
- **B-Tree Engine** — Table B-Trees with `u64` rowids, cell pointer arrays, and page splitting; on `DELETE`, a page that runs low is merged into or refilled from a sibling, so no empty page is left behind, and the root page never moves
- **Automatic Indexes** — Every `UNIQUE` column, and every `PRIMARY KEY` that is not `INTEGER PRIMARY KEY`, gets the `sqlite_autoindex_<table>_<n>` index SQLite expects: an index B-Tree kept up to date on `INSERT`, `UPDATE` and `DELETE`, which `sqlite3` uses for lookups
- **Freelist** — Pages released by table and index merges and by `DROP TABLE` go on SQLite's freelist and are reused
- **Write-Ahead Logging (WAL)** — Crash-safe atomic commits with checkpoint support
- **4KB Paged Storage** — LRU-cached page I/O with proper 1-based page indexing
- **SQL Parser** — Hand-written recursive descent parser supporting:
  - `CREATE TABLE` / `DROP TABLE`
  - `INSERT` (single and multi-row)
  - `SELECT` with `WHERE` (AND/OR, comparisons)
  - `UPDATE` with `WHERE`
  - `DELETE` with `WHERE`
  - `BEGIN` / `COMMIT` / `ROLLBACK` transactions
- **Column Constraints** — `PRIMARY KEY`, `AUTOINCREMENT`, `NOT NULL`, `UNIQUE`, `DEFAULT` (`NOT NULL`, `UNIQUE` and `PRIMARY KEY` are enforced on both `INSERT` and `UPDATE`, and a statement that breaks one changes nothing: an `INTEGER PRIMARY KEY` as the rowid, so a duplicate id is rejected rather than overwriting the row, and `UNIQUE` and non-integer `PRIMARY KEY` through their automatic index)
- **Data Types** — `INTEGER`, `TEXT`, `REAL`, `BLOB`, `NULL`
- **Interactive REPL** — Pretty-printed table output with box-drawing characters

## 🚀 Quick Start

```bash
# Build and run
cargo run

# Or specify a database file
cargo run -- mydb.db
```

```sql
rustsqlite> CREATE TABLE users (
       ...>     id INTEGER PRIMARY KEY AUTOINCREMENT,
       ...>     name TEXT NOT NULL,
       ...>     email TEXT
       ...> );
OK
rustsqlite> INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com');
OK (1 row affected)
rustsqlite> SELECT * FROM users;
 id │ name  │ email
────┼───────┼──────────────────
 1  │ Alice │ alice@example.com
(1 row)
rustsqlite> .quit
Bye!
```

Then verify with the official SQLite CLI:

```bash
$ sqlite3 rustsqlite.db "SELECT * FROM users;"
1|Alice|alice@example.com
```

## 📂 Project Structure

```
src/
├── lib.rs       # Public API (Database handle)
├── main.rs      # Interactive REPL with dot-commands
├── sql.rs       # Tokenizer + recursive descent SQL parser
├── engine.rs    # Query execution engine & catalog management
├── btree.rs     # Table B-Tree implementation (search, insert, delete, scan)
├── index.rs     # Index B-Tree for automatic indexes (insert, delete, split, merge)
├── pager.rs     # Page-based I/O with LRU cache & file header
├── wal.rs       # Write-Ahead Logging for crash safety
├── schema.rs    # SQLite Record Format serialization & table definitions
└── varint.rs    # SQLite variable-length integer encoding
```

## 🔧 REPL Commands

| Command | Description |
|---------|------------|
| `.help` | Show available commands |
| `.tables` | List all tables |
| `.schema` | Show CREATE statements |
| `.checkpoint` | Force WAL checkpoint |
| `.demo` | Run built-in demo with sample data |
| `.quit` | Exit (checkpoints WAL automatically) |

## 📚 Library Usage

```rust
use rust_sqllite::Database;

let mut db = Database::open("my.db").unwrap();
db.execute("CREATE TABLE users (id INTEGER, name TEXT);").unwrap();
db.execute("INSERT INTO users VALUES (1, 'Alice');").unwrap();

let result = db.execute("SELECT * FROM users;").unwrap();
for row in &result.rows {
    println!("{:?}", row);
}

db.close().unwrap();
```

## 🧪 Tests

```bash
cargo test
```

Includes 48 unit tests, 37 integration tests, and doc tests covering the B-Trees, pager, WAL, SQL parser, record format, and full engine operations. The tests in `tests/autoindex.rs`, `tests/table_delete.rs` and `tests/constraints.rs` run the `sqlite3` CLI (when installed) to check `PRAGMA integrity_check`, query plans and query results on files this engine wrote.

## 📋 SQLite Compatibility

| Feature | Status |
|---------|--------|
| File header (100 bytes) | ✅ |
| B-Tree page types (0x0D, 0x05, 0x0A, 0x02) | ✅ |
| Varint encoding | ✅ |
| Record format (serial types) | ✅ |
| `sqlite_schema` system table | ✅ |
| `PRAGMA integrity_check` | ✅ Passes |
| DB Browser / sqlite3 CLI | ✅ Works |
| Freelist (free and reuse pages) | ✅ |
| Automatic indexes (`UNIQUE`, non-integer `PRIMARY KEY`) | ✅ Keys up to 1002 bytes |
| Overflow pages | ❌ Not yet |
| `CREATE INDEX` / multi-column indexes | ❌ Not yet |
| `JOIN` / subqueries | ❌ Not yet |
| WAL mode (-wal/-shm) | ⚠️ Custom format |

## License

MIT
