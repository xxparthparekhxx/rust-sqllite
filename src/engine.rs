use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::btree::{self, PageStore, init_leaf_page};
use crate::pager::{PAGE_SIZE, Pager};
use crate::schema::{Column, Row, Table, Value, deserialize_row, serialize_row};
use crate::sql::{BinOp, ColumnDef, Expr, Parser, SelectColumn, Statement};
use crate::wal::Wal;

use std::time::{SystemTime, UNIX_EPOCH};

/// Returns a basic ISO-8601ish current timestamp string (UTC) using only std::time.
fn current_timestamp() -> String {
    let now = SystemTime::now();
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = duration.as_secs();

    // Very basic math to get YYYY-MM-DD HH:MM:SS from unix epoch
    // (ignores leap seconds, but standard library only gives this)
    let days = secs / 86400;
    let mut remainder = secs % 86400;

    let hours = remainder / 3600;
    remainder %= 3600;
    let minutes = remainder / 60;
    let seconds = remainder % 60;

    // Calculate year, month, day roughly
    let mut year = 1970;
    let mut days_left = days;
    loop {
        let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let days_in_year = if is_leap { 366 } else { 365 };
        if days_left >= days_in_year {
            days_left -= days_in_year;
            year += 1;
        } else {
            break;
        }
    }

    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let days_in_month = [
        31,
        if is_leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0;
    while days_left >= days_in_month[month] {
        days_left -= days_in_month[month];
        month += 1;
    }
    month += 1; // 1-12
    let day = days_left + 1; // 1-31

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, hours, minutes, seconds
    )
}

/// The catalog B-Tree lives on page 1 (page 0 is the file header).
const CATALOG_ROOT_PAGE: u32 = 1;

/// Result of executing a SQL statement.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// Column names (for SELECT).
    pub columns: Vec<String>,
    /// Row data (for SELECT).
    pub rows: Vec<Row>,
    /// Number of rows affected (for INSERT/UPDATE/DELETE).
    pub rows_affected: u64,
}

impl QueryResult {
    fn empty() -> Self {
        QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            rows_affected: 0,
        }
    }

    fn with_affected(n: u64) -> Self {
        QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            rows_affected: n,
        }
    }
}

/// Adapts Pager + WAL into the `PageStore` trait for the B-Tree.
struct WalPageStore<'a> {
    pager: &'a mut Pager,
    wal: &'a mut Wal,
    /// Pages written during the current transaction (buffered for WAL batch commit).
    dirty_pages: &'a mut Vec<(u32, [u8; PAGE_SIZE])>,
}

impl<'a> PageStore for WalPageStore<'a> {
    fn read_page(&mut self, page_num: u32) -> io::Result<[u8; PAGE_SIZE]> {
        // Check dirty pages first (current transaction writes).
        for (pn, data) in self.dirty_pages.iter().rev() {
            if *pn == page_num {
                return Ok(*data);
            }
        }
        // Then check WAL.
        if let Some(data) = self.wal.read_page(page_num)? {
            return Ok(data);
        }
        // Finally, the main DB file.
        self.pager.read_page(page_num)
    }

    fn write_page(&mut self, page_num: u32, data: [u8; PAGE_SIZE]) -> io::Result<()> {
        let mut writable = data;
        // Ensure page 1 always carries the file header so B-tree page_offset() works.
        if page_num == 1 || page_num == 0 {
            let hdr = self.pager.header.serialize();
            writable[..crate::pager::FileHeader::SIZE].copy_from_slice(&hdr);
        }
        self.dirty_pages.push((page_num, writable));
        Ok(())
    }

    fn allocate_page(&mut self) -> io::Result<u32> {
        // We need a new page number beyond all existing ones.
        let mut max_page = self.pager.header.page_count;
        for (pn, _) in self.dirty_pages.iter() {
            if *pn > max_page {
                max_page = *pn;
            }
        }
        Ok(max_page + 1)
    }
}

/// The core execution engine.
pub struct Engine {
    pager: Pager,
    wal: Wal,
    /// Cached table schemas (loaded from catalog).
    schema_cache: HashMap<String, Table>,
    /// Whether we are in an explicit transaction.
    in_transaction: bool,
    /// Dirty pages accumulated during an explicit transaction.
    txn_dirty_pages: Vec<(u32, [u8; PAGE_SIZE])>,
    /// Next available auto-increment rowid per table.
    next_rowid: HashMap<String, i64>,
    /// Next available rowid for the catalog btree.
    next_catalog_rowid: u64,
}

impl Engine {
    /// Open a database, setting up the catalog if needed.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut pager = Pager::open(&path)?;
        let wal = Wal::open(&path)?;

        // Ensure the catalog B-Tree root page exists (page 1).
        if pager.header.page_count < 2 {
            // Read existing page 1 (which has the SQLite magic header in first 100 bytes).
            let mut page = pager.read_page(CATALOG_ROOT_PAGE)?;
            init_leaf_page(&mut page);
            pager.write_page(CATALOG_ROOT_PAGE, page)?;
        }

        let mut engine = Engine {
            pager,
            wal,
            schema_cache: HashMap::new(),
            in_transaction: false,
            txn_dirty_pages: Vec::new(),
            next_rowid: HashMap::new(),
            next_catalog_rowid: 1,
        };

        // Load existing table schemas.
        engine.load_catalog()?;

        Ok(engine)
    }

    /// Execute a SQL string.
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, String> {
        let stmt = Parser::parse(sql)?;
        self.execute_statement(stmt, sql).map_err(|e| e.to_string())
    }

    /// Execute a parsed statement.
    fn execute_statement(
        &mut self,
        stmt: Statement,
        original_sql: &str,
    ) -> io::Result<QueryResult> {
        match stmt {
            Statement::CreateTable {
                table_name,
                columns,
            } => self.exec_create_table(&table_name, &columns, original_sql),
            Statement::DropTable { table_name } => self.exec_drop_table(&table_name),
            Statement::Insert {
                table_name,
                columns,
                values,
            } => self.exec_insert(&table_name, columns.as_deref(), &values),
            Statement::Select {
                columns,
                table_name,
                where_clause,
            } => self.exec_select(&columns, &table_name, where_clause.as_ref()),
            Statement::Update {
                table_name,
                assignments,
                where_clause,
            } => self.exec_update(&table_name, &assignments, where_clause.as_ref()),
            Statement::Delete {
                table_name,
                where_clause,
            } => self.exec_delete(&table_name, where_clause.as_ref()),
            Statement::Begin => {
                self.in_transaction = true;
                self.txn_dirty_pages.clear();
                Ok(QueryResult::empty())
            }
            Statement::Commit => self.exec_commit(),
            Statement::Rollback => self.exec_rollback(),
        }
    }

    // ─────────── Catalog management ───────────

    fn load_catalog(&mut self) -> io::Result<()> {
        self.schema_cache.clear();
        let mut dirty = Vec::new();
        let mut store = WalPageStore {
            pager: &mut self.pager,
            wal: &mut self.wal,
            dirty_pages: &mut dirty,
        };
        let entries = crate::btree::scan_all(&mut store, CATALOG_ROOT_PAGE)?;
        let mut max_catalog_rowid = 0u64;
        for (key, value) in entries {
            if key > max_catalog_rowid {
                max_catalog_rowid = key;
            }
            // Each entry is a sqlite_schema record: (type, name, tbl_name, rootpage, sql)
            let row = deserialize_row(&value)?;
            if row.len() < 5 {
                continue; // skip malformed entries
            }
            let entry_type = match &row[0] {
                Value::Text(s) => s.clone(),
                _ => continue,
            };
            if entry_type != "table" {
                continue; // only handle tables for now
            }
            let name = match &row[1] {
                Value::Text(s) => s.clone(),
                _ => continue,
            };
            let root_page = match &row[3] {
                Value::Integer(i) => *i as u32,
                _ => continue,
            };
            let sql_text = match &row[4] {
                Value::Text(s) => s.clone(),
                _ => continue,
            };

            // Re-parse the SQL to reconstruct column definitions.
            if let Ok(stmt) = Parser::parse(&sql_text) {
                if let Statement::CreateTable {
                    columns: col_defs, ..
                } = stmt
                {
                    let columns: Vec<Column> = col_defs
                        .iter()
                        .map(|cd| {
                            let default_text =
                                cd.default_value.as_ref().and_then(|expr| match expr {
                                    Expr::Literal(Value::Text(s)) => Some(s.clone()),
                                    Expr::Literal(Value::Integer(i)) => Some(i.to_string()),
                                    Expr::Literal(Value::Real(r)) => Some(r.to_string()),
                                    Expr::Literal(Value::Null) => Some("NULL".to_string()),
                                    Expr::ColumnRef(n) => Some(n.clone()),
                                    _ => None,
                                });
                            Column {
                                name: cd.name.clone(),
                                data_type: cd.data_type.clone(),
                                is_primary_key: cd.is_primary_key,
                                is_autoincrement: cd.is_autoincrement,
                                is_not_null: cd.is_not_null,
                                is_unique: cd.is_unique,
                                default_text,
                            }
                        })
                        .collect();
                    let table = Table {
                        name: name.clone(),
                        columns,
                        root_page,
                        sql: sql_text,
                    };
                    self.schema_cache.insert(name, table);
                }
            }
        }
        self.next_catalog_rowid = max_catalog_rowid + 1;
        Ok(())
    }

    fn save_table_to_catalog(&mut self, table: &Table) -> io::Result<()> {
        // First, delete any existing catalog entry for this table name.
        {
            let mut scan_dirty = Vec::new();
            let entries = {
                let mut store = WalPageStore {
                    pager: &mut self.pager,
                    wal: &mut self.wal,
                    dirty_pages: &mut scan_dirty,
                };
                crate::btree::scan_all(&mut store, CATALOG_ROOT_PAGE)?
            };
            for (k, v) in entries {
                if let Ok(row) = deserialize_row(&v) {
                    if row.len() >= 2 {
                        if let Value::Text(ref n) = row[1] {
                            if n == &table.name {
                                let mut del_dirty = if self.in_transaction {
                                    std::mem::take(&mut self.txn_dirty_pages)
                                } else {
                                    Vec::new()
                                };
                                {
                                    let mut store = WalPageStore {
                                        pager: &mut self.pager,
                                        wal: &mut self.wal,
                                        dirty_pages: &mut del_dirty,
                                    };
                                    crate::btree::delete(&mut store, CATALOG_ROOT_PAGE, k)?;
                                }
                                if self.in_transaction {
                                    self.txn_dirty_pages = del_dirty;
                                } else {
                                    self.commit_dirty_pages(del_dirty)?;
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Build the sqlite_schema row: (type, name, tbl_name, rootpage, sql)
        let schema_row = vec![
            Value::Text("table".to_string()),
            Value::Text(table.name.clone()),
            Value::Text(table.name.clone()),
            Value::Integer(table.root_page as i64),
            Value::Text(table.sql.clone()),
        ];
        let value = serialize_row(&schema_row);

        // Insert with a fresh key.
        let key = self.next_catalog_rowid;
        self.next_catalog_rowid += 1;
        let mut dirty = if self.in_transaction {
            std::mem::take(&mut self.txn_dirty_pages)
        } else {
            Vec::new()
        };

        let mut store = WalPageStore {
            pager: &mut self.pager,
            wal: &mut self.wal,
            dirty_pages: &mut dirty,
        };
        let _new_root = crate::btree::insert(&mut store, CATALOG_ROOT_PAGE, key, &value)?;

        if self.in_transaction {
            self.txn_dirty_pages = dirty;
        } else {
            self.commit_dirty_pages(dirty)?;
        }
        Ok(())
    }

    fn commit_dirty_pages(&mut self, pages: Vec<(u32, [u8; PAGE_SIZE])>) -> io::Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        self.wal.write_transaction(&pages)?;
        // Update pager page_count.
        for (pn, _) in &pages {
            if *pn > self.pager.header.page_count {
                self.pager.header.page_count = *pn;
            }
        }
        // Auto-checkpoint when WAL grows large.
        if self.wal.committed_frame_count() > 1000 {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Checkpoint: flush WAL to main DB.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.wal.checkpoint()?;
        self.pager.invalidate_cache();
        self.pager.reload_header()?;
        Ok(())
    }

    /// List all table names.
    pub fn list_tables(&self) -> Vec<String> {
        let mut names: Vec<String> = self.schema_cache.keys().cloned().collect();
        names.sort();
        names
    }

    // ─────────── CREATE TABLE ───────────

    fn exec_create_table(
        &mut self,
        table_name: &str,
        col_defs: &[ColumnDef],
        original_sql: &str,
    ) -> io::Result<QueryResult> {
        if self.schema_cache.contains_key(table_name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("table '{}' already exists", table_name),
            ));
        }

        // Allocate a root page for the new table.
        let mut dirty = if self.in_transaction {
            std::mem::take(&mut self.txn_dirty_pages)
        } else {
            Vec::new()
        };

        let root_page = {
            let mut store = WalPageStore {
                pager: &mut self.pager,
                wal: &mut self.wal,
                dirty_pages: &mut dirty,
            };
            let rp = store.allocate_page()?;
            let mut page = [0u8; PAGE_SIZE];
            init_leaf_page(&mut page);
            store.write_page(rp, page)?;
            rp
        };

        if self.in_transaction {
            self.txn_dirty_pages = dirty;
        } else {
            self.commit_dirty_pages(dirty)?;
        }

        let columns: Vec<Column> = col_defs
            .iter()
            .map(|cd| {
                // Extract default value text from the parsed Expr.
                let default_text = cd.default_value.as_ref().and_then(|expr| match expr {
                    Expr::Literal(Value::Text(s)) => Some(s.clone()),
                    Expr::Literal(Value::Integer(i)) => Some(i.to_string()),
                    Expr::Literal(Value::Real(r)) => Some(r.to_string()),
                    Expr::Literal(Value::Null) => Some("NULL".to_string()),
                    Expr::ColumnRef(name) => Some(name.clone()), // e.g. CURRENT_TIMESTAMP
                    _ => None,
                });
                Column {
                    name: cd.name.clone(),
                    data_type: cd.data_type.clone(),
                    is_primary_key: cd.is_primary_key,
                    is_autoincrement: cd.is_autoincrement,
                    is_not_null: cd.is_not_null,
                    is_unique: cd.is_unique,
                    default_text,
                }
            })
            .collect();

        let table = Table {
            name: table_name.to_string(),
            columns,
            root_page,
            sql: original_sql.trim().to_string(),
        };

        self.save_table_to_catalog(&table)?;
        self.schema_cache.insert(table_name.to_string(), table);
        self.next_rowid.insert(table_name.to_string(), 1);

        Ok(QueryResult::empty())
    }

    // ─────────── DROP TABLE ───────────

    fn exec_drop_table(&mut self, table_name: &str) -> io::Result<QueryResult> {
        if !self.schema_cache.contains_key(table_name) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("table '{}' does not exist", table_name),
            ));
        }

        // Remove from catalog B-Tree.
        let mut key_to_delete = None;
        let mut dirty_scan = Vec::new();
        {
            let mut store = WalPageStore {
                pager: &mut self.pager,
                wal: &mut self.wal,
                dirty_pages: &mut dirty_scan,
            };
            if let Ok(entries) = crate::btree::scan_all(&mut store, CATALOG_ROOT_PAGE) {
                for (k, v) in entries {
                    if let Ok(row) = deserialize_row(&v) {
                        if row.len() >= 2 {
                            if let Value::Text(ref n) = row[1] {
                                if n == table_name {
                                    key_to_delete = Some(k);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(key) = key_to_delete {
            let mut dirty = if self.in_transaction {
                std::mem::take(&mut self.txn_dirty_pages)
            } else {
                Vec::new()
            };
            {
                let mut store = WalPageStore {
                    pager: &mut self.pager,
                    wal: &mut self.wal,
                    dirty_pages: &mut dirty,
                };
                crate::btree::delete(&mut store, CATALOG_ROOT_PAGE, key)?;
            }
            if self.in_transaction {
                self.txn_dirty_pages = dirty;
            } else {
                self.commit_dirty_pages(dirty)?;
            }
        }

        self.schema_cache.remove(table_name);
        self.next_rowid.remove(table_name);
        Ok(QueryResult::empty())
    }

    // ─────────── INSERT ───────────

    fn exec_insert(
        &mut self,
        table_name: &str,
        col_names: Option<&[String]>,
        value_rows: &[Vec<Expr>],
    ) -> io::Result<QueryResult> {
        let table = self
            .schema_cache
            .get(table_name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such table: {}", table_name),
                )
            })?
            .clone();

        let mut count = 0u64;

        for exprs in value_rows {
            let mut row_values = vec![Value::Null; table.columns.len()];

            if let Some(cols) = col_names {
                // Map provided columns to positions.
                for (i, cn) in cols.iter().enumerate() {
                    let idx = table
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(cn))
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("unknown column: {}", cn),
                            )
                        })?;
                    row_values[idx] = self.eval_expr_no_row(&exprs[i])?;
                }
                // Fill in defaults for missing columns.
                for (idx, col) in table.columns.iter().enumerate() {
                    if row_values[idx] != Value::Null {
                        continue; // already provided
                    }
                    if cols.iter().any(|c| c.eq_ignore_ascii_case(&col.name)) {
                        continue; // explicitly set to NULL
                    }
                    // AUTOINCREMENT: generate next id.
                    if col.is_autoincrement
                        || (col.is_primary_key && col.data_type == crate::schema::DataType::Integer)
                    {
                        let rid = self.get_next_rowid(table_name);
                        row_values[idx] = Value::Integer(rid);
                    } else if let Some(ref def) = col.default_text {
                        // Apply DEFAULT value.
                        if def.eq_ignore_ascii_case("now")
                            || def.eq_ignore_ascii_case("current_timestamp")
                        {
                            row_values[idx] = Value::Text(current_timestamp());
                        } else {
                            row_values[idx] = Value::Text(def.clone());
                        }
                    }
                }
            } else {
                // Positional — must match column count.
                if exprs.len() != table.columns.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "expected {} values, got {}",
                            table.columns.len(),
                            exprs.len()
                        ),
                    ));
                }
                for (i, expr) in exprs.iter().enumerate() {
                    row_values[i] = self.eval_expr_no_row(expr)?;
                }

                // For positional inserts, also fill missing AUTOINCREMENT / DEFAULT
                // if they literally passed less arguments or used NULL for those places.
                // Wait, positional inserts must exactly match the number of columns right now.
                // But if they provided Null, let's override it if it has a default!
                for (idx, col) in table.columns.iter().enumerate() {
                    if row_values[idx] == Value::Null {
                        if col.is_autoincrement
                            || (col.is_primary_key
                                && col.data_type == crate::schema::DataType::Integer)
                        {
                            let rid = self.get_next_rowid(table_name);
                            row_values[idx] = Value::Integer(rid);
                        } else if let Some(ref def) = col.default_text {
                            if def.eq_ignore_ascii_case("now")
                                || def.eq_ignore_ascii_case("current_timestamp")
                            {
                                row_values[idx] = Value::Text(current_timestamp());
                            } else {
                                row_values[idx] = Value::Text(def.clone());
                            }
                        }
                    }
                }
            }

            // Generate rowid key (reuse if we already generated it for AUTOINCREMENT).
            let rowid = if let Some((idx, ref _col)) =
                table.columns.iter().enumerate().find(|(_, c)| {
                    c.is_primary_key && c.data_type == crate::schema::DataType::Integer
                }) {
                match row_values[idx] {
                    Value::Integer(v) => v,
                    _ => self.get_next_rowid(table_name),
                }
            } else {
                self.get_next_rowid(table_name)
            };

            let key = rowid as u64;

            // ── Constraint enforcement ──
            // NOT NULL: a NOT NULL column may not be NULL.
            for (idx, col) in table.columns.iter().enumerate() {
                if col.is_not_null && row_values[idx] == Value::Null {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "NOT NULL constraint failed: {}.{}",
                            table_name, col.name
                        ),
                    ));
                }
            }

            // UNIQUE: no two rows may have the same non-NULL value in a UNIQUE column.
            for (idx, col) in table.columns.iter().enumerate() {
                if !col.is_unique {
                    continue;
                }
                let candidate = &row_values[idx];
                if *candidate == Value::Null {
                    // NULLs are distinct — multiple NULLs are allowed.
                    continue;
                }
                let root_page = table.root_page;
                let mut dummy_dirty = Vec::new();
                let mut store = WalPageStore {
                    pager: &mut self.pager,
                    wal: &mut self.wal,
                    dirty_pages: &mut dummy_dirty,
                };
                let entries = crate::btree::scan_all(&mut store, root_page)?;
                for (_existing_key, value) in &entries {
                    let existing_row = match deserialize_row(value) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    if existing_row.get(idx) == Some(candidate) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "UNIQUE constraint failed: {}.{}",
                                table_name, col.name
                            ),
                        ));
                    }
                }
            }

            let value = serialize_row(&row_values);

            let root_page = table.root_page;
            let mut dirty = if self.in_transaction {
                std::mem::take(&mut self.txn_dirty_pages)
            } else {
                Vec::new()
            };
            let new_root = {
                let mut store = WalPageStore {
                    pager: &mut self.pager,
                    wal: &mut self.wal,
                    dirty_pages: &mut dirty,
                };
                crate::btree::insert(&mut store, root_page, key, &value)?
            };

            if new_root != root_page {
                // Root page changed due to split — update catalog.
                if self.in_transaction {
                    self.txn_dirty_pages = dirty;
                } else {
                    self.commit_dirty_pages(dirty)?;
                }
                self.update_table_root(table_name, new_root)?;
            } else if self.in_transaction {
                self.txn_dirty_pages = dirty;
            } else {
                self.commit_dirty_pages(dirty)?;
            }

            count += 1;
        }

        Ok(QueryResult::with_affected(count))
    }

    fn get_next_rowid(&mut self, table_name: &str) -> i64 {
        if let Some(rid) = self.next_rowid.get_mut(table_name) {
            let v = *rid;
            *rid += 1;
            return v;
        }

        // If not in cache, we need to find the max rowid from the B-Tree.
        let mut max_id = 0i64;
        if let Some(table) = self.schema_cache.get(table_name) {
            let root_page = table.root_page;
            // Scan all keys (which are rowids). Since keys are serialized as LE bytes,
            // we can deserialize them back to i64 to find the max.
            // Note: in a real implementation we'd just traverse down the rightmost children
            // of the B-Tree to find the max key in O(log N) rather than sweeping, but since
            // `scan_all` exists, we'll use it for simplicity here.
            let mut dummy_dirty = Vec::new();
            let mut store = WalPageStore {
                pager: &mut self.pager,
                wal: &mut self.wal,
                dirty_pages: &mut dummy_dirty,
            };
            if let Ok(entries) = crate::btree::scan_all(&mut store, root_page) {
                for (k, _) in entries {
                    let id = k as i64;
                    if id > max_id {
                        max_id = id;
                    }
                }
            }
        }

        let next_id = max_id + 1;
        self.next_rowid.insert(table_name.to_string(), next_id + 1);
        next_id
    }

    fn update_table_root(&mut self, table_name: &str, new_root: u32) -> io::Result<()> {
        if let Some(table) = self.schema_cache.get_mut(table_name) {
            table.root_page = new_root;
            let table_clone = table.clone();
            self.save_table_to_catalog(&table_clone)?;
        }
        Ok(())
    }

    // ─────────── SELECT ───────────

    fn exec_select(
        &mut self,
        select_cols: &[SelectColumn],
        table_name: &str,
        where_clause: Option<&Expr>,
    ) -> io::Result<QueryResult> {
        let table = self
            .schema_cache
            .get(table_name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such table: {}", table_name),
                )
            })?
            .clone();

        // Scan all rows.
        let root = table.root_page;
        let mut dirty = Vec::new();
        let all_kv = {
            let mut store = WalPageStore {
                pager: &mut self.pager,
                wal: &mut self.wal,
                dirty_pages: &mut dirty,
            };
            crate::btree::scan_all(&mut store, root)?
        };

        // Determine output columns.
        let output_col_names: Vec<String> = match &select_cols[0] {
            SelectColumn::AllColumns => table.columns.iter().map(|c| c.name.clone()).collect(),
            _ => select_cols
                .iter()
                .map(|sc| match sc {
                    SelectColumn::Named(n) => n.clone(),
                    SelectColumn::AllColumns => "*".to_string(),
                })
                .collect(),
        };

        let col_indices: Vec<usize> = match &select_cols[0] {
            SelectColumn::AllColumns => (0..table.columns.len()).collect(),
            _ => select_cols
                .iter()
                .map(|sc| match sc {
                    SelectColumn::Named(n) => table
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(n))
                        .unwrap_or(0),
                    SelectColumn::AllColumns => 0,
                })
                .collect(),
        };

        let mut rows = Vec::new();
        for (_key, value) in &all_kv {
            let row = deserialize_row(value)?;
            if let Some(wc) = where_clause {
                let matches = self.eval_where(wc, &row, &table)?;
                if !matches {
                    continue;
                }
            }
            let projected: Row = col_indices.iter().map(|&i| row[i].clone()).collect();
            rows.push(projected);
        }

        Ok(QueryResult {
            columns: output_col_names,
            rows,
            rows_affected: 0,
        })
    }

    // ─────────── UPDATE ───────────

    fn exec_update(
        &mut self,
        table_name: &str,
        assignments: &[(String, Expr)],
        where_clause: Option<&Expr>,
    ) -> io::Result<QueryResult> {
        let table = self
            .schema_cache
            .get(table_name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such table: {}", table_name),
                )
            })?
            .clone();

        let root = table.root_page;
        let mut dirty_scan = Vec::new();
        let all_kv = {
            let mut store = WalPageStore {
                pager: &mut self.pager,
                wal: &mut self.wal,
                dirty_pages: &mut dirty_scan,
            };
            btree::scan_all(&mut store, root)?
        };

        let mut count = 0u64;
        let mut updates: Vec<(u64, Vec<u8>)> = Vec::new();

        for (key, value) in &all_kv {
            let mut row = deserialize_row(value)?;
            if let Some(wc) = where_clause {
                if !self.eval_where(wc, &row, &table)? {
                    continue;
                }
            }
            // Apply assignments.
            for (col_name, expr) in assignments {
                let idx = table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(col_name))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown column: {}", col_name),
                        )
                    })?;
                row[idx] = self.eval_expr_with_row(expr, &row, &table)?;
            }
            updates.push((*key, serialize_row(&row)));
            count += 1;
        }

        // Write updated rows.
        for (key, new_value) in updates {
            let root = self
                .schema_cache
                .get(table_name)
                .map(|t| t.root_page)
                .unwrap_or(root);
            let mut dirty = if self.in_transaction {
                std::mem::take(&mut self.txn_dirty_pages)
            } else {
                Vec::new()
            };
            let new_root = {
                let mut store = WalPageStore {
                    pager: &mut self.pager,
                    wal: &mut self.wal,
                    dirty_pages: &mut dirty,
                };
                crate::btree::insert(&mut store, root, key, &new_value)?
            };
            if new_root != root {
                if self.in_transaction {
                    self.txn_dirty_pages = dirty;
                } else {
                    self.commit_dirty_pages(dirty)?;
                }
                self.update_table_root(table_name, new_root)?;
            } else if self.in_transaction {
                self.txn_dirty_pages = dirty;
            } else {
                self.commit_dirty_pages(dirty)?;
            }
        }

        Ok(QueryResult::with_affected(count))
    }

    // ─────────── DELETE ───────────

    fn exec_delete(
        &mut self,
        table_name: &str,
        where_clause: Option<&Expr>,
    ) -> io::Result<QueryResult> {
        let table = self
            .schema_cache
            .get(table_name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such table: {}", table_name),
                )
            })?
            .clone();

        let root = table.root_page;
        let mut dirty_scan = Vec::new();
        let all_kv = {
            let mut store = WalPageStore {
                pager: &mut self.pager,
                wal: &mut self.wal,
                dirty_pages: &mut dirty_scan,
            };
            btree::scan_all(&mut store, root)?
        };

        let mut keys_to_delete: Vec<u64> = Vec::new();
        for (key, value) in &all_kv {
            let row = deserialize_row(value)?;
            let should_delete = if let Some(wc) = where_clause {
                self.eval_where(wc, &row, &table)?
            } else {
                true
            };
            if should_delete {
                keys_to_delete.push(*key);
            }
        }

        let count = keys_to_delete.len() as u64;

        for key in keys_to_delete {
            let root = self
                .schema_cache
                .get(table_name)
                .map(|t| t.root_page)
                .unwrap_or(root);
            let mut dirty = if self.in_transaction {
                std::mem::take(&mut self.txn_dirty_pages)
            } else {
                Vec::new()
            };
            {
                let mut store = WalPageStore {
                    pager: &mut self.pager,
                    wal: &mut self.wal,
                    dirty_pages: &mut dirty,
                };
                crate::btree::delete(&mut store, root, key)?;
            }
            if self.in_transaction {
                self.txn_dirty_pages = dirty;
            } else {
                self.commit_dirty_pages(dirty)?;
            }
        }

        Ok(QueryResult::with_affected(count))
    }

    // ─────────── COMMIT / ROLLBACK ───────────

    fn exec_commit(&mut self) -> io::Result<QueryResult> {
        if !self.in_transaction {
            return Ok(QueryResult::empty());
        }
        let dirty = std::mem::take(&mut self.txn_dirty_pages);
        self.commit_dirty_pages(dirty)?;
        self.in_transaction = false;
        Ok(QueryResult::empty())
    }

    fn exec_rollback(&mut self) -> io::Result<QueryResult> {
        if !self.in_transaction {
            return Ok(QueryResult::empty());
        }
        // Discard all buffered writes.
        self.txn_dirty_pages.clear();
        self.in_transaction = false;
        // Reload schema in case we had provisional schema changes.
        self.load_catalog()?;
        Ok(QueryResult::empty())
    }

    // ─────────── Expression evaluation ───────────

    fn eval_expr_no_row(&self, expr: &Expr) -> io::Result<Value> {
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::ColumnRef(name) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("column ref '{}' not allowed here", name),
            )),
            Expr::BinaryOp { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "binary op not allowed in VALUES",
            )),
        }
    }

    fn eval_expr_with_row(&self, expr: &Expr, row: &[Value], table: &Table) -> io::Result<Value> {
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::ColumnRef(name) => {
                let idx = table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown column: {}", name),
                        )
                    })?;
                Ok(row[idx].clone())
            }
            Expr::BinaryOp { left, op, right } => {
                let lv = self.eval_expr_with_row(left, row, table)?;
                let rv = self.eval_expr_with_row(right, row, table)?;
                self.eval_binop(&lv, op, &rv)
            }
        }
    }

    fn eval_where(&self, expr: &Expr, row: &[Value], table: &Table) -> io::Result<bool> {
        let v = self.eval_expr_with_row(expr, row, table)?;
        match v {
            Value::Integer(i) => Ok(i != 0),
            _ => Ok(false),
        }
    }

    fn eval_binop(&self, left: &Value, op: &BinOp, right: &Value) -> io::Result<Value> {
        match op {
            BinOp::And => {
                let lb = self.value_truthy(left);
                let rb = self.value_truthy(right);
                Ok(Value::Integer(if lb && rb { 1 } else { 0 }))
            }
            BinOp::Or => {
                let lb = self.value_truthy(left);
                let rb = self.value_truthy(right);
                Ok(Value::Integer(if lb || rb { 1 } else { 0 }))
            }
            _ => {
                let cmp = self.compare_values(left, right);
                let result = match op {
                    BinOp::Eq => cmp == std::cmp::Ordering::Equal,
                    BinOp::Neq => cmp != std::cmp::Ordering::Equal,
                    BinOp::Lt => cmp == std::cmp::Ordering::Less,
                    BinOp::Gt => cmp == std::cmp::Ordering::Greater,
                    BinOp::LtEq => cmp != std::cmp::Ordering::Greater,
                    BinOp::GtEq => cmp != std::cmp::Ordering::Less,
                    _ => false,
                };
                Ok(Value::Integer(if result { 1 } else { 0 }))
            }
        }
    }

    fn value_truthy(&self, v: &Value) -> bool {
        match v {
            Value::Null => false,
            Value::Integer(i) => *i != 0,
            Value::Real(r) => *r != 0.0,
            Value::Text(s) => !s.is_empty(),
            Value::Blob(b) => !b.is_empty(),
        }
    }

    fn compare_values(&self, a: &Value, b: &Value) -> std::cmp::Ordering {
        match (a, b) {
            (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
            (Value::Real(x), Value::Real(y)) => {
                x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
            }
            (Value::Integer(x), Value::Real(y)) => (*x as f64)
                .partial_cmp(y)
                .unwrap_or(std::cmp::Ordering::Equal),
            (Value::Real(x), Value::Integer(y)) => x
                .partial_cmp(&(*y as f64))
                .unwrap_or(std::cmp::Ordering::Equal),
            (Value::Text(x), Value::Text(y)) => x.cmp(y),
            (Value::Text(x), Value::Integer(y)) => x.cmp(&y.to_string()),
            (Value::Integer(x), Value::Text(y)) => x.to_string().cmp(y),
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            (Value::Null, _) => std::cmp::Ordering::Less,
            (_, Value::Null) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        }
    }

    /// Close the database, checkpointing the WAL.
    pub fn close(mut self) -> io::Result<()> {
        if self.wal.has_frames() {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Get the WAL path (for testing).
    pub fn wal_path(&self) -> PathBuf {
        self.wal.path().to_path_buf()
    }

    /// Force a checkpoint (public, for testing).
    pub fn force_checkpoint(&mut self) -> io::Result<()> {
        self.checkpoint()
    }

    /// Check if WAL has uncommitted frames.
    pub fn wal_has_frames(&self) -> bool {
        self.wal.has_frames()
    }
}

// ───────────────────────────── Tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_db(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rustsqlite_engine_{}", name));
        p
    }

    fn cleanup(db: &Path) {
        let _ = fs::remove_file(db);
        let wal = PathBuf::from(format!("{}-wal", db.display()));
        let _ = fs::remove_file(&wal);
    }

    #[test]
    fn test_create_and_insert() {
        let db = tmp_db("create_insert");
        cleanup(&db);

        let mut engine = Engine::open(&db).unwrap();
        engine
            .execute("CREATE TABLE users (id INTEGER, name TEXT, age INTEGER);")
            .unwrap();
        let r = engine
            .execute("INSERT INTO users VALUES (1, 'Alice', 30);")
            .unwrap();
        assert_eq!(r.rows_affected, 1);

        let r2 = engine.execute("SELECT * FROM users;").unwrap();
        assert_eq!(r2.rows.len(), 1);
        assert_eq!(r2.rows[0][1], Value::Text("Alice".to_string()));

        engine.close().unwrap();
        cleanup(&db);
    }

    #[test]
    fn test_select_where() {
        let db = tmp_db("select_where");
        cleanup(&db);

        let mut engine = Engine::open(&db).unwrap();
        engine
            .execute("CREATE TABLE t (x INTEGER, y TEXT);")
            .unwrap();
        engine.execute("INSERT INTO t VALUES (1, 'a');").unwrap();
        engine.execute("INSERT INTO t VALUES (2, 'b');").unwrap();
        engine.execute("INSERT INTO t VALUES (3, 'c');").unwrap();

        let r = engine.execute("SELECT * FROM t WHERE x > 1;").unwrap();
        assert_eq!(r.rows.len(), 2);

        let r2 = engine.execute("SELECT y FROM t WHERE x = 2;").unwrap();
        assert_eq!(r2.rows.len(), 1);
        assert_eq!(r2.rows[0][0], Value::Text("b".to_string()));

        engine.close().unwrap();
        cleanup(&db);
    }

    #[test]
    fn test_update() {
        let db = tmp_db("update");
        cleanup(&db);

        let mut engine = Engine::open(&db).unwrap();
        engine
            .execute("CREATE TABLE t (id INTEGER, val TEXT);")
            .unwrap();
        engine.execute("INSERT INTO t VALUES (1, 'old');").unwrap();

        let r = engine
            .execute("UPDATE t SET val = 'new' WHERE id = 1;")
            .unwrap();
        assert_eq!(r.rows_affected, 1);

        let r2 = engine.execute("SELECT val FROM t WHERE id = 1;").unwrap();
        assert_eq!(r2.rows[0][0], Value::Text("new".to_string()));

        engine.close().unwrap();
        cleanup(&db);
    }

    #[test]
    fn test_delete() {
        let db = tmp_db("delete");
        cleanup(&db);

        let mut engine = Engine::open(&db).unwrap();
        engine
            .execute("CREATE TABLE t (id INTEGER, val TEXT);")
            .unwrap();
        engine.execute("INSERT INTO t VALUES (1, 'a');").unwrap();
        engine.execute("INSERT INTO t VALUES (2, 'b');").unwrap();
        engine.execute("INSERT INTO t VALUES (3, 'c');").unwrap();

        let r = engine.execute("DELETE FROM t WHERE id = 2;").unwrap();
        assert_eq!(r.rows_affected, 1);

        let r2 = engine.execute("SELECT * FROM t;").unwrap();
        assert_eq!(r2.rows.len(), 2);

        engine.close().unwrap();
        cleanup(&db);
    }

    #[test]
    fn test_multiple_tables() {
        let db = tmp_db("multi_table");
        cleanup(&db);

        let mut engine = Engine::open(&db).unwrap();
        engine.execute("CREATE TABLE a (x INTEGER);").unwrap();
        engine.execute("CREATE TABLE b (y TEXT);").unwrap();
        engine.execute("INSERT INTO a VALUES (42);").unwrap();
        engine.execute("INSERT INTO b VALUES ('hello');").unwrap();

        let ra = engine.execute("SELECT * FROM a;").unwrap();
        let rb = engine.execute("SELECT * FROM b;").unwrap();
        assert_eq!(ra.rows.len(), 1);
        assert_eq!(rb.rows.len(), 1);
        assert_eq!(ra.rows[0][0], Value::Integer(42));
        assert_eq!(rb.rows[0][0], Value::Text("hello".to_string()));

        engine.close().unwrap();
        cleanup(&db);
    }
}
