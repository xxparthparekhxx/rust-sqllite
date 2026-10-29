//! # RustSQLite — A SQLite implementation in pure Rust
//!
//! This library implements a basic SQLite-compatible database engine from scratch,
//! using only the Rust standard library. Features include:
//!
//! - **Page-based storage** with 4KB pages and LRU cache
//! - **Write-Ahead Logging (WAL)** for crash safety and atomic commits
//! - **B+Tree** for table data storage
//! - **SQL parser** (recursive descent) supporting:
//!   - CREATE TABLE / DROP TABLE
//!   - INSERT (single and multi-row)
//!   - SELECT with WHERE (AND/OR, comparisons)
//!   - UPDATE with WHERE
//!   - DELETE with WHERE
//!   - BEGIN / COMMIT / ROLLBACK transactions
//! - **Catalog** stored in a B-Tree on page 1
//!
//! ## Example
//!
//! ```no_run
//! use rust_sqllite::Database;
//!
//! let mut db = Database::open("my.db").unwrap();
//! db.execute("CREATE TABLE users (id INTEGER, name TEXT);").unwrap();
//! db.execute("INSERT INTO users VALUES (1, 'Alice');").unwrap();
//! let result = db.execute("SELECT * FROM users;").unwrap();
//! for row in &result.rows {
//!     println!("{:?}", row);
//! }
//! db.close().unwrap();
//! ```

pub mod btree;
pub mod engine;
pub mod index;
pub mod pager;
pub mod schema;
pub mod sql;
pub mod varint;
pub mod wal;

use std::io;
use std::path::Path;

pub use engine::QueryResult;
pub use schema::Value;

/// The main database handle.
pub struct Database {
    engine: engine::Engine,
}

impl Database {
    /// Open a database file (creates it if it doesn't exist).
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let engine = engine::Engine::open(path)?;
        Ok(Database { engine })
    }

    /// Execute a SQL statement. Returns a `QueryResult`.
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, String> {
        self.engine.execute(sql)
    }

    /// Force a WAL checkpoint (flush WAL frames to the main database file).
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.engine.force_checkpoint()
    }

    /// List all table names in the database.
    pub fn list_tables(&self) -> Vec<String> {
        self.engine.list_tables()
    }

    /// Close the database gracefully (checkpoints the WAL).
    pub fn close(self) -> io::Result<()> {
        self.engine.close()
    }
}
