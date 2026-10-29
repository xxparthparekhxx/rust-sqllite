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
                    let rc = right_child(&page);

                    // Find the cell referencing child_page, replace its separator key with
                    // median_key (max key of the now-smaller child_page), and insert
                    // (new_page, old_key) right after it (old_key = max key of new_page).
                    let child_idx = cells
                        .iter()
                        .position(|(cp, _)| *cp == child_page)
                        .expect("child_page must be referenced by a cell");
                    let old_key = cells[child_idx].1;
                    cells[child_idx] = (child_page, median_key);
                    cells.insert(child_idx + 1, (new_page, old_key));

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

/// Delete a key from the tree. Returns true if the key was found and deleted.
pub fn delete(store: &mut dyn PageStore, root_page: u32, key: u64) -> io::Result<bool> {
    let mut page = store.read_page(root_page)?;
    if is_leaf(&page) {
        let cells = collect_leaf_cells(&page);
        let orig_len = cells.len();
        let new_cells: Vec<(u64, Vec<u8>)> = cells.into_iter().filter(|(k, _)| *k != key).collect();
        if new_cells.len() == orig_len {
            return Ok(false);
        }
        rebuild_leaf_page(&mut page, &new_cells);
        store.write_page(root_page, page)?;
        Ok(true)
    } else {
        let n = cell_count(&page);
        for i in 0..n {
            let off = cell_ptr(&page, i) as usize;
            let (child, k) = read_internal_cell(&page, off);
            if key <= k {
                return delete(store, child, key);
            }
        }
        let rc = right_child(&page);
        delete(store, rc, key)
    }
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
    use std::collections::HashMap;

    /// In-memory page store for testing.
    struct MemStore {
        pages: HashMap<u32, [u8; PAGE_SIZE]>,
        next_page: u32,
    }

    impl MemStore {
        fn new() -> Self {
            MemStore {
                pages: HashMap::new(),
                next_page: 0,
            }
        }
    }

    impl PageStore for MemStore {
        fn read_page(&mut self, page_num: u32) -> io::Result<[u8; PAGE_SIZE]> {
            Ok(self
                .pages
                .get(&page_num)
                .copied()
                .unwrap_or([0u8; PAGE_SIZE]))
        }
        fn write_page(&mut self, page_num: u32, data: [u8; PAGE_SIZE]) -> io::Result<()> {
            self.pages.insert(page_num, data);
            Ok(())
        }
        fn allocate_page(&mut self) -> io::Result<u32> {
            let p = self.next_page;
            self.next_page += 1;
            Ok(p)
        }
        fn free_page(&mut self, page_num: u32) -> io::Result<()> {
            self.pages.remove(&page_num);
            Ok(())
        }
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
}
