//! Index B-trees in SQLite's on-disk format.
//!
//! An index B-tree holds records only, here `(column value, rowid)`, sorted the
//! way SQLite compares records (see [`compare_records`]). Unlike a table B-tree,
//! it is a true B-tree: every entry is stored exactly once, and the cells of an
//! interior page are real entries that separate their child subtrees.
//!
//! Page formats (no 100-byte offset: an index root is never page 1):
//!
//! ```text
//! leaf     0x0A  header 8 bytes   cell = payload_len varint, payload
//! interior 0x02  header 12 bytes  cell = left child u32, payload_len varint, payload
//! header: [0] type  [1..3] first freeblock  [3..5] cell count
//!         [5..7] cell content start  [7] fragmented bytes  [8..12] right child
//! ```
//!
//! Every page is decoded into a [`Node`], changed in memory, and written back
//! packed: cells sit contiguously at the end of the page, with no freeblocks and
//! a fragment count of zero. That keeps the header, cell pointers, freeblock
//! chain and fragment count consistent with what SQLite's btreeInitPage and
//! `PRAGMA integrity_check` verify.
//!
//! The root page never moves: when it splits, its contents move into two new
//! children, and when an interior root is left with a single child, that child
//! is pulled up into it. Non-root pages always keep at least one cell, since
//! SQLite treats an empty non-root page as corruption.

use std::cmp::Ordering;
use std::io;

use crate::btree::PageStore;
use crate::pager::PAGE_SIZE;
use crate::schema::{Value, compare_records, compare_values, deserialize_row};
use crate::varint::{get_varint, put_varint};

const LEAF: u8 = 0x0A;
const INTERIOR: u8 = 0x02;
const LEAF_HEADER: usize = 8;
const INTERIOR_HEADER: usize = 12;
const CELL_PTR: usize = 2;

/// The largest payload SQLite keeps entirely on an index page (larger ones spill to
/// overflow pages, which this engine does not write): ((U-12)*64/255) - 23.
pub const MAX_LOCAL_PAYLOAD: usize = (PAGE_SIZE - 12) * 64 / 255 - 23;

/// A non-root page whose encoded size drops below this is merged with, or
/// refilled from, a sibling.
const MIN_FILL: usize = PAGE_SIZE / 3;

/// One decoded index page.
#[derive(Debug, Clone, Default)]
struct Node {
    /// Record payloads in key order.
    cells: Vec<Vec<u8>>,
    /// Child page numbers: empty for a leaf; for an interior page, `children[i]` is
    /// the left child of `cells[i]` and the last entry is the right-most child.
    children: Vec<u32>,
}

impl Node {
    fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    fn header_size(&self) -> usize {
        if self.is_leaf() {
            LEAF_HEADER
        } else {
            INTERIOR_HEADER
        }
    }

    /// Bytes this node needs on a page: header + cell pointers + cells.
    fn byte_size(&self) -> usize {
        let interior = !self.is_leaf();
        self.header_size()
            + self
                .cells
                .iter()
                .map(|c| CELL_PTR + cell_size(c, interior))
                .sum::<usize>()
    }

    fn fits(&self) -> bool {
        self.byte_size() <= PAGE_SIZE
    }
}

fn varint_len(v: u64) -> usize {
    let mut buf = [0u8; 9];
    put_varint(&mut buf, v)
}

fn cell_size(payload: &[u8], interior: bool) -> usize {
    (if interior { 4 } else { 0 }) + varint_len(payload.len() as u64) + payload.len()
}

fn read_u16(page: &[u8], off: usize) -> usize {
    u16::from_be_bytes([page[off], page[off + 1]]) as usize
}

fn read_u32(page: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]])
}

fn corrupt(page_num: u32, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("index page {}: {}", page_num, what),
    )
}

fn decode(page: &[u8; PAGE_SIZE], page_num: u32) -> io::Result<Node> {
    let leaf = match page[0] {
        LEAF => true,
        INTERIOR => false,
        t => {
            return Err(corrupt(
                page_num,
                &format!("unexpected page type {:#04x}", t),
            ));
        }
    };
    let header = if leaf { LEAF_HEADER } else { INTERIOR_HEADER };
    let n = read_u16(page, 3);
    let ptrs_end = header + n * CELL_PTR;
    if ptrs_end > PAGE_SIZE {
        return Err(corrupt(page_num, "cell count too large"));
    }
    let mut cells = Vec::with_capacity(n);
    let mut children = Vec::with_capacity(if leaf { 0 } else { n + 1 });
    for i in 0..n {
        let mut off = read_u16(page, header + i * CELL_PTR);
        if off < ptrs_end || off + if leaf { 1 } else { 5 } > PAGE_SIZE {
            return Err(corrupt(page_num, "cell pointer out of range"));
        }
        if !leaf {
            children.push(read_u32(page, off));
            off += 4;
        }
        let (len, len_bytes) = get_varint(&page[off..]);
        let start = off + len_bytes;
        let end = start.saturating_add(len as usize);
        if end > PAGE_SIZE {
            return Err(corrupt(page_num, "cell extends past end of page"));
        }
        cells.push(page[start..end].to_vec());
    }
    if !leaf {
        children.push(read_u32(page, 8));
    }
    Ok(Node { cells, children })
}

fn encode(node: &Node) -> [u8; PAGE_SIZE] {
    debug_assert!(node.fits());
    let mut page = [0u8; PAGE_SIZE];
    let leaf = node.is_leaf();
    let header = node.header_size();
    page[0] = if leaf { LEAF } else { INTERIOR };
    // [1..3] first freeblock = 0 and [7] fragmented bytes = 0: the page is packed.
    page[3..5].copy_from_slice(&(node.cells.len() as u16).to_be_bytes());
    let mut top = PAGE_SIZE;
    for (i, cell) in node.cells.iter().enumerate() {
        top -= cell_size(cell, !leaf);
        let mut off = top;
        if !leaf {
            page[off..off + 4].copy_from_slice(&node.children[i].to_be_bytes());
            off += 4;
        }
        off += put_varint(&mut page[off..], cell.len() as u64);
        page[off..off + cell.len()].copy_from_slice(cell);
        let ptr = header + i * CELL_PTR;
        page[ptr..ptr + 2].copy_from_slice(&(top as u16).to_be_bytes());
    }
    // Start of the cell content area (PAGE_SIZE for an empty page; fits in a u16).
    page[5..7].copy_from_slice(&(top as u16).to_be_bytes());
    if !leaf {
        let right = *node
            .children
            .last()
            .expect("interior node has a right child");
        page[8..12].copy_from_slice(&right.to_be_bytes());
    }
    page
}

fn read_node(store: &mut dyn PageStore, page_num: u32) -> io::Result<Node> {
    let page = store.read_page(page_num)?;
    decode(&page, page_num)
}

fn write_node(store: &mut dyn PageStore, page_num: u32, node: &Node) -> io::Result<()> {
    if !node.fits() {
        return Err(corrupt(page_num, "node does not fit on a page"));
    }
    store.write_page(page_num, encode(node))
}

/// Initialize `page` as an empty index leaf, the state of a new index root.
pub fn init_root_page(page: &mut [u8; PAGE_SIZE]) {
    *page = encode(&Node::default());
}

/// Binary-search `node` for `key`: `Ok(i)` if `cells[i]` equals it, otherwise
/// `Err(i)` with the index of the first cell greater than it.
fn search_node(node: &Node, key: &[Value]) -> io::Result<Result<usize, usize>> {
    let (mut lo, mut hi) = (0, node.cells.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let rec = deserialize_row(&node.cells[mid])?;
        match compare_records(&rec, key) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Ok(Ok(mid)),
        }
    }
    Ok(Err(lo))
}

/// One page on the path from the root to the page being changed.
struct Level {
    page_num: u32,
    node: Node,
    /// Index into `node.children` of the next page down the path.
    child: usize,
    /// Whether `node` differs from what is on disk.
    modified: bool,
}

/// Insert `record` into the index rooted at `root`. The root page never changes.
pub fn insert(store: &mut dyn PageStore, root: u32, record: &[u8]) -> io::Result<()> {
    if record.len() > MAX_LOCAL_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "index key of {} bytes is too large (max {}): overflow pages are not supported",
                record.len(),
                MAX_LOCAL_PAYLOAD
            ),
        ));
    }
    let key = deserialize_row(record)?;
    let mut path: Vec<Level> = Vec::new();
    let mut page_num = root;
    loop {
        let mut node = read_node(store, page_num)?;
        let pos = match search_node(&node, &key)? {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "index entry already exists",
                ));
            }
            Err(pos) => pos,
        };
        if node.is_leaf() {
            node.cells.insert(pos, record.to_vec());
            path.push(Level {
                page_num,
                node,
                child: 0,
                modified: true,
            });
            break;
        }
        let next = node.children[pos];
        path.push(Level {
            page_num,
            node,
            child: pos,
            modified: false,
        });
        page_num = next;
    }
    balance(store, path)
}

/// Delete `record` from the index rooted at `root`. Returns false if it was absent.
pub fn delete(store: &mut dyn PageStore, root: u32, record: &[u8]) -> io::Result<bool> {
    let key = deserialize_row(record)?;
    let mut path: Vec<Level> = Vec::new();
    let mut page_num = root;
    loop {
        let mut node = read_node(store, page_num)?;
        match search_node(&node, &key)? {
            Ok(i) if node.is_leaf() => {
                node.cells.remove(i);
                path.push(Level {
                    page_num,
                    node,
                    child: 0,
                    modified: true,
                });
                break;
            }
            Ok(i) => {
                // The entry is on an interior page. Replace it with its predecessor,
                // the last entry of the right-most leaf of its left subtree, and remove
                // that entry from the leaf instead (as sqlite3BtreeDelete does).
                let at = path.len();
                let mut child = node.children[i];
                path.push(Level {
                    page_num,
                    node,
                    child: i,
                    modified: true,
                });
                loop {
                    let mut n = read_node(store, child)?;
                    if n.is_leaf() {
                        let pred = n
                            .cells
                            .pop()
                            .ok_or_else(|| corrupt(child, "empty non-root page"))?;
                        path[at].node.cells[i] = pred;
                        path.push(Level {
                            page_num: child,
                            node: n,
                            child: 0,
                            modified: true,
                        });
                        break;
                    }
                    let last = n.children.len() - 1;
                    let next = n.children[last];
                    path.push(Level {
                        page_num: child,
                        node: n,
                        child: last,
                        modified: false,
                    });
                    child = next;
                }
                break;
            }
            Err(pos) => {
                if node.is_leaf() {
                    return Ok(false);
                }
                let next = node.children[pos];
                path.push(Level {
                    page_num,
                    node,
                    child: pos,
                    modified: false,
                });
                page_num = next;
            }
        }
    }
    balance(store, path)?;
    Ok(true)
}

/// Split `node` around one of its cells so that both halves fit on a page and
/// keep at least one cell. Returns (left, divider, right).
fn split(node: Node) -> io::Result<(Node, Vec<u8>, Node)> {
    let n = node.cells.len();
    let interior = !node.is_leaf();
    if n < 3 {
        return Err(io::Error::other("index page split needs at least 3 cells"));
    }
    let sizes: Vec<usize> = node
        .cells
        .iter()
        .map(|c| CELL_PTR + cell_size(c, interior))
        .collect();
    let total: usize = sizes.iter().sum();
    // Pick the divider that makes the larger half as small as possible.
    let mut best = (usize::MAX, 0);
    let mut left = 0;
    for d in 1..n - 1 {
        left += sizes[d - 1];
        let worst = left.max(total - left - sizes[d]);
        if worst < best.0 {
            best = (worst, d);
        }
    }
    let (worst, d) = best;
    if node.header_size() + worst > PAGE_SIZE {
        return Err(io::Error::other("index page split cannot fit both halves"));
    }
    let Node {
        mut cells,
        mut children,
    } = node;
    let right_cells = cells.split_off(d + 1);
    let divider = cells.pop().expect("divider cell");
    let right_children = if interior {
        children.split_off(d + 1)
    } else {
        Vec::new()
    };
    Ok((
        Node { cells, children },
        divider,
        Node {
            cells: right_cells,
            children: right_children,
        },
    ))
}

/// Write back every modified page on `path`, from the bottom up, splitting pages
/// that overflow and merging or refilling non-root pages that run low.
fn balance(store: &mut dyn PageStore, mut path: Vec<Level>) -> io::Result<()> {
    for level in (1..path.len()).rev() {
        let (upper, lower) = path.split_at_mut(level);
        let parent = upper.last_mut().expect("parent level");
        let cur = &mut lower[0];
        if !cur.modified {
            continue;
        }
        let ci = parent.child;
        debug_assert_eq!(parent.node.children[ci], cur.page_num);

        if !cur.node.fits() {
            // Overflow: keep the left half here and move the right half to a new
            // page; the divider goes up into the parent.
            let (left, divider, right) = split(std::mem::take(&mut cur.node))?;
            write_node(store, cur.page_num, &left)?;
            let new_page = store.allocate_page()?;
            write_node(store, new_page, &right)?;
            parent.node.cells.insert(ci, divider);
            parent.node.children.insert(ci + 1, new_page);
            parent.modified = true;
        } else if cur.node.byte_size() < MIN_FILL {
            // Underflow: combine with the left sibling (or the right one for the
            // first child) and the divider between them. The parent always has a
            // cell here: on-disk interior pages have one, and only this level
            // removes cells from the parent.
            let li = if ci > 0 { ci - 1 } else { ci };
            let (left_pg, right_pg) = (parent.node.children[li], parent.node.children[li + 1]);
            let this = std::mem::take(&mut cur.node);
            let (left, right) = if li == ci {
                (this, read_node(store, right_pg)?)
            } else {
                (read_node(store, left_pg)?, this)
            };
            let mut merged = left;
            merged.cells.push(parent.node.cells[li].clone());
            merged.cells.extend(right.cells);
            merged.children.extend(right.children);
            if merged.fits() {
                write_node(store, left_pg, &merged)?;
                store.free_page(right_pg)?;
                parent.node.cells.remove(li);
                parent.node.children.remove(li + 1);
            } else {
                let (l, divider, r) = split(merged)?;
                write_node(store, left_pg, &l)?;
                write_node(store, right_pg, &r)?;
                parent.node.cells[li] = divider;
            }
            parent.modified = true;
        } else {
            write_node(store, cur.page_num, &cur.node)?;
        }
    }

    let root = &mut path[0];
    if !root.modified {
        return Ok(());
    }
    if !root.node.fits() {
        // Root overflow: move both halves into new children so the root page
        // number (recorded in sqlite_schema) stays the same.
        let (left, divider, right) = split(std::mem::take(&mut root.node))?;
        let left_pg = store.allocate_page()?;
        write_node(store, left_pg, &left)?;
        let right_pg = store.allocate_page()?;
        write_node(store, right_pg, &right)?;
        root.node = Node {
            cells: vec![divider],
            children: vec![left_pg, right_pg],
        };
    } else if !root.node.is_leaf() && root.node.cells.is_empty() {
        // An interior root with a single child: pull the child up into the root.
        let only = root.node.children[0];
        root.node = read_node(store, only)?;
        store.free_page(only)?;
    }
    write_node(store, root.page_num, &root.node)
}

/// Find an entry whose first field (the indexed value) equals `value` and return
/// its rowid. In a UNIQUE index there is at most one such entry.
pub fn find_rowid(store: &mut dyn PageStore, root: u32, value: &Value) -> io::Result<Option<i64>> {
    let mut page_num = root;
    loop {
        let node = read_node(store, page_num)?;
        // Lower bound: the first cell whose indexed value is >= `value`.
        let (mut lo, mut hi) = (0, node.cells.len());
        let mut hit = None;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let rec = deserialize_row(&node.cells[mid])?;
            let first = rec
                .first()
                .ok_or_else(|| corrupt(page_num, "empty record"))?;
            match compare_values(first, value) {
                Ordering::Less => lo = mid + 1,
                Ordering::Equal => {
                    hit = Some(rec);
                    break;
                }
                Ordering::Greater => hi = mid,
            }
        }
        if let Some(rec) = hit {
            return match rec.last() {
                Some(Value::Integer(rowid)) => Ok(Some(*rowid)),
                _ => Err(corrupt(page_num, "index record without a rowid")),
            };
        }
        if node.is_leaf() {
            return Ok(None);
        }
        page_num = node.children[lo];
    }
}

/// All records of the index rooted at `root`, in key order.
pub fn scan(store: &mut dyn PageStore, root: u32) -> io::Result<Vec<Vec<u8>>> {
    fn walk(store: &mut dyn PageStore, page_num: u32, out: &mut Vec<Vec<u8>>) -> io::Result<()> {
        let node = read_node(store, page_num)?;
        if node.is_leaf() {
            out.extend(node.cells);
            return Ok(());
        }
        for (child, cell) in node.children.iter().zip(node.cells) {
            walk(store, *child, out)?;
            out.push(cell);
        }
        walk(store, *node.children.last().expect("right child"), out)
    }
    let mut out = Vec::new();
    walk(store, root, &mut out)?;
    Ok(out)
}

/// Every page number in the index rooted at `root` (root included).
pub fn collect_pages(store: &mut dyn PageStore, root: u32) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(page_num) = stack.pop() {
        out.push(page_num);
        stack.extend(read_node(store, page_num)?.children);
    }
    Ok(out)
}

/// Check the structural rules SQLite relies on: keys strictly increasing in order,
/// every leaf at the same depth, no empty page except a leaf root, and every page
/// decodable. Returns the number of entries.
pub fn verify(store: &mut dyn PageStore, root: u32) -> Result<usize, String> {
    struct State {
        leaf_depth: Option<usize>,
        prev: Option<Vec<Value>>,
        count: usize,
    }
    fn visit(state: &mut State, page_num: u32, cell: &[u8]) -> Result<(), String> {
        let rec = deserialize_row(cell).map_err(|e| e.to_string())?;
        if state
            .prev
            .as_ref()
            .is_some_and(|p| compare_records(p, &rec) != Ordering::Less)
        {
            return Err(format!("page {}: keys out of order", page_num));
        }
        state.prev = Some(rec);
        state.count += 1;
        Ok(())
    }
    fn walk(
        store: &mut dyn PageStore,
        page_num: u32,
        is_root: bool,
        depth: usize,
        state: &mut State,
    ) -> Result<(), String> {
        let node = read_node(store, page_num).map_err(|e| e.to_string())?;
        if node.cells.is_empty() && !(is_root && node.is_leaf()) {
            return Err(format!("page {} is empty", page_num));
        }
        if node.is_leaf() {
            match state.leaf_depth {
                Some(d) if d != depth => {
                    return Err(format!("page {}: leaf depth differs", page_num));
                }
                _ => state.leaf_depth = Some(depth),
            }
            for cell in &node.cells {
                visit(state, page_num, cell)?;
            }
            return Ok(());
        }
        for (child, cell) in node.children.iter().zip(&node.cells) {
            walk(store, *child, false, depth + 1, state)?;
            visit(state, page_num, cell)?;
        }
        let right = *node.children.last().expect("right child");
        walk(store, right, false, depth + 1, state)
    }
    let mut state = State {
        leaf_depth: None,
        prev: None,
        count: 0,
    };
    walk(store, root, true, 0, &mut state)?;
    Ok(state.count)
}

// ───────────────────────────── Tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::index_record;
    use std::collections::{BTreeSet, HashMap};

    struct MemStore {
        pages: HashMap<u32, [u8; PAGE_SIZE]>,
        free: Vec<u32>,
        next_page: u32,
    }

    impl MemStore {
        fn new() -> Self {
            MemStore {
                pages: HashMap::new(),
                free: Vec::new(),
                next_page: 2,
            }
        }
        fn new_index(&mut self) -> u32 {
            let root = self.allocate_page().unwrap();
            let mut page = [0u8; PAGE_SIZE];
            init_root_page(&mut page);
            self.write_page(root, page).unwrap();
            root
        }
    }

    impl PageStore for MemStore {
        fn read_page(&mut self, page_num: u32) -> io::Result<[u8; PAGE_SIZE]> {
            self.pages
                .get(&page_num)
                .copied()
                .ok_or_else(|| io::Error::other(format!("read of unallocated page {}", page_num)))
        }
        fn write_page(&mut self, page_num: u32, data: [u8; PAGE_SIZE]) -> io::Result<()> {
            self.pages.insert(page_num, data);
            Ok(())
        }
        fn allocate_page(&mut self) -> io::Result<u32> {
            if let Some(p) = self.free.pop() {
                return Ok(p);
            }
            let p = self.next_page;
            self.next_page += 1;
            Ok(p)
        }
        fn free_page(&mut self, page_num: u32) -> io::Result<()> {
            assert!(
                !self.free.contains(&page_num),
                "double free of page {}",
                page_num
            );
            self.pages.remove(&page_num);
            self.free.push(page_num);
            Ok(())
        }
    }

    /// Tiny deterministic PRNG (xorshift64*), so tests need no dependencies.
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

    fn text_key(i: u64, pad: usize) -> Value {
        Value::Text(format!("{:08}{}", i, "x".repeat(pad)))
    }

    fn check_tree(store: &mut MemStore, root: u32, expected: usize) {
        let n = verify(store, root).unwrap_or_else(|e| panic!("invalid index: {}", e));
        assert_eq!(n, expected);
        // Every allocated page is either in the tree or on the free list.
        let mut used: BTreeSet<u32> = collect_pages(store, root).unwrap().into_iter().collect();
        assert_eq!(
            used.len(),
            collect_pages(store, root).unwrap().len(),
            "page used twice"
        );
        used.extend(store.free.iter().copied());
        let all: BTreeSet<u32> = (2..store.next_page).collect();
        assert_eq!(used, all, "pages leaked");
    }

    #[test]
    fn test_empty_root_is_valid_leaf() {
        let mut store = MemStore::new();
        let root = store.new_index();
        let page = store.read_page(root).unwrap();
        assert_eq!(page[0], 0x0A);
        assert_eq!(read_u16(&page, 3), 0);
        assert_eq!(read_u16(&page, 5), PAGE_SIZE);
        check_tree(&mut store, root, 0);
        assert_eq!(
            find_rowid(&mut store, root, &Value::Integer(1)).unwrap(),
            None
        );
    }

    #[test]
    fn test_page_layout_matches_sqlite() {
        // Same bytes sqlite3 writes for ('a', 1) and ('b', 2) in a 4 KiB index leaf.
        let mut store = MemStore::new();
        let root = store.new_index();
        insert(&mut store, root, &index_record(&Value::Text("b".into()), 2)).unwrap();
        insert(&mut store, root, &index_record(&Value::Text("a".into()), 1)).unwrap();
        let page = store.read_page(root).unwrap();
        assert_eq!(
            &page[..12],
            &[0x0a, 0, 0, 0, 2, 0x0f, 0xf5, 0, 0x0f, 0xfb, 0x0f, 0xf5]
        );
        assert_eq!(
            &page[PAGE_SIZE - 11..],
            &[
                0x05, 0x03, 0x0f, 0x01, 0x62, 0x02, 0x04, 0x03, 0x0f, 0x09, 0x61
            ]
        );
    }

    #[test]
    fn test_insert_find_delete_many_random() {
        let mut store = MemStore::new();
        let root = store.new_index();
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let mut ids: Vec<u64> = (0..3000).collect();
        for i in (1..ids.len()).rev() {
            ids.swap(i, rng.below(i + 1));
        }
        for (n, &i) in ids.iter().enumerate() {
            insert(
                &mut store,
                root,
                &index_record(&text_key(i, (i % 50) as usize), i as i64),
            )
            .unwrap();
            if n % 500 == 0 {
                check_tree(&mut store, root, n + 1);
            }
        }
        check_tree(&mut store, root, ids.len());
        assert!(
            !read_node(&mut store, root).unwrap().is_leaf(),
            "tree should have grown"
        );

        for &i in &ids[..100] {
            let key = text_key(i, (i % 50) as usize);
            assert_eq!(find_rowid(&mut store, root, &key).unwrap(), Some(i as i64));
        }
        assert_eq!(
            find_rowid(&mut store, root, &Value::Text("nope".into())).unwrap(),
            None
        );

        // Duplicates of a full (value, rowid) record are refused.
        let dup = index_record(&text_key(ids[0], (ids[0] % 50) as usize), ids[0] as i64);
        assert!(insert(&mut store, root, &dup).is_err());

        // Delete everything in another random order, checking structure as we go.
        for i in (1..ids.len()).rev() {
            ids.swap(i, rng.below(i + 1));
        }
        for (n, &i) in ids.iter().enumerate() {
            let rec = index_record(&text_key(i, (i % 50) as usize), i as i64);
            assert!(delete(&mut store, root, &rec).unwrap(), "missing {}", i);
            assert!(!delete(&mut store, root, &rec).unwrap());
            if n % 250 == 0 {
                check_tree(&mut store, root, ids.len() - n - 1);
            }
        }
        check_tree(&mut store, root, 0);
        assert!(read_node(&mut store, root).unwrap().is_leaf());
    }

    #[test]
    fn test_large_keys_force_deep_trees() {
        // ~900-byte keys: four per page, so interior pages split and merge often.
        let mut store = MemStore::new();
        let root = store.new_index();
        let mut rng = Rng(42);
        let mut live: Vec<u64> = Vec::new();
        for step in 0..4000u64 {
            if live.is_empty() || rng.below(3) != 0 {
                let i = rng.next() >> 8;
                let pad = 850 + (i % 100) as usize;
                insert(&mut store, root, &index_record(&text_key(i, pad), i as i64)).unwrap();
                live.push(i);
            } else {
                let i = live.swap_remove(rng.below(live.len()));
                let pad = 850 + (i % 100) as usize;
                assert!(
                    delete(&mut store, root, &index_record(&text_key(i, pad), i as i64)).unwrap()
                );
            }
            if step % 200 == 0 {
                check_tree(&mut store, root, live.len());
            }
        }
        check_tree(&mut store, root, live.len());
        let mut depth = 0;
        let mut node = read_node(&mut store, root).unwrap();
        while !node.is_leaf() {
            depth += 1;
            node = read_node(&mut store, node.children[0]).unwrap();
        }
        assert!(depth >= 2, "expected interior levels, got depth {}", depth);
    }

    #[test]
    fn test_mixed_types_sort_like_sqlite() {
        let mut store = MemStore::new();
        let root = store.new_index();
        let values = vec![
            Value::Blob(vec![1, 2]),
            Value::Text("abc".into()),
            Value::Real(2.5),
            Value::Null,
            Value::Integer(-7),
            Value::Null,
            Value::Integer(3),
            Value::Text("ab".into()),
        ];
        for (rowid, v) in values.iter().enumerate() {
            insert(&mut store, root, &index_record(v, rowid as i64 + 1)).unwrap();
        }
        let got: Vec<Vec<Value>> = scan(&mut store, root)
            .unwrap()
            .iter()
            .map(|r| deserialize_row(r).unwrap())
            .collect();
        let want = vec![
            vec![Value::Null, Value::Integer(4)],
            vec![Value::Null, Value::Integer(6)],
            vec![Value::Integer(-7), Value::Integer(5)],
            vec![Value::Real(2.5), Value::Integer(3)],
            vec![Value::Integer(3), Value::Integer(7)],
            vec![Value::Text("ab".into()), Value::Integer(8)],
            vec![Value::Text("abc".into()), Value::Integer(2)],
            vec![Value::Blob(vec![1, 2]), Value::Integer(1)],
        ];
        assert_eq!(got, want);
        // 3 and 3.0 are the same indexed value.
        assert_eq!(
            find_rowid(&mut store, root, &Value::Real(3.0)).unwrap(),
            Some(7)
        );
    }

    #[test]
    fn test_oversized_key_is_rejected() {
        let mut store = MemStore::new();
        let root = store.new_index();
        let big = Value::Text("x".repeat(MAX_LOCAL_PAYLOAD));
        assert!(insert(&mut store, root, &index_record(&big, 1)).is_err());
        let ok = Value::Text("x".repeat(MAX_LOCAL_PAYLOAD - 10));
        insert(&mut store, root, &index_record(&ok, 1)).unwrap();
        check_tree(&mut store, root, 1);
    }
}
