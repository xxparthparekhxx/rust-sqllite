use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Page size in bytes (4 KB, same as default SQLite).
pub const PAGE_SIZE: usize = 4096;

/// Magic bytes for SQLite 3.
pub const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Maximum number of pages to hold in cache.
const MAX_CACHE_SIZE: usize = 256;

/// Database file header stored at the beginning of page 1 (which SQLite calls page 1, but we use 0-indexed internally in places, wait, let's just stick to 1-indexed pages for SQLite).
/// Total size: 100 bytes.
#[derive(Debug, Clone, Copy)]
pub struct FileHeader {
    pub magic: [u8; 16],
    pub page_size: u16,
    pub write_version: u8,
    pub read_version: u8,
    pub reserved_space: u8,
    pub max_payload_frac: u8,
    pub min_payload_frac: u8,
    pub leaf_payload_frac: u8,
    pub file_change_counter: u32,
    pub page_count: u32,
    pub freelist_trunk_page: u32,
    pub freelist_pages: u32,
    pub schema_cookie: u32,
    pub schema_format: u32,
    pub default_page_cache_size: u32,
    pub largest_root_page: u32,
    pub text_encoding: u32,
    pub user_version: u32,
    pub incremental_vacuum_mode: u32,
    pub application_id: u32,
    pub version_valid_for: u32,
    pub sqlite_version: u32,
}

impl FileHeader {
    pub const SIZE: usize = 100;

    pub fn new() -> Self {
        let mut magic = [0u8; 16];
        magic.copy_from_slice(MAGIC);
        FileHeader {
            magic,
            page_size: PAGE_SIZE as u16,
            write_version: 1, // 1 for legacy rollback journal
            read_version: 1,  // 1 for legacy rollback journal
            reserved_space: 0,
            max_payload_frac: 64,
            min_payload_frac: 32,
            leaf_payload_frac: 32,
            file_change_counter: 0,
            page_count: 0,
            freelist_trunk_page: 0,
            freelist_pages: 0,
            schema_cookie: 0,
            schema_format: 4, // 4 = SQLite 3.3.0 and later
            default_page_cache_size: 0,
            largest_root_page: 0,
            text_encoding: 1, // 1 = UTF-8
            user_version: 0,
            incremental_vacuum_mode: 0,
            application_id: 0,
            version_valid_for: 0,
            sqlite_version: 3042000,
        }
    }

    pub fn serialize(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..16].copy_from_slice(&self.magic);
        buf[16..18].copy_from_slice(&self.page_size.to_be_bytes());
        buf[18] = self.write_version;
        buf[19] = self.read_version;
        buf[20] = self.reserved_space;
        buf[21] = self.max_payload_frac;
        buf[22] = self.min_payload_frac;
        buf[23] = self.leaf_payload_frac;
        buf[24..28].copy_from_slice(&self.file_change_counter.to_be_bytes());
        buf[28..32].copy_from_slice(&self.page_count.to_be_bytes());
        buf[32..36].copy_from_slice(&self.freelist_trunk_page.to_be_bytes());
        buf[36..40].copy_from_slice(&self.freelist_pages.to_be_bytes());
        buf[40..44].copy_from_slice(&self.schema_cookie.to_be_bytes());
        buf[44..48].copy_from_slice(&self.schema_format.to_be_bytes());
        buf[48..52].copy_from_slice(&self.default_page_cache_size.to_be_bytes());
        buf[52..56].copy_from_slice(&self.largest_root_page.to_be_bytes());
        buf[56..60].copy_from_slice(&self.text_encoding.to_be_bytes());
        buf[60..64].copy_from_slice(&self.user_version.to_be_bytes());
        buf[64..68].copy_from_slice(&self.incremental_vacuum_mode.to_be_bytes());
        buf[68..72].copy_from_slice(&self.application_id.to_be_bytes());
        // 72..92 is reserved (zeros)
        buf[92..96].copy_from_slice(&self.version_valid_for.to_be_bytes());
        buf[96..100].copy_from_slice(&self.sqlite_version.to_be_bytes());
        buf
    }

    pub fn deserialize(buf: &[u8; Self::SIZE]) -> io::Result<Self> {
        let mut magic = [0u8; 16];
        magic.copy_from_slice(&buf[0..16]);
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad magic (not a SQLite 3 DB)",
            ));
        }

        let read_u32 = |offset: usize| {
            u32::from_be_bytes([
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
            ])
        };
        let read_u16 = |offset: usize| u16::from_be_bytes([buf[offset], buf[offset + 1]]);

        Ok(FileHeader {
            magic,
            page_size: read_u16(16),
            write_version: buf[18],
            read_version: buf[19],
            reserved_space: buf[20],
            max_payload_frac: buf[21],
            min_payload_frac: buf[22],
            leaf_payload_frac: buf[23],
            file_change_counter: read_u32(24),
            page_count: read_u32(28),
            freelist_trunk_page: read_u32(32),
            freelist_pages: read_u32(36),
            schema_cookie: read_u32(40),
            schema_format: read_u32(44),
            default_page_cache_size: read_u32(48),
            largest_root_page: read_u32(52),
            text_encoding: read_u32(56),
            user_version: read_u32(60),
            incremental_vacuum_mode: read_u32(64),
            application_id: read_u32(68),
            version_valid_for: read_u32(92),
            sqlite_version: read_u32(96),
        })
    }
}

/// A single cached page entry.
#[derive(Clone)]
struct CacheEntry {
    data: [u8; PAGE_SIZE],
    _dirty: bool,
    access_count: u64,
}

/// The pager: manages reading/writing fixed-size pages to/from the database file.
pub struct Pager {
    pub path: PathBuf,
    file: File,
    pub header: FileHeader,
    cache: HashMap<u32, CacheEntry>,
    access_counter: u64,
}

impl Pager {
    /// Open or create a database file.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        let file_len = file.metadata()?.len();
        let header = if file_len == 0 {
            // New database — write header.
            let h = FileHeader::new();
            let mut page0 = [0u8; PAGE_SIZE];
            let hdr = h.serialize();
            page0[..FileHeader::SIZE].copy_from_slice(&hdr);
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&page0)?;
            file.flush()?;
            let mut h2 = h;
            h2.page_count = 1; // page 0 counts
            // Rewrite header with correct page_count.
            let hdr2 = h2.serialize();
            page0[..FileHeader::SIZE].copy_from_slice(&hdr2);
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&page0)?;
            file.flush()?;
            h2
        } else {
            // Existing database — read header.
            let mut hdr_buf = [0u8; FileHeader::SIZE];
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut hdr_buf)?;
            FileHeader::deserialize(&hdr_buf)?
        };

        Ok(Pager {
            path,
            file,
            header,
            cache: HashMap::new(),
            access_counter: 0,
        })
    }

    /// Read a page into a buffer. Returns the page data.
    pub fn read_page(&mut self, page_num: u32) -> io::Result<[u8; PAGE_SIZE]> {
        // Check cache first.
        if let Some(entry) = self.cache.get_mut(&page_num) {
            self.access_counter += 1;
            entry.access_count = self.access_counter;
            return Ok(entry.data);
        }

        // SQLite 1-indexes pages internally (1 is the first page).
        // Our pager historically 0-indexed. Let's convert `page_num` back to 1-indexed to match standard, or adjust offsets.
        // Actually, SQLite's first page is page 1, which resides at offset 0.
        // So `offset = (page_num - 1) * PAGE_SIZE`.
        let offset = ((page_num.max(1) - 1) as u64) * (PAGE_SIZE as u64);
        let file_len = self.file.metadata()?.len();
        let mut data = [0u8; PAGE_SIZE];

        if offset < file_len {
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.read_exact(&mut data)?;
        }
        // else: page doesn't exist on disk yet — return zeroed page.

        self.insert_cache(page_num, data, false);
        Ok(data)
    }

    /// Write a page buffer to the cache (will be flushed later or immediately).
    pub fn write_page(&mut self, page_num: u32, data: [u8; PAGE_SIZE]) -> io::Result<()> {
        let mut writable_data = data;

        // Page 1 always contains the file header in its first 100 bytes.
        if page_num <= 1 {
            let hdr = self.header.serialize();
            writable_data[..FileHeader::SIZE].copy_from_slice(&hdr);
        }

        // Write through to disk immediately.
        let offset = ((page_num.max(1) - 1) as u64) * (PAGE_SIZE as u64);
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&writable_data)?;
        self.file.flush()?;

        // Update header page_count if we extended the file.
        if page_num > self.header.page_count {
            self.header.page_count = page_num;
            self.write_header()?;
        }

        self.insert_cache(page_num, data, false);
        Ok(())
    }

    /// Allocate a new page and return its page number.
    pub fn allocate_page(&mut self) -> io::Result<u32> {
        let page_num = self.header.page_count + 1;
        let data = [0u8; PAGE_SIZE];
        self.write_page(page_num, data)?;
        Ok(page_num)
    }

    /// Flush the file header to page 0.
    pub fn write_header(&mut self) -> io::Result<()> {
        let hdr = self.header.serialize();
        // Read page 0, patch the header bytes, write it back.
        let offset = 0u64;
        let mut page0 = [0u8; PAGE_SIZE];
        let file_len = self.file.metadata()?.len();
        if file_len >= PAGE_SIZE as u64 {
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.read_exact(&mut page0)?;
        }
        page0[..FileHeader::SIZE].copy_from_slice(&hdr);
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page0)?;
        self.file.flush()?;
        Ok(())
    }

    /// Insert a page into the LRU cache, evicting if necessary.
    fn insert_cache(&mut self, page_num: u32, data: [u8; PAGE_SIZE], dirty: bool) {
        self.access_counter += 1;
        if self.cache.len() >= MAX_CACHE_SIZE && !self.cache.contains_key(&page_num) {
            // Evict the least recently used entry.
            let lru_key = self
                .cache
                .iter()
                .min_by_key(|(_, v)| v.access_count)
                .map(|(k, _)| *k);
            if let Some(key) = lru_key {
                self.cache.remove(&key);
            }
        }
        self.cache.insert(
            page_num,
            CacheEntry {
                data,
                _dirty: dirty,
                access_count: self.access_counter,
            },
        );
    }

    /// Invalidate the cache (used after WAL checkpoint).
    pub fn invalidate_cache(&mut self) {
        self.cache.clear();
    }

    /// Reload the header from disk.
    pub fn reload_header(&mut self) -> io::Result<()> {
        let mut hdr_buf = [0u8; FileHeader::SIZE];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut hdr_buf)?;
        self.header = FileHeader::deserialize(&hdr_buf)?;
        Ok(())
    }
}

// ───────────────────────────── Tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rustsqlite_test_{}", name));
        p
    }

    #[test]
    fn test_pager_create_and_header() {
        let p = tmp_path("pager_hdr");
        let _ = fs::remove_file(&p);

        let pager = Pager::open(&p).unwrap();
        assert_eq!(pager.header.page_size, PAGE_SIZE as u16);
        assert_eq!(pager.header.page_count, 1);
        drop(pager);

        // Reopen and verify header.
        let pager2 = Pager::open(&p).unwrap();
        assert_eq!(pager2.header.page_size, PAGE_SIZE as u16);
        drop(pager2);

        let _ = fs::remove_file(&p);
    }

    #[test]
    fn test_pager_read_write() {
        let p = tmp_path("pager_rw");
        let _ = fs::remove_file(&p);

        let mut pager = Pager::open(&p).unwrap();

        // Write page 1 with known data.
        let mut data = [0u8; PAGE_SIZE];
        data[0] = 0xAB;
        data[1] = 0xCD;
        data[PAGE_SIZE - 1] = 0xFF;
        pager.write_page(2, data).unwrap();

        assert_eq!(pager.header.page_count, 2);

        // Read it back.
        let read = pager.read_page(2).unwrap();
        assert_eq!(read[0], 0xAB);
        assert_eq!(read[1], 0xCD);
        assert_eq!(read[PAGE_SIZE - 1], 0xFF);

        drop(pager);

        // Reopen — data should persist.
        let mut pager2 = Pager::open(&p).unwrap();
        let read2 = pager2.read_page(2).unwrap();
        assert_eq!(read2[0], 0xAB);
        assert_eq!(read2[PAGE_SIZE - 1], 0xFF);

        drop(pager2);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn test_pager_allocate() {
        let p = tmp_path("pager_alloc");
        let _ = fs::remove_file(&p);

        let mut pager = Pager::open(&p).unwrap();
        let pg1 = pager.allocate_page().unwrap();
        let pg2 = pager.allocate_page().unwrap();
        assert_eq!(pg1, 2);
        assert_eq!(pg2, 3);
        assert_eq!(pager.header.page_count, 3);

        drop(pager);
        let _ = fs::remove_file(&p);
    }
}
