use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::btree::{self, PageStore, init_leaf_page};
use crate::index;
use crate::pager::{FileHeader, PAGE_SIZE, Pager};
use crate::schema::{
    Column, Index, Row, Table, Value, autoindex_columns, autoindex_name, compare_values,
    deserialize_row, index_record, serialize_row,
};
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

/// Build a catalog column from its parsed definition.
fn column_from_def(cd: &ColumnDef) -> Column {
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
        is_rowid_alias: cd.is_rowid_alias,
        default_text,
    }
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
        // Reuse a page from the freelist first.
        let trunk = self.pager.header.freelist_trunk_page;
        if trunk != 0 {
            let mut tp = self.read_page(trunk)?;
            let leaves = read_be_u32(&tp, 4);
            let page_num = if leaves > 0 {
                let leaf = read_be_u32(&tp, 8 + 4 * (leaves as usize - 1));
                write_be_u32(&mut tp, 4, leaves - 1);
                self.write_page(trunk, tp)?;
                leaf
            } else {
                // An empty trunk is handed out itself; its successor becomes the head.
                self.pager.header.freelist_trunk_page = read_be_u32(&tp, 0);
                trunk
            };
            self.pager.header.freelist_pages = self.pager.header.freelist_pages.saturating_sub(1);
            self.sync_header()?;
            return Ok(page_num);
        }

        // We need a new page number beyond all existing ones.
        let mut max_page = self.pager.header.page_count;
        for (pn, _) in self.dirty_pages.iter() {
            if *pn > max_page {
                max_page = *pn;
            }
        }
        Ok(max_page + 1)
    }

    fn free_page(&mut self, page_num: u32) -> io::Result<()> {
        // SQLite freelist: header offset 32 = first trunk page, offset 36 = total
        // freelist pages. A trunk holds [next trunk u32][leaf count u32][leaf u32...].
        let trunk = self.pager.header.freelist_trunk_page;
        if trunk != 0 {
            let mut tp = self.read_page(trunk)?;
            let leaves = read_be_u32(&tp, 4);
            if leaves < FREELIST_TRUNK_MAX_LEAVES {
                write_be_u32(&mut tp, 8 + 4 * leaves as usize, page_num);
                write_be_u32(&mut tp, 4, leaves + 1);
                self.write_page(trunk, tp)?;
                self.pager.header.freelist_pages += 1;
                return self.sync_header();
            }
        }
        // No trunk, or it is full: the freed page becomes the new head trunk.
        let mut tp = [0u8; PAGE_SIZE];
        write_be_u32(&mut tp, 0, trunk);
        self.write_page(page_num, tp)?;
        self.pager.header.freelist_trunk_page = page_num;
        self.pager.header.freelist_pages += 1;
        self.sync_header()
    }
}

/// Leaf slots used per freelist trunk page. SQLite accepts up to usable/4 - 2 but
/// writes at most usable/4 - 8 so that older readers stay compatible.
const FREELIST_TRUNK_MAX_LEAVES: u32 = (PAGE_SIZE / 4 - 8) as u32;

/// True if `row[i]` is the text `s`.
fn text_at(row: &[Value], i: usize, s: &str) -> bool {
    matches!(row.get(i), Some(Value::Text(t)) if t == s)
}

/// The column whose value is the row's B-tree key (rowid): the integer PRIMARY KEY,
/// if the table has one.
fn rowid_column(table: &Table) -> Option<usize> {
    table
        .columns
        .iter()
        .position(|c| c.is_primary_key && c.data_type == crate::schema::DataType::Integer)
}

fn read_be_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn write_be_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

impl WalPageStore<'_> {
    /// Rewrite page 1 so the pending writes carry the current in-memory file header
    /// (freelist fields included).
    fn sync_header(&mut self) -> io::Result<()> {
        let page = self.read_page(1)?;
        self.write_page(1, page)
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
    /// Pages written but not yet committed: by the open explicit transaction, or by
    /// the statement being executed. Reads see these before the WAL and the file.
    txn_dirty_pages: Vec<(u32, [u8; PAGE_SIZE])>,
    /// File header (freelist fields included) as of BEGIN, restored on ROLLBACK.
    txn_header: FileHeader,
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
        let mut wal = Wal::open(&path)?;

        // Committed WAL frames are newer than the main file. Take the freelist from
        // the latest page 1, and count every page in the WAL as allocated.
        if let Some(page1) = wal.read_page(CATALOG_ROOT_PAGE)? {
            let mut hdr = [0u8; FileHeader::SIZE];
            hdr.copy_from_slice(&page1[..FileHeader::SIZE]);
            if let Ok(h) = FileHeader::deserialize(&hdr) {
                pager.header.freelist_trunk_page = h.freelist_trunk_page;
                pager.header.freelist_pages = h.freelist_pages;
            }
        }
        pager.header.page_count = pager.header.page_count.max(wal.max_page());

        // Ensure the catalog B-Tree root page exists (page 1).
        if pager.header.page_count < 2 {
            // Read existing page 1 (which has the SQLite magic header in first 100 bytes).
            let mut page = pager.read_page(CATALOG_ROOT_PAGE)?;
            init_leaf_page(&mut page);
            pager.write_page(CATALOG_ROOT_PAGE, page)?;
        }

        let txn_header = pager.header;
        let mut engine = Engine {
            pager,
            wal,
            schema_cache: HashMap::new(),
            in_transaction: false,
            txn_dirty_pages: Vec::new(),
            txn_header,
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
            } => self.atomic(|e| e.exec_create_table(&table_name, &columns, original_sql)),
            Statement::DropTable { table_name } => self.atomic(|e| e.exec_drop_table(&table_name)),
            Statement::Insert {
                table_name,
                columns,
                values,
            } => self.atomic(|e| e.exec_insert(&table_name, columns.as_deref(), &values)),
            Statement::Select {
                columns,
                table_name,
                where_clause,
            } => self.exec_select(&columns, &table_name, where_clause.as_ref()),
            Statement::Update {
                table_name,
                assignments,
                where_clause,
            } => self.atomic(|e| e.exec_update(&table_name, &assignments, where_clause.as_ref())),
            Statement::Delete {
                table_name,
                where_clause,
            } => self.atomic(|e| e.exec_delete(&table_name, where_clause.as_ref())),
            Statement::Begin => {
                if self.in_transaction {
                    // The pending writes are dropped below; drop their header
                    // (freelist) changes with them.
                    self.pager.header = self.txn_header;
                }
                self.in_transaction = true;
                self.txn_dirty_pages.clear();
                self.txn_header = self.pager.header;
                Ok(QueryResult::empty())
            }
            Statement::Commit => self.exec_commit(),
            Statement::Rollback => self.exec_rollback(),
        }
    }

    // ─────────── Statement execution helpers ───────────

    /// Page access that sees every write not yet committed. Writes are added to
    /// `txn_dirty_pages`, which `atomic` commits when the statement succeeds (or
    /// which COMMIT flushes inside an explicit transaction).
    fn store(&mut self) -> WalPageStore<'_> {
        WalPageStore {
            pager: &mut self.pager,
            wal: &mut self.wal,
            dirty_pages: &mut self.txn_dirty_pages,
        }
    }

    /// Run a writing statement atomically. Its page writes are only committed (or
    /// kept in the open transaction) if it succeeds; on error, every page it wrote,
    /// the file header and the cached schema are put back as they were, so a failed
    /// constraint check partway through a multi-row statement changes nothing.
    fn atomic<F>(&mut self, f: F) -> io::Result<QueryResult>
    where
        F: FnOnce(&mut Self) -> io::Result<QueryResult>,
    {
        let implicit = !self.in_transaction;
        let mark = self.txn_dirty_pages.len();
        let header = self.pager.header;
        let schema = self.schema_cache.clone();
        let next_rowid = self.next_rowid.clone();
        let next_catalog_rowid = self.next_catalog_rowid;

        self.in_transaction = true;
        let result = f(self);
        if implicit {
            self.in_transaction = false;
        }
        match result {
            Ok(r) => {
                if implicit {
                    let dirty = std::mem::take(&mut self.txn_dirty_pages);
                    self.commit_dirty_pages(dirty)?;
                }
                Ok(r)
            }
            Err(e) => {
                self.txn_dirty_pages.truncate(mark);
                self.pager.header = header;
                self.schema_cache = schema;
                self.next_rowid = next_rowid;
                self.next_catalog_rowid = next_catalog_rowid;
                Err(e)
            }
        }
    }

    // ─────────── Catalog management ───────────

    fn load_catalog(&mut self) -> io::Result<()> {
        self.schema_cache.clear();
        let entries = crate::btree::scan_all(&mut self.store(), CATALOG_ROOT_PAGE)?;
        let mut max_catalog_rowid = 0u64;
        let mut index_roots: HashMap<String, u32> = HashMap::new();
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
            let name = match &row[1] {
                Value::Text(s) => s.clone(),
                _ => continue,
            };
            let root_page = match &row[3] {
                Value::Integer(i) => *i as u32,
                _ => continue,
            };
            if entry_type == "index" {
                index_roots.insert(name, root_page);
                continue;
            }
            if entry_type != "table" {
                continue;
            }
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
                    let columns: Vec<Column> = col_defs.iter().map(column_from_def).collect();
                    let table = Table {
                        name: name.clone(),
                        columns,
                        root_page,
                        sql: sql_text,
                        indexes: Vec::new(),
                    };
                    self.schema_cache.insert(name, table);
                }
            }
        }

        // Attach each table's automatic indexes. Their columns follow from the
        // CREATE TABLE text; their root pages come from the 'index' rows. A file
        // written before autoindexes existed has none, and then UNIQUE falls back
        // to a table scan.
        for table in self.schema_cache.values_mut() {
            table.indexes = autoindex_columns(&table.columns)
                .into_iter()
                .enumerate()
                .filter_map(|(n, column)| {
                    let name = autoindex_name(&table.name, n + 1);
                    let root_page = *index_roots.get(&name)?;
                    Some(Index {
                        name,
                        column,
                        root_page,
                    })
                })
                .collect();
        }

        self.next_catalog_rowid = max_catalog_rowid + 1;
        Ok(())
    }

    /// Rowids of the sqlite_schema rows for which `pred` holds.
    fn catalog_keys(&mut self, pred: impl Fn(&[Value]) -> bool) -> io::Result<Vec<u64>> {
        let entries = crate::btree::scan_all(&mut self.store(), CATALOG_ROOT_PAGE)?;
        Ok(entries
            .into_iter()
            .filter(|(_, v)| deserialize_row(v).map(|row| pred(&row)).unwrap_or(false))
            .map(|(k, _)| k)
            .collect())
    }

    /// Insert or overwrite the sqlite_schema row with rowid `key`.
    fn write_catalog_row(&mut self, key: u64, row: &[Value]) -> io::Result<()> {
        let value = serialize_row(row);
        let new_root = crate::btree::insert(&mut self.store(), CATALOG_ROOT_PAGE, key, &value)?;
        if new_root != CATALOG_ROOT_PAGE {
            // sqlite_schema must stay rooted at page 1; a split would move it.
            return Err(io::Error::other(
                "sqlite_schema is full: page 1 cannot split",
            ));
        }
        Ok(())
    }

    fn save_table_to_catalog(&mut self, table: &Table) -> io::Result<()> {
        // Overwrite the table's existing row in place, keeping its rowid. SQLite
        // reads sqlite_schema in rowid order and reports an index row that comes
        // before its table as an "orphan index".
        let existing = self
            .catalog_keys(|row| text_at(row, 0, "table") && text_at(row, 1, &table.name))?
            .into_iter()
            .next();
        let key = existing.unwrap_or_else(|| self.next_catalog_key());

        // Build the sqlite_schema row: (type, name, tbl_name, rootpage, sql)
        let schema_row = vec![
            Value::Text("table".to_string()),
            Value::Text(table.name.clone()),
            Value::Text(table.name.clone()),
            Value::Integer(table.root_page as i64),
            Value::Text(table.sql.clone()),
        ];
        self.write_catalog_row(key, &schema_row)
    }

    fn next_catalog_key(&mut self) -> u64 {
        let key = self.next_catalog_rowid;
        self.next_catalog_rowid += 1;
        key
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
        let root_page = {
            let mut store = self.store();
            let rp = store.allocate_page()?;
            let mut page = [0u8; PAGE_SIZE];
            init_leaf_page(&mut page);
            store.write_page(rp, page)?;
            rp
        };

        let columns: Vec<Column> = col_defs.iter().map(column_from_def).collect();

        let mut table = Table {
            name: table_name.to_string(),
            columns,
            root_page,
            sql: original_sql.trim().to_string(),
            indexes: Vec::new(),
        };

        self.save_table_to_catalog(&table)?;

        // SQLite keeps an automatic index for every UNIQUE column and for a PRIMARY
        // KEY that is not the rowid alias. Each gets its own root page and a
        // sqlite_schema row after the table's: ('index', name, table, root, NULL).
        for (n, column) in autoindex_columns(&table.columns).into_iter().enumerate() {
            let name = autoindex_name(table_name, n + 1);
            let index_root = {
                let mut store = self.store();
                let rp = store.allocate_page()?;
                let mut page = [0u8; PAGE_SIZE];
                index::init_root_page(&mut page);
                store.write_page(rp, page)?;
                rp
            };
            let key = self.next_catalog_key();
            self.write_catalog_row(
                key,
                &[
                    Value::Text("index".to_string()),
                    Value::Text(name.clone()),
                    Value::Text(table_name.to_string()),
                    Value::Integer(index_root as i64),
                    Value::Null,
                ],
            )?;
            table.indexes.push(Index {
                name,
                column,
                root_page: index_root,
            });
        }

        self.schema_cache.insert(table_name.to_string(), table);
        self.next_rowid.insert(table_name.to_string(), 1);

        Ok(QueryResult::empty())
    }

    // ─────────── DROP TABLE ───────────

    fn exec_drop_table(&mut self, table_name: &str) -> io::Result<QueryResult> {
        let table = self
            .schema_cache
            .get(table_name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("table '{}' does not exist", table_name),
                )
            })?
            .clone();

        // Remove the table's sqlite_schema row and the rows of its indexes.
        let keys = self.catalog_keys(|row| {
            (text_at(row, 0, "table") && text_at(row, 1, table_name))
                || (text_at(row, 0, "index") && text_at(row, 2, table_name))
        })?;
        for key in keys {
            crate::btree::delete(&mut self.store(), CATALOG_ROOT_PAGE, key)?;
        }

        // Put the table's and its indexes' pages on the freelist, so the file has
        // no pages that nothing references.
        let mut pages = crate::btree::collect_pages(&mut self.store(), table.root_page)?;
        for idx in &table.indexes {
            pages.extend(index::collect_pages(&mut self.store(), idx.root_page)?);
        }
        for page in pages {
            self.store().free_page(page)?;
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
            let rowid = if let Some(idx) = rowid_column(&table) {
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

            // The rowid (the INTEGER PRIMARY KEY) must be free, then the UNIQUE and
            // non-rowid PRIMARY KEY columns are checked through their indexes, in
            // SQLite's order. A failure undoes the whole statement (see `atomic`).
            self.check_rowid_free(&table, key)?;
            self.check_unique(&table, &row_values, rowid, None)?;

            let value = serialize_row(&row_values);
            self.write_row(table_name, key, &value)?;
            self.update_indexes(&table, rowid, None, &row_values)?;
            self.note_rowid_used(table_name, rowid);

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
            if let Ok(entries) = crate::btree::scan_all(&mut self.store(), root_page) {
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

    /// The table's current root page. It can move when the root splits, so a
    /// statement must not keep using the one it started with.
    fn table_root(&self, table_name: &str) -> u32 {
        self.schema_cache
            .get(table_name)
            .map(|t| t.root_page)
            .unwrap_or(0)
    }

    /// Store a table row under `key`, following the root if it moves.
    fn write_row(&mut self, table_name: &str, key: u64, value: &[u8]) -> io::Result<()> {
        let root = self.table_root(table_name);
        let new_root = crate::btree::insert(&mut self.store(), root, key, value)?;
        if new_root != root {
            // Root page changed due to split — update catalog.
            self.update_table_root(table_name, new_root)?;
        }
        Ok(())
    }

    /// Fail if writing `row` as `rowid` would give a UNIQUE (or non-rowid PRIMARY
    /// KEY) column a value another row already holds. For an UPDATE, `old` is the
    /// row's current contents: unchanged values are skipped and the row's own
    /// index entry is not a conflict. For an INSERT (`old` is None) every existing
    /// holder conflicts. NULLs never conflict.
    fn check_unique(
        &mut self,
        table: &Table,
        row: &[Value],
        rowid: i64,
        old: Option<&[Value]>,
    ) -> io::Result<()> {
        for column in autoindex_columns(&table.columns) {
            let value = &row[column];
            if *value == Value::Null || old.is_some_and(|o| o[column] == *value) {
                continue;
            }
            let holder = match table.indexes.iter().find(|i| i.column == column) {
                Some(idx) => index::find_rowid(&mut self.store(), idx.root_page, value)?,
                // A file written before autoindexes existed: scan the table.
                None => {
                    let root = self.table_root(&table.name);
                    let mut found = None;
                    for (k, v) in crate::btree::scan_all(&mut self.store(), root)? {
                        let existing = deserialize_row(&v)?;
                        if existing
                            .get(column)
                            .is_some_and(|e| compare_values(e, value) == std::cmp::Ordering::Equal)
                        {
                            found = Some(k as i64);
                            break;
                        }
                    }
                    found
                }
            };
            if holder.is_some_and(|h| old.is_none() || h != rowid) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "UNIQUE constraint failed: {}.{}",
                        table.name, table.columns[column].name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Fail if the table already has a row with rowid `key`. The rowid is the
    /// INTEGER PRIMARY KEY, so SQLite reports this as a UNIQUE failure on that
    /// column (on "rowid" if the table has none).
    fn check_rowid_free(&mut self, table: &Table, key: u64) -> io::Result<()> {
        let root = self.table_root(&table.name);
        if btree::search(&mut self.store(), root, key)?.is_none() {
            return Ok(());
        }
        let column = rowid_column(table).map_or("rowid", |i| table.columns[i].name.as_str());
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("UNIQUE constraint failed: {}.{}", table.name, column),
        ))
    }

    /// Record that `rowid` is taken, so that rowids the engine picks later (for
    /// rows inserted without one) come after it instead of colliding with it.
    fn note_rowid_used(&mut self, table_name: &str, rowid: i64) {
        if let Some(next) = self.next_rowid.get_mut(table_name) {
            if *next <= rowid {
                *next = rowid.saturating_add(1);
            }
        }
    }

    /// Bring the automatic indexes in line after row `rowid` changed from `old`
    /// (None for a new row) to `new`.
    fn update_indexes(
        &mut self,
        table: &Table,
        rowid: i64,
        old: Option<&[Value]>,
        new: &[Value],
    ) -> io::Result<()> {
        for idx in &table.indexes {
            let value = &new[idx.column];
            if let Some(old) = old {
                if old[idx.column] == *value {
                    continue;
                }
                self.index_delete(idx, &old[idx.column], rowid)?;
            }
            index::insert(
                &mut self.store(),
                idx.root_page,
                &index_record(value, rowid),
            )?;
        }
        Ok(())
    }

    fn index_delete(&mut self, idx: &Index, value: &Value, rowid: i64) -> io::Result<()> {
        if !index::delete(
            &mut self.store(),
            idx.root_page,
            &index_record(value, rowid),
        )? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("index {} has no entry for rowid {}", idx.name, rowid),
            ));
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

        // Scan all rows, including writes of the open transaction.
        let root = table.root_page;
        let all_kv = crate::btree::scan_all(&mut self.store(), root)?;

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

        let all_kv = btree::scan_all(&mut self.store(), table.root_page)?;

        let mut count = 0u64;
        let mut updates: Vec<(u64, Row, Row)> = Vec::new();

        for (key, value) in &all_kv {
            let old_row = deserialize_row(value)?;
            let mut row = old_row.clone();
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
            updates.push((*key, old_row, row));
            count += 1;
        }

        // Write updated rows one at a time, as SQLite does: each row's new values are
        // checked against the table and the indexes as left by the rows before it.
        // A conflict fails the whole statement (see `atomic`).
        let rowid_col = rowid_column(&table);
        for (key, old_row, new_row) in updates {
            let rowid = key as i64;
            let new_rowid = match rowid_col {
                Some(c) if new_row[c] != old_row[c] => match new_row[c] {
                    Value::Integer(v) => v,
                    // SQLite: an INTEGER PRIMARY KEY can only hold an integer.
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "datatype mismatch",
                        ));
                    }
                },
                _ => rowid,
            };
            if new_rowid == rowid {
                self.check_unique(&table, &new_row, rowid, Some(&old_row))?;
                self.write_row(table_name, key, &serialize_row(&new_row))?;
                self.update_indexes(&table, rowid, Some(&old_row), &new_row)?;
                continue;
            }
            // A new INTEGER PRIMARY KEY is a new rowid: the row moves to that key,
            // and every index entry of the row is rewritten with it.
            self.check_rowid_free(&table, new_rowid as u64)?;
            self.check_unique(&table, &new_row, rowid, Some(&old_row))?;
            let root = self.table_root(table_name);
            btree::delete(&mut self.store(), root, key)?;
            self.write_row(table_name, new_rowid as u64, &serialize_row(&new_row))?;
            for idx in &table.indexes {
                self.index_delete(idx, &old_row[idx.column], rowid)?;
                index::insert(
                    &mut self.store(),
                    idx.root_page,
                    &index_record(&new_row[idx.column], new_rowid),
                )?;
            }
            self.note_rowid_used(table_name, new_rowid);
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

        let all_kv = btree::scan_all(&mut self.store(), table.root_page)?;

        let mut rows_to_delete: Vec<(u64, Row)> = Vec::new();
        for (key, value) in &all_kv {
            let row = deserialize_row(value)?;
            let should_delete = if let Some(wc) = where_clause {
                self.eval_where(wc, &row, &table)?
            } else {
                true
            };
            if should_delete {
                rows_to_delete.push((*key, row));
            }
        }

        let count = rows_to_delete.len() as u64;

        for (key, row) in rows_to_delete {
            let root = self.table_root(table_name);
            crate::btree::delete(&mut self.store(), root, key)?;
            for idx in &table.indexes {
                self.index_delete(idx, &row[idx.column], key as i64)?;
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
        // Discard all buffered writes, and the freelist changes they made.
        self.txn_dirty_pages.clear();
        self.pager.header = self.txn_header;
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
