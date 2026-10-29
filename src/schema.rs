use crate::varint::{get_varint, put_varint};
use std::cmp::Ordering;
use std::fmt;
use std::io;

/// Supported column data types.
#[derive(Debug, Clone, PartialEq)]
pub enum DataType {
    Integer,
    Text,
    Real,
    Blob,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Integer => write!(f, "INTEGER"),
            DataType::Text => write!(f, "TEXT"),
            DataType::Real => write!(f, "REAL"),
            DataType::Blob => write!(f, "BLOB"),
        }
    }
}

impl DataType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "INTEGER" | "INT" | "TINYINT" | "SMALLINT" | "MEDIUMINT" | "BIGINT" | "INT2"
            | "INT8" | "BOOLEAN" | "BOOL" => Some(DataType::Integer),
            "TEXT" | "VARCHAR" | "STRING" | "CHAR" | "NCHAR" | "NVARCHAR" | "CLOB"
            | "CHARACTER" | "DATETIME" | "TIMESTAMP" | "DATE" | "TIME" => Some(DataType::Text),
            "REAL" | "FLOAT" | "DOUBLE" | "NUMERIC" | "DECIMAL" | "NUMBER" => Some(DataType::Real),
            "BLOB" | "BINARY" | "VARBINARY" => Some(DataType::Blob),
            _ => None,
        }
    }
}

/// A column definition.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub is_primary_key: bool,
    pub is_autoincrement: bool,
    pub is_not_null: bool,
    pub is_unique: bool,
    /// Declared exactly `INTEGER PRIMARY KEY`, which SQLite treats as an alias for
    /// the rowid (no automatic index). `INT PRIMARY KEY` and friends are not.
    pub is_rowid_alias: bool,
    /// Default value as a string literal (if any).
    pub default_text: Option<String>,
}

/// A table definition.
#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// The B-Tree root page for this table's data.
    pub root_page: u32,
    /// The original CREATE TABLE SQL statement.
    pub sql: String,
    /// Automatic indexes for UNIQUE and non-INTEGER PRIMARY KEY columns.
    pub indexes: Vec<Index>,
}

/// An automatic index (`sqlite_autoindex_<table>_<n>`) that SQLite expects for a
/// UNIQUE column or a PRIMARY KEY that is not the rowid alias.
#[derive(Debug, Clone)]
pub struct Index {
    pub name: String,
    /// Position of the indexed column in the table.
    pub column: usize,
    /// Root page of the index B-tree. It never moves.
    pub root_page: u32,
}

/// Columns that get an automatic index, in the order SQLite creates and numbers
/// them: one per column (PRIMARY KEY and UNIQUE on the same column share one),
/// in column order.
pub fn autoindex_columns(columns: &[Column]) -> Vec<usize> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.is_unique || (c.is_primary_key && !c.is_rowid_alias))
        .map(|(i, _)| i)
        .collect()
}

/// SQLite's name for the `n`-th (1-based) automatic index of `table`.
pub fn autoindex_name(table: &str, n: usize) -> String {
    format!("sqlite_autoindex_{}_{}", table, n)
}

/// The record SQLite stores in a single-column index on a rowid table:
/// (column value, rowid).
pub fn index_record(value: &Value, rowid: i64) -> Vec<u8> {
    serialize_row(&[value.clone(), Value::Integer(rowid)])
}

/// Order two values the way SQLite orders index records under the BINARY
/// collation: NULL < INTEGER and REAL (compared by numeric value) < TEXT < BLOB.
/// TEXT and BLOB compare bytewise (memcmp), then by length.
pub fn compare_values(a: &Value, b: &Value) -> Ordering {
    fn class(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Integer(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
        }
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Integer(i), Value::Real(r)) => int_float_cmp(*i, *r),
        (Value::Real(r), Value::Integer(i)) => int_float_cmp(*i, *r).reverse(),
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
        _ => class(a).cmp(&class(b)),
    }
}

/// sqlite3IntFloatCompare: exact comparison of an i64 against an f64.
fn int_float_cmp(i: i64, r: f64) -> Ordering {
    if r.is_nan() {
        // SQLite treats NaN as NULL, and every number sorts after NULL.
        return Ordering::Greater;
    }
    if r < -9223372036854775808.0 {
        return Ordering::Greater;
    }
    if r >= 9223372036854775808.0 {
        return Ordering::Less;
    }
    let y = r as i64;
    match i.cmp(&y) {
        Ordering::Equal => (i as f64).partial_cmp(&r).unwrap_or(Ordering::Equal),
        other => other,
    }
}

/// Compare two decoded records field by field, as sqlite3VdbeRecordCompare does.
/// When one record is a prefix of the other, the shorter sorts first.
pub fn compare_records(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = compare_values(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// A runtime value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(i) => write!(f, "{}", i),
            Value::Real(r) => write!(f, "{}", r),
            Value::Text(s) => write!(f, "{}", s),
            Value::Blob(b) => write!(f, "<blob {} bytes>", b.len()),
        }
    }
}

// ──────────────── SQLite Record Format ────────────────
//
// A record is: [header_size (varint)] [serial_type_1 (varint)] ... [serial_type_N (varint)] [val_1] ... [val_N]
//
// Serial types:
//   0  = NULL (0 bytes)
//   1  = 8-bit signed int (1 byte)
//   2  = 16-bit BE int (2 bytes)
//   3  = 24-bit BE int (3 bytes)
//   4  = 32-bit BE int (4 bytes)
//   5  = 48-bit BE int (6 bytes)
//   6  = 64-bit BE int (8 bytes)
//   7  = IEEE 754 float BE (8 bytes)
//   8  = integer constant 0 (0 bytes)
//   9  = integer constant 1 (0 bytes)
//   N >= 12, even = blob of (N-12)/2 bytes
//   N >= 13, odd  = text of (N-13)/2 bytes (UTF-8)

/// Compute the SQLite serial type for a Value.
fn serial_type(v: &Value) -> u64 {
    match v {
        Value::Null => 0,
        Value::Integer(i) => {
            let i = *i;
            if i == 0 {
                return 8;
            }
            if i == 1 {
                return 9;
            }
            let u = if i < 0 { (!(i as u64)) + 1 } else { i as u64 };
            if u <= 0x7F {
                1
            } else if u <= 0x7FFF {
                2
            } else if u <= 0x7F_FFFF {
                3
            } else if u <= 0x7FFF_FFFF {
                4
            } else if u <= 0x7FFF_FFFF_FFFF {
                5
            } else {
                6
            }
        }
        Value::Real(_) => 7,
        Value::Text(s) => (s.len() as u64) * 2 + 13,
        Value::Blob(b) => (b.len() as u64) * 2 + 12,
    }
}

/// Size in bytes of the body (value) for a given serial type.
fn serial_type_content_size(st: u64) -> usize {
    match st {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 => 8,
        7 => 8,
        8 => 0,
        9 => 0,
        n if n >= 12 && n % 2 == 0 => ((n - 12) / 2) as usize,
        n if n >= 13 && n % 2 == 1 => ((n - 13) / 2) as usize,
        _ => 0,
    }
}

/// Write the body bytes for a value given its serial type.
fn write_value_body(v: &Value, st: u64, out: &mut Vec<u8>) {
    match st {
        0 | 8 | 9 => {} // no body
        1 => {
            if let Value::Integer(i) = v {
                out.push(*i as u8);
            }
        }
        2 => {
            if let Value::Integer(i) = v {
                out.extend_from_slice(&(*i as i16).to_be_bytes());
            }
        }
        3 => {
            if let Value::Integer(i) = v {
                let b = (*i as i32).to_be_bytes();
                out.extend_from_slice(&b[1..4]); // last 3 bytes
            }
        }
        4 => {
            if let Value::Integer(i) = v {
                out.extend_from_slice(&(*i as i32).to_be_bytes());
            }
        }
        5 => {
            if let Value::Integer(i) = v {
                let b = (*i as i64).to_be_bytes();
                out.extend_from_slice(&b[2..8]); // last 6 bytes
            }
        }
        6 => {
            if let Value::Integer(i) = v {
                out.extend_from_slice(&i.to_be_bytes());
            }
        }
        7 => {
            if let Value::Real(r) = v {
                out.extend_from_slice(&r.to_be_bytes());
            }
        }
        n if n >= 12 && n % 2 == 0 => {
            if let Value::Blob(b) = v {
                out.extend_from_slice(b);
            }
        }
        n if n >= 13 && n % 2 == 1 => {
            if let Value::Text(s) = v {
                out.extend_from_slice(s.as_bytes());
            }
        }
        _ => {}
    }
}

/// Read a Value back from a body slice given its serial type.
fn read_value_body(st: u64, data: &[u8]) -> Value {
    match st {
        0 => Value::Null,
        1 => Value::Integer(data[0] as i8 as i64),
        2 => Value::Integer(i16::from_be_bytes([data[0], data[1]]) as i64),
        3 => {
            // Sign-extend 24-bit
            let sign_ext = if data[0] & 0x80 != 0 { 0xFF } else { 0x00 };
            let v = i32::from_be_bytes([sign_ext, data[0], data[1], data[2]]);
            Value::Integer(v as i64)
        }
        4 => Value::Integer(i32::from_be_bytes([data[0], data[1], data[2], data[3]]) as i64),
        5 => {
            let sign_ext = if data[0] & 0x80 != 0 { 0xFF } else { 0x00 };
            let v = i64::from_be_bytes([
                sign_ext, sign_ext, data[0], data[1], data[2], data[3], data[4], data[5],
            ]);
            Value::Integer(v)
        }
        6 => {
            let v = i64::from_be_bytes([
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]);
            Value::Integer(v)
        }
        7 => {
            let v = f64::from_be_bytes([
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]);
            Value::Real(v)
        }
        8 => Value::Integer(0),
        9 => Value::Integer(1),
        n if n >= 12 && n % 2 == 0 => {
            let len = ((n - 12) / 2) as usize;
            Value::Blob(data[..len].to_vec())
        }
        n if n >= 13 && n % 2 == 1 => {
            let len = ((n - 13) / 2) as usize;
            Value::Text(String::from_utf8_lossy(&data[..len]).to_string())
        }
        _ => Value::Null,
    }
}

/// A row is a list of values.
pub type Row = Vec<Value>;

/// Serialize a row as a SQLite Record Format payload.
pub fn serialize_row(row: &[Value]) -> Vec<u8> {
    // 1. Compute serial types and build the header.
    let types: Vec<u64> = row.iter().map(|v| serial_type(v)).collect();

    // Build header content (serial types as varints).
    let mut header_body = Vec::new();
    let mut vbuf = [0u8; 9];
    for &st in &types {
        let n = put_varint(&mut vbuf, st);
        header_body.extend_from_slice(&vbuf[..n]);
    }

    // Header size includes itself (the varint encoding of the header size).
    // We need to figure out how many bytes the header-size varint takes.
    let mut header_size = header_body.len() + 1; // assume 1-byte varint
    let n = put_varint(&mut vbuf, header_size as u64);
    if n > 1 {
        header_size = header_body.len() + n;
    }

    let mut out = Vec::new();
    let n = put_varint(&mut vbuf, header_size as u64);
    out.extend_from_slice(&vbuf[..n]);
    out.extend_from_slice(&header_body);

    // 2. Write value bodies.
    for (i, v) in row.iter().enumerate() {
        write_value_body(v, types[i], &mut out);
    }

    out
}

/// Deserialize a row from SQLite Record Format payload.
pub fn deserialize_row(data: &[u8]) -> io::Result<Row> {
    if data.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty record"));
    }

    // Read header size.
    let (header_size, hs_len) = get_varint(data);
    let header_size = header_size as usize;

    // Read serial types from the header.
    let mut types = Vec::new();
    let mut hoff = hs_len;
    while hoff < header_size {
        let (st, st_len) = get_varint(&data[hoff..]);
        types.push(st);
        hoff += st_len;
    }

    // Read values from the body.
    let mut body_off = header_size;
    let mut row = Vec::with_capacity(types.len());
    for &st in &types {
        let sz = serial_type_content_size(st);
        if body_off + sz > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "record body truncated at offset {} (need {} more bytes, serial type {})",
                    body_off, sz, st
                ),
            ));
        }
        let v = read_value_body(st, &data[body_off..body_off + sz]);
        row.push(v);
        body_off += sz;
    }

    Ok(row)
}

/// Serialize a Table definition (for storage in the catalog B-Tree).
pub fn serialize_table(table: &Table) -> Vec<u8> {
    let mut out = Vec::new();
    // name length + name
    let name_bytes = table.name.as_bytes();
    out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(name_bytes);
    // root_page
    out.extend_from_slice(&table.root_page.to_le_bytes());
    // column count
    out.extend_from_slice(&(table.columns.len() as u16).to_le_bytes());
    for col in &table.columns {
        let cn = col.name.as_bytes();
        out.extend_from_slice(&(cn.len() as u16).to_le_bytes());
        out.extend_from_slice(cn);
        let dt = match &col.data_type {
            DataType::Integer => 0u8,
            DataType::Text => 1u8,
            DataType::Real => 2u8,
            DataType::Blob => 3u8,
        };
        out.push(dt);
        // Flags byte: bit0 = primary_key, bit1 = autoincrement, bit2 = not_null, bit3 = unique
        let mut flags: u8 = 0;
        if col.is_primary_key {
            flags |= 0x01;
        }
        if col.is_autoincrement {
            flags |= 0x02;
        }
        if col.is_not_null {
            flags |= 0x04;
        }
        if col.is_unique {
            flags |= 0x08;
        }
        if col.default_text.is_some() {
            flags |= 0x10;
        }
        out.push(flags);
        if let Some(ref dt_str) = col.default_text {
            let db = dt_str.as_bytes();
            out.extend_from_slice(&(db.len() as u16).to_le_bytes());
            out.extend_from_slice(db);
        }
    }
    out
}

/// Deserialize a Table definition.
pub fn deserialize_table(data: &[u8]) -> io::Result<Table> {
    let mut off = 0;
    if data.len() < 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short table"));
    }
    let name_len = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
    off += 2;
    let name = String::from_utf8_lossy(&data[off..off + name_len]).to_string();
    off += name_len;

    let root_page = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
    off += 4;

    let col_count = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
    off += 2;

    let mut columns = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let cn_len = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
        off += 2;
        let cn = String::from_utf8_lossy(&data[off..off + cn_len]).to_string();
        off += cn_len;
        let dt = match data[off] {
            0 => DataType::Integer,
            1 => DataType::Text,
            2 => DataType::Real,
            3 => DataType::Blob,
            _ => DataType::Blob,
        };
        off += 1;
        let flags = data[off];
        off += 1;
        let is_pk = (flags & 0x01) != 0;
        let is_autoinc = (flags & 0x02) != 0;
        let is_not_null = (flags & 0x04) != 0;
        let is_unique = (flags & 0x08) != 0;
        let has_default = (flags & 0x10) != 0;
        let default_text = if has_default {
            let dl = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
            off += 2;
            let ds = String::from_utf8_lossy(&data[off..off + dl]).to_string();
            off += dl;
            Some(ds)
        } else {
            None
        };
        columns.push(Column {
            name: cn,
            data_type: dt,
            is_primary_key: is_pk,
            is_autoincrement: is_autoinc,
            is_not_null,
            is_unique,
            is_rowid_alias: false,
            default_text,
        });
    }

    Ok(Table {
        name,
        columns,
        root_page,
        sql: String::new(),
        indexes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_roundtrip() {
        // Test individual values through record format roundtrip
        let test_cases: Vec<Vec<Value>> = vec![
            vec![Value::Null],
            vec![Value::Integer(0)],
            vec![Value::Integer(1)],
            vec![Value::Integer(42)],
            vec![Value::Integer(-100)],
            vec![Value::Integer(100000)],
            vec![Value::Integer(i64::MAX)],
            vec![Value::Integer(i64::MIN)],
            vec![Value::Real(3.14)],
            vec![Value::Text("hello world".to_string())],
            vec![Value::Text("".to_string())],
            vec![Value::Blob(vec![1, 2, 3, 4])],
            vec![Value::Blob(vec![])],
        ];
        for row in &test_cases {
            let bytes = serialize_row(row);
            let row2 = deserialize_row(&bytes).unwrap();
            assert_eq!(row, &row2, "Failed roundtrip for {:?}", row);
        }
    }

    #[test]
    fn test_row_roundtrip() {
        let row = vec![
            Value::Integer(1),
            Value::Text("Alice".to_string()),
            Value::Real(3.14),
        ];
        let bytes = serialize_row(&row);
        let row2 = deserialize_row(&bytes).unwrap();
        assert_eq!(row, row2);
    }

    #[test]
    fn test_table_roundtrip() {
        let t = Table {
            name: "users".to_string(),
            columns: vec![
                Column {
                    name: "id".to_string(),
                    data_type: DataType::Integer,
                    is_primary_key: true,
                    is_autoincrement: true,
                    is_not_null: false,
                    is_unique: false,
                    is_rowid_alias: true,
                    default_text: None,
                },
                Column {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    is_primary_key: false,
                    is_autoincrement: false,
                    is_not_null: false,
                    is_unique: false,
                    is_rowid_alias: false,
                    default_text: Some("unnamed".to_string()),
                },
            ],
            root_page: 42,
            sql: "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT DEFAULT 'unnamed');".to_string(),
            indexes: Vec::new(),
        };
        let bytes = serialize_table(&t);
        let t2 = deserialize_table(&bytes).unwrap();
        assert_eq!(t.name, t2.name);
        assert_eq!(t.root_page, t2.root_page);
        assert_eq!(t.columns.len(), t2.columns.len());
        assert_eq!(t.columns[0].name, t2.columns[0].name);
        assert_eq!(t.columns[1].data_type, t2.columns[1].data_type);
    }

    #[test]
    fn test_compare_values_sqlite_order() {
        use Value::*;
        // Type classes: NULL < numbers < text < blob.
        let ordered = [
            Null,
            Integer(i64::MIN),
            Real(-1.5),
            Integer(-1),
            Integer(0),
            Real(0.5),
            Integer(1),
            Real(1e300),
            Text(String::new()),
            Text("A".to_string()),
            Text("a".to_string()),
            Text("ab".to_string()),
            Text("b".to_string()),
            Blob(vec![]),
            Blob(vec![0]),
            Blob(vec![0, 0]),
            Blob(vec![1]),
        ];
        for (i, a) in ordered.iter().enumerate() {
            for (j, b) in ordered.iter().enumerate() {
                assert_eq!(compare_values(a, b), i.cmp(&j), "{:?} vs {:?}", a, b);
            }
        }
        // Integers and reals compare by value.
        assert_eq!(compare_values(&Integer(5), &Real(5.0)), Ordering::Equal);
        assert_eq!(compare_values(&Real(5.0), &Integer(5)), Ordering::Equal);
        assert_eq!(
            compare_values(&Integer(i64::MAX), &Real(9.3e18)),
            Ordering::Less
        );
        assert_eq!(
            compare_values(&Integer(9007199254740993), &Real(9007199254740992.0)),
            Ordering::Greater
        );
        // Text compares by bytes, not by numeric value.
        assert_eq!(
            compare_values(&Text("10".into()), &Text("9".into())),
            Ordering::Less
        );
        // Two NULLs are equal in a record comparison.
        assert_eq!(
            compare_records(&[Null, Integer(1)], &[Null, Integer(2)]),
            Ordering::Less
        );
    }

    #[test]
    fn test_autoindex_columns_follow_sqlite_rules() {
        let col = |name: &str, pk: bool, alias: bool, unique: bool| Column {
            name: name.to_string(),
            data_type: DataType::Text,
            is_primary_key: pk,
            is_autoincrement: false,
            is_not_null: false,
            is_unique: unique,
            is_rowid_alias: alias,
            default_text: None,
        };
        // CREATE TABLE t (id INTEGER PRIMARY KEY, a UNIQUE, b, c TEXT PRIMARY KEY UNIQUE)
        let cols = vec![
            col("id", true, true, false),
            col("a", false, false, true),
            col("b", false, false, false),
            col("c", true, false, true),
        ];
        assert_eq!(autoindex_columns(&cols), vec![1, 3]);
        // INTEGER PRIMARY KEY UNIQUE still gets an index for the UNIQUE.
        assert_eq!(autoindex_columns(&[col("id", true, true, true)]), vec![0]);
        assert_eq!(autoindex_name("t", 2), "sqlite_autoindex_t_2");
    }

    #[test]
    fn test_record_format_multi_column() {
        let row = vec![
            Value::Null,
            Value::Integer(42),
            Value::Text("hello".to_string()),
            Value::Real(2.718),
            Value::Blob(vec![0xDE, 0xAD]),
            Value::Integer(0),
            Value::Integer(1),
        ];
        let bytes = serialize_row(&row);
        let row2 = deserialize_row(&bytes).unwrap();
        assert_eq!(row, row2);
    }
}
