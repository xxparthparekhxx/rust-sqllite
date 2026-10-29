use crate::pager::PAGE_SIZE;
use crate::varint::{get_varint, put_varint};
use std::io;

/// Node type stored in the first byte of each B-Tree page.
const LEAF_NODE: u8 = 0x0D;
const INTERNAL_NODE: u8 = 0x05;

/// Node header layout (fixed region at the start of each B-Tree page):
///   [0]       node_type       (1 byte)
///   [1..2]    (reserved)      (2 bytes)
///   [3..5]    cell_count      (2 bytes, u16 BE)
///   [5..7]    cell_area_start (2 bytes, u16 BE) — offset of first cell from top
///   [7]       fragmented_bytes(1 byte)
///   [8..12]   right_child     (4 bytes, u32 BE) — only for internal nodes
///
/// Leaf header:  8 bytes
/// Internal hdr: 12 bytes
///
/// A "cell pointer array" follows the header: 2 bytes per cell (offset into the page).
/// Cells are stored from the end of the page growing downward.
///
/// SQLite Table Leaf cell layout:
///   payload_len (varint)
///   rowid       (varint)
///   payload     (payload_len bytes)
///
/// SQLite Table Interior cell layout:
///   left_child  (4 bytes u32 BE)
///   rowid       (varint)

const LEAF_HEADER_SIZE: usize = 8;
const INTERNAL_HEADER_SIZE: usize = 12;
const CELL_PTR_SIZE: usize = 2;

// ──────────────── Helper r/w functions ────────────────

fn read_u16(page: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([page[off], page[off + 1]])
}

fn write_u16(page: &mut [u8], off: usize, v: u16) {
    let b = v.to_be_bytes();
    page[off] = b[0];
    page[off + 1] = b[1];
}

fn read_u32(page: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]])
}

fn write_u32(page: &mut [u8], off: usize, v: u32) {
    let b = v.to_be_bytes();
    page[off] = b[0];
    page[off + 1] = b[1];
    page[off + 2] = b[2];
    page[off + 3] = b[3];
}

// ──────────────── Node accessors ────────────────

#[inline]
fn page_offset(page: &[u8]) -> usize {
    if page.starts_with(b"SQLite format 3\0") {
        100
    } else {
        0
    }
}

fn node_type(page: &[u8]) -> u8 {
    page[page_offset(page)]
}
fn is_leaf(page: &[u8]) -> bool {
    node_type(page) == LEAF_NODE
}
fn cell_count(page: &[u8]) -> u16 {
    read_u16(page, page_offset(page) + 3)
}
fn set_cell_count(page: &mut [u8], n: u16) {
    let off = page_offset(page);
    write_u16(page, off + 3, n);
}
fn cell_area_start(page: &[u8]) -> u16 {
    read_u16(page, page_offset(page) + 5)
}
fn set_cell_area_start(page: &mut [u8], v: u16) {
    let off = page_offset(page);
    write_u16(page, off + 5, v);
}
#[allow(dead_code)]
fn get_fragmented_bytes(page: &[u8]) -> u8 {
    page[page_offset(page) + 7]
}
#[allow(dead_code)]
fn set_fragmented_bytes(page: &mut [u8], n: u8) {
    let off = page_offset(page);
    page[off + 7] = n;
}
fn right_child(page: &[u8]) -> u32 {
    read_u32(page, page_offset(page) + 8)
}
fn set_right_child(page: &mut [u8], v: u32) {
    let off = page_offset(page);
    write_u32(page, off + 8, v);
}

fn header_size(page: &[u8]) -> usize {
    page_offset(page)
        + if is_leaf(page) {
            LEAF_HEADER_SIZE
        } else {
            INTERNAL_HEADER_SIZE
        }
}

/// Get the offset stored in the i-th cell pointer.
fn cell_ptr(page: &[u8], i: u16) -> u16 {
    let off = header_size(page) + (i as usize) * CELL_PTR_SIZE;
    read_u16(page, off)
}

/// Set the i-th cell pointer.
fn set_cell_ptr(page: &mut [u8], hdr_sz: usize, i: u16, offset: u16) {
    let off = hdr_sz + (i as usize) * CELL_PTR_SIZE;
    write_u16(page, off, offset);
}

// ──────────────── Cell reading ────────────────

/// Read a leaf cell at the given offset. Returns (key (rowid), value (payload)).
fn read_leaf_cell(page: &[u8], off: usize) -> (u64, Vec<u8>) {
    let (payload_len, len1) = get_varint(&page[off..]);
    let (rowid, len2) = get_varint(&page[off + len1..]);
    // Ignoring overflow pages for now
    let payload = page[off + len1 + len2..off + len1 + len2 + payload_len as usize].to_vec();
    (rowid, payload)
}

/// Read an internal cell at the given offset. Returns (left_child, key (rowid)).
fn read_internal_cell(page: &[u8], off: usize) -> (u32, u64) {
    let left_child = read_u32(page, off);
    let (rowid, _) = get_varint(&page[off + 4..]);
    (left_child, rowid)
}

fn leaf_cell_size(key: u64, value: &[u8]) -> usize {
    let mut buf = [0u8; 9];
    let len1 = put_varint(&mut buf, value.len() as u64);
    let len2 = put_varint(&mut buf, key);
    len1 + len2 + value.len()
}

fn internal_cell_size(key: u64) -> usize {
    let mut buf = [0u8; 9];
    let len1 = put_varint(&mut buf, key);
    4 + len1
}

// ──────────────── Page initializers ────────────────

pub fn init_leaf_page(page: &mut [u8; PAGE_SIZE]) {
    let off = page_offset(page);
    page[off..].fill(0);
    page[off] = LEAF_NODE;
    set_cell_count(page, 0);
    set_cell_area_start(page, PAGE_SIZE as u16);
}

pub fn init_internal_page(page: &mut [u8; PAGE_SIZE], right: u32) {
    let off = page_offset(page);
    page[off..].fill(0);
    page[off] = INTERNAL_NODE;
    set_cell_count(page, 0);
    set_cell_area_start(page, PAGE_SIZE as u16);
    set_right_child(page, right);
}

// ──────────────── The B-Tree API ────────────────

/// Provides read/write access to pages for the B-Tree.
/// The caller (engine) decides whether to route through WAL or pager directly.
pub trait PageStore {
    fn read_page(&mut self, page_num: u32) -> io::Result<[u8; PAGE_SIZE]>;
    fn write_page(&mut self, page_num: u32, data: [u8; PAGE_SIZE]) -> io::Result<()>;
    fn allocate_page(&mut self) -> io::Result<u32>;
    /// Return a page that is no longer part of any B-tree to the freelist.
    fn free_page(&mut self, page_num: u32) -> io::Result<()>;
}

/// Every page number in the table B-tree rooted at `root_page` (root included).
pub fn collect_pages(store: &mut dyn PageStore, root_page: u32) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut stack = vec![root_page];
    while let Some(pn) = stack.pop() {
        out.push(pn);
        let page = store.read_page(pn)?;
        if !is_leaf(&page) {
            for i in 0..cell_count(&page) {
                let (child, _) = read_internal_cell(&page, cell_ptr(&page, i) as usize);
                stack.push(child);
            }
            stack.push(right_child(&page));
        }
    }
    Ok(out)
}

/// Search for `key` starting from `root_page`. Returns the value if found.
pub fn search(store: &mut dyn PageStore, root_page: u32, key: u64) -> io::Result<Option<Vec<u8>>> {
    let page = store.read_page(root_page)?;
    if is_leaf(&page) {
        let n = cell_count(&page);
        for i in 0..n {
            let off = cell_ptr(&page, i) as usize;
            let (k, v) = read_leaf_cell(&page, off);
            if k == key {
                return Ok(Some(v));
            }
            if k > key {
                return Ok(None);
            }
        }
        Ok(None)
    } else {
        // Internal node: find the child to descend into.
        let n = cell_count(&page);
        for i in 0..n {
            let off = cell_ptr(&page, i) as usize;
            let (child, k) = read_internal_cell(&page, off);
            if key <= k {
                return search(store, child, key);
            }
        }
        // Key >= all keys: go to right child.
        let rc = right_child(&page);
        search(store, rc, key)
    }
}

/// Scan all key-value pairs in the tree rooted at `root_page`.
pub fn scan_all(store: &mut dyn PageStore, root_page: u32) -> io::Result<Vec<(u64, Vec<u8>)>> {
    let page = store.read_page(root_page)?;
    if is_leaf(&page) {
        let n = cell_count(&page);
        let mut results = Vec::new();
        for i in 0..n {
            let off = cell_ptr(&page, i) as usize;
            results.push(read_leaf_cell(&page, off));
        }
        Ok(results)
    } else {
        let n = cell_count(&page);
        let mut results = Vec::new();
        for i in 0..n {
            let off = cell_ptr(&page, i) as usize;
            let (child, _) = read_internal_cell(&page, off);
            results.extend(scan_all(store, child)?);
        }
        let rc = right_child(&page);
        results.extend(scan_all(store, rc)?);
        Ok(results)
    }
}

/// Insert a key-value pair into the tree rooted at `root_page`.
/// Returns the (possibly new) root page number.
pub fn insert(
    store: &mut dyn PageStore,
    root_page: u32,
    key: u64,
    value: &[u8],
) -> io::Result<u32> {
    match insert_recursive(store, root_page, key, value)? {
        InsertResult::Done => Ok(root_page),
        InsertResult::Split {
            new_page,
            median_key,
        } => {
            // Root was split — create a new root.
            let new_root = store.allocate_page()?;
            let mut page = [0u8; PAGE_SIZE];
            init_internal_page(&mut page, new_page);
            // Insert a single cell: (old root, median_key).
            write_internal_cell_to_page(&mut page, root_page, median_key)?;
            store.write_page(new_root, page)?;
            Ok(new_root)
        }
    }
}

enum InsertResult {
    Done,
    Split { new_page: u32, median_key: u64 },
}

fn insert_recursive(
    store: &mut dyn PageStore,
    page_num: u32,
    key: u64,
    value: &[u8],
) -> io::Result<InsertResult> {
    let mut page = store.read_page(page_num)?;

    if is_leaf(&page) {
        // Check for existing key (update in place).
        let n = cell_count(&page);
        for i in 0..n {
            let off = cell_ptr(&page, i) as usize;
            let (k, _) = read_leaf_cell(&page, off);
            if k == key {
                // For simplicity, rebuild the page without this cell and re-insert.
                let cells = collect_leaf_cells(&page);
                let mut new_cells: Vec<(u64, Vec<u8>)> = cells
                    .into_iter()
                    .map(|(k2, v2)| {
                        if k2 == key {
                            (k2, value.to_vec())
                        } else {
                            (k2, v2)
                        }
                    })
                    .collect();
                new_cells.sort_by(|a, b| a.0.cmp(&b.0));
                if !leaf_cells_fit(page_offset(&page), &new_cells) {
                    // The row grew and no longer fits: split like an insert would.
                    return split_leaf(store, page_num, page, new_cells);
                }
                rebuild_leaf_page(&mut page, &new_cells);
                store.write_page(page_num, page)?;
                return Ok(InsertResult::Done);
            }
        }

        // Try to insert into this leaf.
        let cell_sz = leaf_cell_size(key, value);
        let free_space = free_space_leaf(&page);
        if cell_sz + CELL_PTR_SIZE <= free_space {
            insert_leaf_cell(&mut page, key, value);
            store.write_page(page_num, page)?;
            Ok(InsertResult::Done)
        } else {
            // Split.
            let mut cells = collect_leaf_cells(&page);
            cells.push((key, value.to_vec()));
            cells.sort_by(|a, b| a.0.cmp(&b.0));
            split_leaf(store, page_num, page, cells)
        }
    } else {
        // Internal node — find the child.
        let n = cell_count(&page);
        let mut child_page = right_child(&page);
        let mut child_idx: Option<u16> = None; // index of the cell whose left child we descended

        for i in 0..n {
            let off = cell_ptr(&page, i) as usize;
            let (child, k) = read_internal_cell(&page, off);
            if key <= k {
                child_page = child;
                child_idx = Some(i);
                break;
            }
        }

        match insert_recursive(store, child_page, key, value)? {
            InsertResult::Done => Ok(InsertResult::Done),
            InsertResult::Split {
                new_page,
                median_key,
            } => {
                // Insert median_key into this internal node.
                // The new_page becomes the right part; we need to adjust pointers.
                let cell_sz = internal_cell_size(median_key);
                let free = free_space_internal(&page);

                if cell_sz + CELL_PTR_SIZE <= free {
                    // Enough room — insert here.
                    // The cell to insert: (child_page, median_key)
                    // Then the right of the split (new_page) replaces whatever child_page was.
                    // If child_idx is None, we descended into right_child:
                    //   - insert cell (old_right, median) and set right_child = new_page
                    // If child_idx is Some(i):
                    //   - the cell at i had left_child = child_page
                    //   - insert new cell (child_page, median) before cell i,
                    //     and change cell i's left_child to new_page
                    let mut page = store.read_page(page_num)?;

                    match child_idx {
                        None => {
                            // Descended into right_child.
                            let old_right = right_child(&page);
                            insert_internal_cell(&mut page, old_right, median_key);
                            set_right_child(&mut page, new_page);
                        }
                        Some(child_idx) => {
                            // The cell at child_idx references child_page with an old separator key.
                            // The child page split into child_page (keys < median_key) and new_page
                            // (keys >= median_key). We must:
                            //   1. Replace (child_page, old_key) with (child_page, median_key)
                            //   2. Insert (new_page, old_key) right after it.
                            let mut cells = collect_internal_cells(&page);
                            let rc = right_child(&page);
                            let old_key = cells[child_idx as usize].1;
                            cells[child_idx as usize] = (child_page, median_key);
                            cells.insert(child_idx as usize + 1, (new_page, old_key));
                            rebuild_internal_page(&mut page, &cells, rc);
                        }
                    }
                    store.write_page(page_num, page)?;
                    Ok(InsertResult::Done)
                } else {
                    // Split this internal node.
                    let mut cells = collect_internal_cells(&page);
                    let mut rc = right_child(&page);

                    match child_idx {
                        // Replace the separator key of the cell referencing child_page
                        // with median_key (max key of the now-smaller child_page), and
                        // insert (new_page, old_key) right after it (old_key = max key
                        // of new_page).
                        Some(i) => {
                            let i = i as usize;
                            let old_key = cells[i].1;
                            cells[i] = (child_page, median_key);
                            cells.insert(i + 1, (new_page, old_key));
                        }
                        // The right child split: it keeps the keys up to median_key
                        // behind a new last cell, and new_page becomes the right child.
                        None => {
                            cells.push((child_page, median_key));
                            rc = new_page;
                        }
                    }

                    // Promote cells[mid] as the separator key to the parent.
                    let mid = cells.len() / 2;
                    let promote_key = cells[mid].1;

                    let left_cells = &cells[..mid];
                    // The left child of the promoted key's cell is the right_child of the left page.
                    let left_rc = cells[mid].0;
                    let right_cells = &cells[mid + 1..];
                    let right_rc = rc;

                    rebuild_internal_page(&mut page, left_cells, left_rc);
                    store.write_page(page_num, page)?;

                    let new_int_page = store.allocate_page()?;
                    let mut new_page_data = [0u8; PAGE_SIZE];
                    init_internal_page(&mut new_page_data, right_rc);
                    rebuild_internal_page(&mut new_page_data, right_cells, right_rc);
                    store.write_page(new_int_page, new_page_data)?;

                    Ok(InsertResult::Split {
                        new_page: new_int_page,
                        median_key: promote_key,
                    })
                }
            }
        }
    }
}

/// True if `cells` fit on one leaf page whose B-tree header starts at `hdr_off`.
fn leaf_cells_fit(hdr_off: usize, cells: &[(u64, Vec<u8>)]) -> bool {
    let used: usize = cells
        .iter()
        .map(|(k, v)| leaf_cell_size(*k, v) + CELL_PTR_SIZE)
        .sum();
    hdr_off + LEAF_HEADER_SIZE + used <= PAGE_SIZE
}

/// Split the sorted `cells` of leaf `page_num` across it and a new right sibling.
fn split_leaf(
    store: &mut dyn PageStore,
    page_num: u32,
    mut page: [u8; PAGE_SIZE],
    cells: Vec<(u64, Vec<u8>)>,
) -> io::Result<InsertResult> {
    let hdr_off = page_offset(&page);
    let fits =
        |mid: usize| leaf_cells_fit(hdr_off, &cells[..mid]) && leaf_cells_fit(0, &cells[mid..]);

    // Split by count, as before; fall back to the most even split by bytes when one
    // half would overflow (rows of very different sizes).
    let mut mid = cells.len() / 2;
    if !fits(mid) {
        let sizes: Vec<usize> = cells
            .iter()
            .map(|(k, v)| leaf_cell_size(*k, v) + CELL_PTR_SIZE)
            .collect();
        let total: usize = sizes.iter().sum();
        let mut left = 0;
        let mut best = (usize::MAX, mid);
        for m in 1..cells.len() {
            left += sizes[m - 1];
            let worst = left.max(total - left);
            if worst < best.0 {
                best = (worst, m);
            }
        }
        mid = best.1;
        if !fits(mid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "row too large: overflow pages are not supported",
            ));
        }
    }
    // SQLite separator key convention: max key of the left subtree.
    let median_key = cells[mid - 1].0;

    let left_cells = &cells[..mid];
    let right_cells = &cells[mid..];

    // Reuse current page for left.
    rebuild_leaf_page(&mut page, left_cells);
    store.write_page(page_num, page)?;

    // New page for right.
    let new_page_num = store.allocate_page()?;
    let mut new_page = [0u8; PAGE_SIZE];
    init_leaf_page(&mut new_page);
    rebuild_leaf_page(&mut new_page, right_cells);
    store.write_page(new_page_num, new_page)?;

    Ok(InsertResult::Split {
        new_page: new_page_num,
        median_key,
    })
}

// ──────────────── Delete and rebalancing ────────────────
//
// A delete decodes the pages on the path from the root to the leaf into `Node`s,
// removes the row, and writes the path back from the bottom up, the way
// `index::balance` does for index B-trees:
//
// - a non-root page that runs low (below `MIN_FILL`, in particular an empty one)
//   is merged with a sibling, and the parent loses the divider between them and
//   the pointer to the emptied page, which goes on the freelist; if the two do
//   not fit on one page, they are split evenly between the two pages instead and
//   the parent gets a new divider;
// - a page that overflows (only an interior page can, when a divider below it is
//   replaced by a longer varint) is split, and its parent gets the new divider;
// - an interior root left with a single child takes over that child's contents,
//   so the root page number recorded in sqlite_schema never changes.
//
// SQLite reports any empty non-root page as corruption, so none is left behind.
// Dividers follow the convention of `split_leaf`: a divider is at least the largest
// rowid of the subtree on its left and below every rowid of the subtree on its
// right; a divider set by a redistribution is the largest rowid of the left page.

/// A non-root page whose encoded size drops below this is merged with, or
/// refilled from, a sibling (the threshold the index B-tree uses too).
const MIN_FILL: usize = PAGE_SIZE / 3;

/// One decoded table B-tree page.
#[derive(Debug, Clone, Default)]
struct Node {
    /// Leaf cells, (rowid, record) in rowid order. Empty for an interior page.
    cells: Vec<(u64, Vec<u8>)>,
    /// Interior dividers: `keys[i]` bounds the rowids under `children[i]` from
    /// above, and is below every rowid under `children[i + 1]`.
    keys: Vec<u64>,
    /// Child pages, `keys.len() + 1` of them, the last being the right child.
    /// Empty for a leaf.
    children: Vec<u32>,
}

impl Node {
    fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    /// Bytes the node needs on a page whose B-tree header starts at `hdr_off`
    /// (100 on page 1, 0 elsewhere): header, cell pointers and cells.
    fn byte_size(&self, hdr_off: usize) -> usize {
        hdr_off
            + if self.is_leaf() {
                LEAF_HEADER_SIZE
                    + self
                        .cells
                        .iter()
                        .map(|(k, v)| leaf_cell_size(*k, v) + CELL_PTR_SIZE)
                        .sum::<usize>()
            } else {
                INTERNAL_HEADER_SIZE
                    + self
                        .keys
                        .iter()
                        .map(|k| internal_cell_size(*k) + CELL_PTR_SIZE)
                        .sum::<usize>()
            }
    }

    fn fits(&self, hdr_off: usize) -> bool {
        self.byte_size(hdr_off) <= PAGE_SIZE
    }
}

fn decode(page: &[u8; PAGE_SIZE], page_num: u32) -> io::Result<Node> {
    match node_type(page) {
        LEAF_NODE => Ok(Node {
            cells: collect_leaf_cells(page),
            ..Node::default()
        }),
        INTERNAL_NODE => {
            let cells = collect_internal_cells(page);
            let keys = cells.iter().map(|&(_, k)| k).collect();
            let mut children: Vec<u32> = cells.iter().map(|&(c, _)| c).collect();
            children.push(right_child(page));
            Ok(Node {
                cells: Vec::new(),
                keys,
                children,
            })
        }
        t => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("table page {}: unexpected page type {:#04x}", page_num, t),
        )),
    }
}

/// Write `node` into `page`, packed: cells contiguous at the end of the page, in
/// key order, with no freeblocks or fragments. Whatever precedes the B-tree header
/// (page 1's file header) is kept.
fn encode(node: &Node, page: &mut [u8; PAGE_SIZE]) {
    debug_assert!(node.fits(page_offset(page)));
    let leaf = node.is_leaf();
    if leaf {
        init_leaf_page(page);
    } else {
        let right = *node
            .children
            .last()
            .expect("interior node has a right child");
        init_internal_page(page, right);
    }
    let hdr = header_size(page);
    let n = if leaf {
        node.cells.len()
    } else {
        node.keys.len()
    };
    let mut top = PAGE_SIZE;
    for i in 0..n {
        if leaf {
            let (key, value) = &node.cells[i];
            top -= leaf_cell_size(*key, value);
            let mut off = top;
            off += put_varint(&mut page[off..], value.len() as u64);
            off += put_varint(&mut page[off..], *key);
            page[off..off + value.len()].copy_from_slice(value);
        } else {
            let key = node.keys[i];
            top -= internal_cell_size(key);
            write_u32(page, top, node.children[i]);
            put_varint(&mut page[top + 4..], key);
        }
        set_cell_ptr(page, hdr, i as u16, top as u16);
    }
    set_cell_count(page, n as u16);
    set_cell_area_start(page, top as u16);
}

fn read_node(store: &mut dyn PageStore, page_num: u32) -> io::Result<Node> {
    decode(&store.read_page(page_num)?, page_num)
}

/// Write `node` to `page_num`, a page that is not the root.
fn write_node(store: &mut dyn PageStore, page_num: u32, node: &Node) -> io::Result<()> {
    if !node.fits(0) {
        return Err(io::Error::other(format!(
            "table page {}: node does not fit on a page",
            page_num
        )));
    }
    let mut page = [0u8; PAGE_SIZE];
    encode(node, &mut page);
    store.write_page(page_num, page)
}

/// Split `node` into two halves that each fit on a page and keep at least one
/// cell, as even in bytes as possible. Returns (left, divider, right). A leaf's
/// divider is the largest rowid of the left half; an interior page's is the key
/// between the halves, which moves up into the parent.
fn split(node: Node) -> io::Result<(Node, u64, Node)> {
    if node.is_leaf() {
        let mut cells = node.cells;
        let sizes: Vec<usize> = cells
            .iter()
            .map(|(k, v)| leaf_cell_size(*k, v) + CELL_PTR_SIZE)
            .collect();
        let total: usize = sizes.iter().sum();
        let mut best = (usize::MAX, 0);
        let mut left = 0;
        for m in 1..cells.len() {
            left += sizes[m - 1];
            let worst = left.max(total - left);
            if worst < best.0 {
                best = (worst, m);
            }
        }
        let (worst, m) = best;
        if m == 0 || LEAF_HEADER_SIZE + worst > PAGE_SIZE {
            return Err(io::Error::other("table leaf split cannot fit both halves"));
        }
        let right = cells.split_off(m);
        let divider = cells[m - 1].0;
        Ok((
            Node {
                cells,
                ..Node::default()
            },
            divider,
            Node {
                cells: right,
                ..Node::default()
            },
        ))
    } else {
        let (mut keys, mut children) = (node.keys, node.children);
        let n = keys.len();
        let sizes: Vec<usize> = keys
            .iter()
            .map(|k| internal_cell_size(*k) + CELL_PTR_SIZE)
            .collect();
        let total: usize = sizes.iter().sum();
        let mut best = (usize::MAX, 0);
        let mut left = 0;
        for d in 1..n.saturating_sub(1) {
            left += sizes[d - 1];
            let worst = left.max(total - left - sizes[d]);
            if worst < best.0 {
                best = (worst, d);
            }
        }
        let (worst, d) = best;
        if d == 0 || INTERNAL_HEADER_SIZE + worst > PAGE_SIZE {
            return Err(io::Error::other(
                "table interior split cannot fit both halves",
            ));
        }
        let right_keys = keys.split_off(d + 1);
        let divider = keys.pop().expect("divider key");
        let right_children = children.split_off(d + 1);
        Ok((
            Node {
                cells: Vec::new(),
                keys,
                children,
            },
            divider,
            Node {
                cells: Vec::new(),
                keys: right_keys,
                children: right_children,
            },
        ))
    }
}

/// Concatenate two siblings and the parent's divider between them. Leaves drop
/// the divider (it is only a bound); interior pages pull it down between the left
/// page's right child and the right page's first child.
fn join(mut left: Node, divider: u64, right: Node) -> Node {
    if left.is_leaf() {
        left.cells.extend(right.cells);
    } else {
        left.keys.push(divider);
        left.keys.extend(right.keys);
        left.children.extend(right.children);
    }
    left
}

/// One page on the path from the root to the leaf being changed.
struct Level {
    page_num: u32,
    node: Node,
    /// Index into `node.children` of the next page down the path.
    child: usize,
    /// Whether `node` differs from what is on disk.
    modified: bool,
}

/// Delete `key` from the tree rooted at `root_page`, rebalancing as needed (see
/// above). The root page never changes. Returns true if the key was found.
pub fn delete(store: &mut dyn PageStore, root_page: u32, key: u64) -> io::Result<bool> {
    let mut path: Vec<Level> = Vec::new();
    let mut page_num = root_page;
    loop {
        let mut node = read_node(store, page_num)?;
        if node.is_leaf() {
            match node.cells.binary_search_by_key(&key, |(k, _)| *k) {
                Ok(i) => {
                    node.cells.remove(i);
                }
                Err(_) => return Ok(false),
            }
            path.push(Level {
                page_num,
                node,
                child: 0,
                modified: true,
            });
            break;
        }
        // The first child whose divider is >= key, else the right child.
        let child = node.keys.partition_point(|&k| k < key);
        let next = node.children[child];
        path.push(Level {
            page_num,
            node,
            child,
            modified: false,
        });
        page_num = next;
    }
    balance(store, path)?;
    Ok(true)
}

/// Write back every modified page on `path`, from the bottom up, merging or
/// refilling non-root pages that run low and splitting pages that overflow.
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

        if !cur.node.fits(0) {
            // Overflow: keep the left half here, move the right half to a new page,
            // and give the parent the divider between them.
            let (left, divider, right) = split(std::mem::take(&mut cur.node))?;
            write_node(store, cur.page_num, &left)?;
            let new_page = store.allocate_page()?;
            write_node(store, new_page, &right)?;
            parent.node.keys.insert(ci, divider);
            parent.node.children.insert(ci + 1, new_page);
            parent.modified = true;
        } else if cur.node.byte_size(0) < MIN_FILL && !parent.node.keys.is_empty() {
            // Underflow: combine with the left sibling (the right one for the first
            // child). The parent has a divider here: interior pages on disk have
            // one (except an uncollapsible page-1 root, excluded above), and only
            // this level removes dividers from the parent.
            let li = ci.saturating_sub(1);
            let (left_pg, right_pg) = (parent.node.children[li], parent.node.children[li + 1]);
            let this = std::mem::take(&mut cur.node);
            let (left, right) = if li == ci {
                (this, read_node(store, right_pg)?)
            } else {
                (read_node(store, left_pg)?, this)
            };
            let merged = join(left, parent.node.keys[li], right);
            if merged.fits(0) {
                // The merged page keeps the right page's upper bound: the divider
                // after it in the parent (or the parent's own bound) still holds.
                write_node(store, left_pg, &merged)?;
                store.free_page(right_pg)?;
                parent.node.keys.remove(li);
                parent.node.children.remove(li + 1);
            } else {
                let (l, divider, r) = split(merged)?;
                write_node(store, left_pg, &l)?;
                write_node(store, right_pg, &r)?;
                parent.node.keys[li] = divider;
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
    let mut page = store.read_page(root.page_num)?;
    let hdr_off = page_offset(&page);
    if !root.node.fits(hdr_off) {
        // Root overflow: move both halves into new children so that the root page
        // number (recorded in sqlite_schema) stays the same.
        let (left, divider, right) = split(std::mem::take(&mut root.node))?;
        let left_pg = store.allocate_page()?;
        write_node(store, left_pg, &left)?;
        let right_pg = store.allocate_page()?;
        write_node(store, right_pg, &right)?;
        root.node = Node {
            cells: Vec::new(),
            keys: vec![divider],
            children: vec![left_pg, right_pg],
        };
    } else if !root.node.is_leaf() && root.node.keys.is_empty() {
        // An interior root with a single child: pull the child up into the root.
        // (Its contents always fit unless the root is page 1, which has 100 bytes
        // less; SQLite accepts an interior page 1 without cells, so it then stays.)
        let only = root.node.children[0];
        let child = read_node(store, only)?;
        if child.fits(hdr_off) {
            root.node = child;
            store.free_page(only)?;
        }
    }
    encode(&root.node, &mut page);
    store.write_page(root.page_num, page)
}

// ──────────────── Page-level helpers ────────────────

fn free_space_leaf(page: &[u8]) -> usize {
    let n = cell_count(page) as usize;
    let ptrs_end = header_size(page) + n * CELL_PTR_SIZE;
    let area = cell_area_start(page) as usize;
    if area > ptrs_end { area - ptrs_end } else { 0 }
}

fn free_space_internal(page: &[u8]) -> usize {
    let n = cell_count(page) as usize;
    let ptrs_end = header_size(page) + n * CELL_PTR_SIZE;
    let area = cell_area_start(page) as usize;
    if area > ptrs_end { area - ptrs_end } else { 0 }
}

fn insert_leaf_cell(page: &mut [u8], key: u64, value: &[u8]) {
    let n = cell_count(page);
    let cell_sz = leaf_cell_size(key, value);
    let new_area = cell_area_start(page) as usize - cell_sz;

    // Write cell data.
    let mut off = new_area;
    off += put_varint(&mut page[off..], value.len() as u64);
    off += put_varint(&mut page[off..], key);
    page[off..off + value.len()].copy_from_slice(value);

    // Insert cell pointer in sorted order.
    // Find position.
    let mut pos = n;
    for i in 0..n {
        let existing_off = cell_ptr(page, i) as usize;
        let (k, _) = read_leaf_cell(page, existing_off);
        if key < k {
            pos = i;
            break;
        }
    }

    // Shift pointers right.
    let hdr = header_size(page);
    for i in (pos..n).rev() {
        let v = read_u16(page, hdr + (i as usize) * CELL_PTR_SIZE);
        write_u16(page, hdr + ((i + 1) as usize) * CELL_PTR_SIZE, v);
    }
    set_cell_ptr(page, hdr, pos, new_area as u16);

    set_cell_count(page, n + 1);
    set_cell_area_start(page, new_area as u16);
}

fn insert_internal_cell(page: &mut [u8], left_child: u32, key: u64) {
    let n = cell_count(page);
    let cell_sz = internal_cell_size(key);
    let new_area = cell_area_start(page) as usize - cell_sz;

    let mut off = new_area;
    write_u32(page, off, left_child);
    off += 4;
    put_varint(&mut page[off..], key);

    // Find sorted position.
    let mut pos = n;
    for i in 0..n {
        let existing_off = cell_ptr(page, i) as usize;
        let (_, k) = read_internal_cell(page, existing_off);
        if key < k {
            pos = i;
            break;
        }
    }

    let hdr = header_size(page);
    for i in (pos..n).rev() {
        let v = read_u16(page, hdr + (i as usize) * CELL_PTR_SIZE);
        write_u16(page, hdr + ((i + 1) as usize) * CELL_PTR_SIZE, v);
    }
    set_cell_ptr(page, hdr, pos, new_area as u16);

    set_cell_count(page, n + 1);
    set_cell_area_start(page, new_area as u16);
}

fn write_internal_cell_to_page(
    page: &mut [u8; PAGE_SIZE],
    left_child: u32,
    key: u64,
) -> io::Result<()> {
    insert_internal_cell(page, left_child, key);
    Ok(())
}

fn collect_leaf_cells(page: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let n = cell_count(page);
    let mut cells = Vec::with_capacity(n as usize);
    for i in 0..n {
        let off = cell_ptr(page, i) as usize;
        cells.push(read_leaf_cell(page, off));
    }
    cells
}

fn collect_internal_cells(page: &[u8]) -> Vec<(u32, u64)> {
    let n = cell_count(page);
    let mut cells = Vec::with_capacity(n as usize);
    for i in 0..n {
        let off = cell_ptr(page, i) as usize;
        cells.push(read_internal_cell(page, off));
    }
    cells
}

fn rebuild_leaf_page(page: &mut [u8; PAGE_SIZE], cells: &[(u64, Vec<u8>)]) {
    init_leaf_page(page);
    for (k, v) in cells {
        insert_leaf_cell(page, *k, v);
    }
}

fn rebuild_internal_page(page: &mut [u8; PAGE_SIZE], cells: &[(u32, u64)], right: u32) {
    init_internal_page(page, right);
    for (child, k) in cells {
        insert_internal_cell(page, *child, *k);
    }
}

// ───────────────────────────── Tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    /// In-memory page store for testing, with a freelist that is reused first.
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
    }

    impl PageStore for MemStore {
        fn read_page(&mut self, page_num: u32) -> io::Result<[u8; PAGE_SIZE]> {
            self.pages
                .get(&page_num)
                .copied()
                .ok_or_else(|| io::Error::other(format!("read of unallocated page {}", page_num)))
        }
        fn write_page(&mut self, page_num: u32, data: [u8; PAGE_SIZE]) -> io::Result<()> {
            assert!(
                !self.free.contains(&page_num),
                "write to free page {}",
                page_num
            );
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

    /// Check the tree rooted at `root` the way `PRAGMA integrity_check` does:
    /// rowids increase and stay inside the bounds the dividers above them set
    /// (left subtree <= divider < right subtree), every leaf is at the same depth,
    /// no page but a leaf root is empty, and no page is used twice. Also check
    /// that every allocated page is either in the tree or free. Returns the rowids.
    fn check_tree(store: &mut MemStore, root: u32) -> Vec<u64> {
        struct Walk {
            keys: Vec<u64>,
            leaf_depth: Option<usize>,
            pages: BTreeSet<u32>,
        }
        fn walk(
            store: &mut MemStore,
            w: &mut Walk,
            pn: u32,
            depth: usize,
            lo: Option<u64>,
            hi: Option<u64>,
        ) {
            assert!(w.pages.insert(pn), "page {} used twice", pn);
            let page = store.read_page(pn).unwrap();
            let n = cell_count(&page);
            assert!(
                n > 0 || (depth == 0 && is_leaf(&page)),
                "page {} is empty (depth {})",
                pn,
                depth
            );
            let in_bounds = |k: u64| lo.is_none_or(|lo| k > lo) && hi.is_none_or(|hi| k <= hi);
            if is_leaf(&page) {
                match w.leaf_depth {
                    Some(d) => assert_eq!(d, depth, "page {}: leaf depth differs", pn),
                    None => w.leaf_depth = Some(depth),
                }
                for i in 0..n {
                    let (k, _) = read_leaf_cell(&page, cell_ptr(&page, i) as usize);
                    assert!(
                        in_bounds(k),
                        "page {}: rowid {} outside ({:?}, {:?}]",
                        pn,
                        k,
                        lo,
                        hi
                    );
                    assert!(
                        w.keys.last().is_none_or(|&p| k > p),
                        "page {}: rowid {} out of order",
                        pn,
                        k
                    );
                    w.keys.push(k);
                }
                return;
            }
            let mut lower = lo;
            for i in 0..n {
                let (child, k) = read_internal_cell(&page, cell_ptr(&page, i) as usize);
                assert!(
                    in_bounds(k) && lower.is_none_or(|l| k > l),
                    "page {}: divider {} out of order",
                    pn,
                    k
                );
                walk(store, w, child, depth + 1, lower, Some(k));
                lower = Some(k);
            }
            walk(store, w, right_child(&page), depth + 1, lower, hi);
        }
        let mut w = Walk {
            keys: Vec::new(),
            leaf_depth: None,
            pages: BTreeSet::new(),
        };
        walk(store, &mut w, root, 0, None, None);
        let mut accounted = w.pages.clone();
        for &f in &store.free {
            assert!(accounted.insert(f), "page {} is both used and free", f);
        }
        let allocated: BTreeSet<u32> = (2..store.next_page).collect();
        assert_eq!(accounted, allocated, "pages leaked");
        w.keys
    }

    /// The row stored under rowid `i` in the delete tests, a little like
    /// 'value N padded out a little'.
    fn row(i: u64) -> Vec<u8> {
        format!("value {} padded out a little", i).into_bytes()
    }

    fn tree_depth(store: &mut MemStore, root: u32) -> usize {
        let mut depth = 0;
        let mut page = store.read_page(root).unwrap();
        while !is_leaf(&page) {
            depth += 1;
            let (child, _) = read_internal_cell(&page, cell_ptr(&page, 0) as usize);
            page = store.read_page(child).unwrap();
        }
        depth
    }

    fn make_root(store: &mut MemStore) -> u32 {
        let pn = store.allocate_page().unwrap();
        let mut page = [0u8; PAGE_SIZE];
        init_leaf_page(&mut page);
        store.write_page(pn, page).unwrap();
        pn
    }

    #[test]
    fn test_btree_insert_search() {
        let mut store = MemStore::new();
        let mut root = make_root(&mut store);

        root = insert(&mut store, root, 1, b"world").unwrap();
        let r = search(&mut store, root, 1).unwrap();
        assert_eq!(r, Some(b"world".to_vec()));

        let r2 = search(&mut store, root, 2).unwrap();
        assert!(r2.is_none());
    }

    #[test]
    fn test_btree_update() {
        let mut store = MemStore::new();
        let mut root = make_root(&mut store);

        root = insert(&mut store, root, 1, b"val1").unwrap();
        root = insert(&mut store, root, 1, b"val2").unwrap();

        let r = search(&mut store, root, 1).unwrap();
        assert_eq!(r, Some(b"val2".to_vec()));
    }

    #[test]
    fn test_btree_many_inserts() {
        let mut store = MemStore::new();
        let mut root = make_root(&mut store);

        // Insert enough keys to trigger splits.
        for i in 0..200u64 {
            let val = format!("val_{:05}", i);
            root = insert(&mut store, root, i, val.as_bytes()).unwrap();
        }

        // Verify all keys.
        for i in 0..200u64 {
            let val = format!("val_{:05}", i);
            let r = search(&mut store, root, i).unwrap();
            assert_eq!(r, Some(val.into_bytes()), "failed for {}", i);
        }
    }

    #[test]
    fn test_btree_scan() {
        let mut store = MemStore::new();
        let mut root = make_root(&mut store);

        for i in 0..50u64 {
            let val = format!("v{:03}", i);
            root = insert(&mut store, root, i, val.as_bytes()).unwrap();
        }

        let all = scan_all(&mut store, root).unwrap();
        assert_eq!(all.len(), 50);
        // Should be sorted.
        for w in all.windows(2) {
            assert!(w[0].0 <= w[1].0);
        }
    }

    #[test]
    fn test_btree_update_grows_row_on_full_leaf() {
        let mut store = MemStore::new();
        let mut root = make_root(&mut store);

        // Fill one leaf almost completely with small rows.
        let mut i = 0u64;
        while free_space_leaf(&store.read_page(root).unwrap()) > 40 {
            root = insert(&mut store, root, i, b"0123456789").unwrap();
            i += 1;
        }
        assert!(
            is_leaf(&store.read_page(root).unwrap()),
            "should still be one leaf"
        );

        // Growing one row past the free space must split instead of overlapping cells.
        let big = vec![b'x'; 200];
        root = insert(&mut store, root, 3, &big).unwrap();
        assert!(
            !is_leaf(&store.read_page(root).unwrap()),
            "root should have split"
        );
        assert_eq!(search(&mut store, root, 3).unwrap(), Some(big));
        let all = scan_all(&mut store, root).unwrap();
        assert_eq!(all.len() as u64, i);
        for (k, v) in &all {
            if *k != 3 {
                assert_eq!(v.as_slice(), b"0123456789");
            }
        }
    }

    #[test]
    fn test_btree_sequential_inserts_split_full_interior_page() {
        // ~1000-byte rows: a few per leaf, so the interior root fills up and has to
        // split while the insert is going down its right-most child.
        let mut store = MemStore::new();
        let mut root = make_root(&mut store);
        let val = vec![b'v'; 1000];
        let n = 3000u64;
        for i in 1..=n {
            root = insert(&mut store, root, i, &val).unwrap();
        }
        let page = store.read_page(root).unwrap();
        let (first, _) = read_internal_cell(&page, cell_ptr(&page, 0) as usize);
        assert!(
            !is_leaf(&store.read_page(first).unwrap()),
            "expected two interior levels"
        );
        let keys: Vec<u64> = scan_all(&mut store, root)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, (1..=n).collect::<Vec<_>>());
        for i in [1, n / 2, n] {
            assert_eq!(search(&mut store, root, i).unwrap(), Some(val.clone()));
        }
    }

    #[test]
    fn test_btree_delete() {
        let mut store = MemStore::new();
        let mut root = make_root(&mut store);

        root = insert(&mut store, root, 1, b"1").unwrap();
        root = insert(&mut store, root, 2, b"2").unwrap();
        root = insert(&mut store, root, 3, b"3").unwrap();

        let deleted = delete(&mut store, root, 2).unwrap();
        assert!(deleted);

        assert!(search(&mut store, root, 2).unwrap().is_none());
        assert_eq!(search(&mut store, root, 1).unwrap(), Some(b"1".to_vec()));
        assert_eq!(search(&mut store, root, 3).unwrap(), Some(b"3".to_vec()));

        // Delete non-existent key.
        assert!(!delete(&mut store, root, 99).unwrap());
    }

    #[test]
    fn test_btree_range_deletes_leave_no_empty_pages() {
        // 2000 rows, then a contiguous range deleted one rowid at a time, the way
        // `DELETE FROM p WHERE id <= 1000` does it.
        let n = 2000u64;
        let patterns: Vec<(&str, Vec<u64>)> = vec![
            ("lower half", (1..=n / 2).collect()),
            ("upper half", (n / 2 + 1..=n).collect()),
            ("middle", (n / 4..=3 * n / 4).collect()),
            ("lower half, descending", (1..=n / 2).rev().collect()),
            ("all but the ends", (2..n).collect()),
        ];
        for (name, doomed) in patterns {
            let mut store = MemStore::new();
            let mut root = make_root(&mut store);
            for i in 1..=n {
                root = insert(&mut store, root, i, &row(i)).unwrap();
            }
            assert!(tree_depth(&mut store, root) >= 1);
            for (j, &k) in doomed.iter().enumerate() {
                assert!(
                    delete(&mut store, root, k).unwrap(),
                    "{}: missing {}",
                    name,
                    k
                );
                if j % 97 == 0 {
                    check_tree(&mut store, root);
                }
            }
            let gone: BTreeSet<u64> = doomed.iter().copied().collect();
            let want: Vec<u64> = (1..=n).filter(|k| !gone.contains(k)).collect();
            assert_eq!(check_tree(&mut store, root), want, "{}", name);
            assert!(!store.free.is_empty(), "{}: no page was freed", name);
            for k in [1, n / 4 - 1, n / 2, n / 2 + 1, n] {
                let expect = (!gone.contains(&k)).then(|| row(k));
                assert_eq!(
                    search(&mut store, root, k).unwrap(),
                    expect,
                    "{}: rowid {}",
                    name,
                    k
                );
            }
        }
    }

    #[test]
    fn test_btree_delete_everything_collapses_root_and_frees_pages() {
        let n = 10_000u64;
        let mut store = MemStore::new();
        let mut root = make_root(&mut store);
        for i in 1..=n {
            root = insert(&mut store, root, i, &row(i)).unwrap();
        }
        assert!(tree_depth(&mut store, root) >= 1);
        let allocated = store.next_page;
        for i in 1..=n {
            assert!(delete(&mut store, root, i).unwrap());
        }
        assert!(!delete(&mut store, root, 1).unwrap());
        // The root stays where it is (sqlite_schema points at it), now an empty
        // leaf; every other page is on the freelist.
        assert!(check_tree(&mut store, root).is_empty());
        assert!(is_leaf(&store.read_page(root).unwrap()));
        assert_eq!(store.free.len() as u32, allocated - 2 - 1);

        // Inserting again reuses the freed pages before growing the file.
        for i in 1..=n {
            root = insert(&mut store, root, i, &row(i)).unwrap();
        }
        assert_eq!(check_tree(&mut store, root), (1..=n).collect::<Vec<_>>());
        assert!(store.next_page <= allocated + 1);
    }

    #[test]
    fn test_btree_random_churn_with_mixed_row_sizes() {
        // Rows of 1 to 1500 bytes, so some leaves hold dozens of rows and some two:
        // merges, redistributions, and rebalancing of interior pages all happen,
        // on a tree with two interior levels.
        let mut store = MemStore::new();
        let mut root = make_root(&mut store);
        let mut rng = Rng(0x5EED);
        let mut live: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let mut keys: Vec<u64> = Vec::new();
        fn value(rng: &mut Rng, k: u64) -> Vec<u8> {
            let len = if rng.below(4) == 0 {
                600 + rng.below(900)
            } else {
                1 + rng.below(60)
            };
            vec![(k % 251) as u8; len]
        }
        while live.len() < 8000 {
            let k = 1 + rng.below(1_000_000) as u64;
            let v = value(&mut rng, k);
            root = insert(&mut store, root, k, &v).unwrap();
            if live.insert(k, v).is_none() {
                keys.push(k);
            }
        }
        assert!(
            tree_depth(&mut store, root) >= 2,
            "expected two interior levels"
        );
        assert_eq!(
            check_tree(&mut store, root),
            live.keys().copied().collect::<Vec<_>>()
        );

        // Mostly deletes, some inserts, until the table is empty.
        let mut step = 0;
        while !keys.is_empty() {
            if keys.len() > 100 && rng.below(4) == 0 {
                let k = 1 + rng.below(1_000_000) as u64;
                let v = value(&mut rng, k);
                root = insert(&mut store, root, k, &v).unwrap();
                if live.insert(k, v).is_none() {
                    keys.push(k);
                }
            } else {
                let k = keys.swap_remove(rng.below(keys.len()));
                live.remove(&k);
                assert!(delete(&mut store, root, k).unwrap(), "missing {}", k);
                assert!(!delete(&mut store, root, k).unwrap());
            }
            step += 1;
            if step % 500 == 0 {
                assert_eq!(
                    check_tree(&mut store, root),
                    live.keys().copied().collect::<Vec<_>>()
                );
                let probe = keys[rng.below(keys.len())];
                assert_eq!(
                    search(&mut store, root, probe).unwrap().as_ref(),
                    live.get(&probe)
                );
            }
        }
        assert!(check_tree(&mut store, root).is_empty());
        assert_eq!(tree_depth(&mut store, root), 0);
    }

    /// A new leaf page holding `rows`.
    fn leaf_with(store: &mut MemStore, rows: &[(u64, Vec<u8>)]) -> u32 {
        let pn = store.allocate_page().unwrap();
        let mut page = [0u8; PAGE_SIZE];
        init_leaf_page(&mut page);
        for (k, v) in rows {
            insert_leaf_cell(&mut page, *k, v);
        }
        store.write_page(pn, page).unwrap();
        pn
    }

    /// An interior page with no byte to spare, over leaves. The first leaf holds
    /// rowids 1..=10 (100-byte rows) under divider 100, a 1-byte varint; the second
    /// holds rowids 20000..=20010 (300-byte rows); the others one row each. Returns
    /// the page and every row under it, in order.
    fn full_interior_page(store: &mut MemStore) -> ([u8; PAGE_SIZE], Vec<(u64, Vec<u8>)>) {
        fn add(
            store: &mut MemStore,
            page: &mut [u8; PAGE_SIZE],
            rows: &mut Vec<(u64, Vec<u8>)>,
            k: u64,
        ) {
            let one = vec![(k, b"x".to_vec())];
            insert_internal_cell(page, leaf_with(store, &one), k);
            rows.extend(one);
        }
        let small: Vec<(u64, Vec<u8>)> = (1..=10).map(|k| (k, vec![b's'; 100])).collect();
        let big: Vec<(u64, Vec<u8>)> = (20000..=20010).map(|k| (k, vec![b'b'; 300])).collect();
        let mut page = [0u8; PAGE_SIZE];
        init_internal_page(&mut page, 0);
        insert_internal_cell(&mut page, leaf_with(store, &small), 100);
        insert_internal_cell(&mut page, leaf_with(store, &big), 20010);
        let mut rows: Vec<(u64, Vec<u8>)> = small.into_iter().chain(big).collect();
        // Fill with 9-byte cells (3-byte varint keys), then finish with 10-byte
        // ones (4-byte keys) so that the page is full exactly.
        let mut key = 30_000;
        while free_space_internal(&page) > 200 {
            add(store, &mut page, &mut rows, key);
            key += 10;
        }
        let f = free_space_internal(&page);
        let wide = (0..=f / 10).find(|b| (f - 10 * b) % 9 == 0).unwrap();
        for _ in 0..(f - 10 * wide) / 9 {
            add(store, &mut page, &mut rows, key);
            key += 10;
        }
        key = 3_000_000;
        for _ in 0..wide {
            add(store, &mut page, &mut rows, key);
            key += 10;
        }
        assert_eq!(free_space_internal(&page), 0);
        let last = vec![(key, b"r".to_vec())];
        set_right_child(&mut page, leaf_with(store, &last));
        rows.extend(last);
        (page, rows)
    }

    #[test]
    fn test_btree_delete_splits_interior_page_when_a_divider_grows() {
        // Deleting rowid 1 refills the first leaf from the second; the new divider
        // between them is a 3-byte varint where there was a 1-byte one, which no
        // longer fits on the full interior page above them, so that page splits.
        for as_root in [true, false] {
            let mut store = MemStore::new();
            let (page, mut rows) = full_interior_page(&mut store);
            let full = store.allocate_page().unwrap();
            store.write_page(full, page).unwrap();
            let root = if as_root {
                full
            } else {
                // A root over the full page and a small interior page after it.
                let top = rows.last().unwrap().0;
                let tail = vec![(top + 10, b"t".to_vec()), (top + 20, b"u".to_vec())];
                let mut second = [0u8; PAGE_SIZE];
                init_interior_with(&mut store, &mut second, &tail);
                let second_pn = store.allocate_page().unwrap();
                store.write_page(second_pn, second).unwrap();
                rows.extend(tail);
                let mut root_page = [0u8; PAGE_SIZE];
                init_internal_page(&mut root_page, second_pn);
                insert_internal_cell(&mut root_page, full, top);
                let root = store.allocate_page().unwrap();
                store.write_page(root, root_page).unwrap();
                root
            };
            let all: Vec<u64> = rows.iter().map(|r| r.0).collect();
            assert_eq!(check_tree(&mut store, root), all);
            let depth = tree_depth(&mut store, root);

            assert!(delete(&mut store, root, 1).unwrap());
            assert_eq!(check_tree(&mut store, root), all[1..].to_vec());
            if as_root {
                // The root split into two new children and stayed where it was.
                assert_eq!(tree_depth(&mut store, root), depth + 1);
            } else {
                let top = store.read_page(root).unwrap();
                assert_eq!(cell_count(&top), 2, "the root should have gained a divider");
            }
            for k in 2..=10 {
                assert!(delete(&mut store, root, k).unwrap());
            }
            assert_eq!(check_tree(&mut store, root), all[10..].to_vec());
            for (k, v) in &rows[10..] {
                assert_eq!(search(&mut store, root, *k).unwrap().as_ref(), Some(v));
            }
        }
    }

    /// Make `page` an interior page over one-row leaves holding `rows`: one cell
    /// per row but the last, whose leaf is the right child.
    fn init_interior_with(
        store: &mut MemStore,
        page: &mut [u8; PAGE_SIZE],
        rows: &[(u64, Vec<u8>)],
    ) {
        let (last, cells) = rows.split_last().unwrap();
        init_internal_page(page, leaf_with(store, std::slice::from_ref(last)));
        for r in cells {
            insert_internal_cell(page, leaf_with(store, std::slice::from_ref(r)), r.0);
        }
    }
}
