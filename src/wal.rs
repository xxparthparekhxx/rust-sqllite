use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::pager::PAGE_SIZE;

/// WAL file magic bytes.
const WAL_MAGIC: [u8; 8] = *b"RSWLV1\0\0";

/// WAL file header size in bytes.
const WAL_HEADER_SIZE: usize = 24;

/// Frame header: page_number (4) + commit_flag (1) + reserved (3) = 8 bytes.
const FRAME_HEADER_SIZE: usize = 8;

/// Total frame size on disk.
const FRAME_SIZE: usize = FRAME_HEADER_SIZE + PAGE_SIZE;

/// WAL file header.
#[derive(Debug, Clone)]
struct WalHeader {
    magic: [u8; 8],
    page_size: u32,
    frame_count: u32,
    salt1: u32,
    salt2: u32,
}

impl WalHeader {
    fn new() -> Self {
        WalHeader {
            magic: WAL_MAGIC,
            page_size: PAGE_SIZE as u32,
            frame_count: 0,
            salt1: 0,
            salt2: 0,
        }
    }

    fn serialize(&self) -> [u8; WAL_HEADER_SIZE] {
        let mut buf = [0u8; WAL_HEADER_SIZE];
        buf[0..8].copy_from_slice(&self.magic);
        buf[8..12].copy_from_slice(&self.page_size.to_le_bytes());
        buf[12..16].copy_from_slice(&self.frame_count.to_le_bytes());
        buf[16..20].copy_from_slice(&self.salt1.to_le_bytes());
        buf[20..24].copy_from_slice(&self.salt2.to_le_bytes());
        buf
    }

    fn deserialize(buf: &[u8; WAL_HEADER_SIZE]) -> io::Result<Self> {
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&buf[0..8]);
        if magic != WAL_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad WAL magic"));
        }
        let page_size = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let frame_count = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let salt1 = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let salt2 = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
        Ok(WalHeader {
            magic,
            page_size,
            frame_count,
            salt1,
            salt2,
        })
    }
}

/// The Write-Ahead Log.
pub struct Wal {
    wal_path: PathBuf,
    db_path: PathBuf,
    file: File,
    header: WalHeader,
    /// Index: page_num -> list of frame offsets (latest last).
    index: std::collections::HashMap<u32, Vec<u32>>,
}

impl Wal {
    /// Open or create a WAL file. If an existing WAL has committed frames,
    /// they are indexed for read access. Uncommitted tail frames are discarded.
    pub fn open<P: AsRef<Path>>(db_path: P) -> io::Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
        let exists = wal_path.exists() && fs::metadata(&wal_path)?.len() > WAL_HEADER_SIZE as u64;

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&wal_path)?;

        let (header, index) = if exists {
            // Read existing WAL and rebuild index.
            let mut hdr_buf = [0u8; WAL_HEADER_SIZE];
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut hdr_buf)?;
            let header = WalHeader::deserialize(&hdr_buf)?;

            let index = Self::rebuild_index(&mut file, &header)?;
            (header, index)
        } else {
            // Create fresh WAL.
            let header = WalHeader::new();
            file.seek(SeekFrom::Start(0))?;
            file.set_len(0)?;
            file.write_all(&header.serialize())?;
            file.flush()?;
            (header, std::collections::HashMap::new())
        };

        Ok(Wal {
            wal_path,
            db_path,
            file,
            header,
            index,
        })
    }

    /// Rebuild the WAL index by reading all frames.
    /// Only includes frames up to and including the last commit frame.
    fn rebuild_index(
        file: &mut File,
        header: &WalHeader,
    ) -> io::Result<std::collections::HashMap<u32, Vec<u32>>> {
        let mut index: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
        let mut pending: Vec<(u32, u32)> = Vec::new(); // (frame_index, page_num)

        for i in 0..header.frame_count {
            let offset = WAL_HEADER_SIZE as u64 + (i as u64) * (FRAME_SIZE as u64);
            let mut fh_buf = [0u8; FRAME_HEADER_SIZE];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut fh_buf)?;

            let page_num = u32::from_le_bytes([fh_buf[0], fh_buf[1], fh_buf[2], fh_buf[3]]);
            let is_commit = fh_buf[4] != 0;

            pending.push((i, page_num));

            if is_commit {
                // All pending frames are now committed.
                for (fi, pn) in pending.drain(..) {
                    index.entry(pn).or_default().push(fi);
                }
            }
        }
        // Any remaining pending frames are uncommitted — discard them.
        Ok(index)
    }

    /// Write a frame to the WAL.
    pub fn write_frame(
        &mut self,
        page_num: u32,
        data: &[u8; PAGE_SIZE],
        is_commit: bool,
    ) -> io::Result<()> {
        let frame_index = self.header.frame_count;
        let offset = WAL_HEADER_SIZE as u64 + (frame_index as u64) * (FRAME_SIZE as u64);

        // Build frame header.
        let mut fh = [0u8; FRAME_HEADER_SIZE];
        fh[0..4].copy_from_slice(&page_num.to_le_bytes());
        fh[4] = if is_commit { 1 } else { 0 };

        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&fh)?;
        self.file.write_all(data)?;

        self.header.frame_count += 1;
        // Rewrite header to update frame_count.
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&self.header.serialize())?;

        // fsync so commit frames survive a power cut.
        if is_commit {
            self.file.sync_all()?;
        } else {
            self.file.flush()?;
        }

        if is_commit {
            // Index all frames since last commit.
            self.index.entry(page_num).or_default().push(frame_index);
        }

        Ok(())
    }

    /// Batch-commit: mark multiple page writes and finalize with a commit.
    /// `pages` is a list of (page_num, data). The last frame gets the commit flag.
    pub fn write_transaction(&mut self, pages: &[(u32, [u8; PAGE_SIZE])]) -> io::Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let last = pages.len() - 1;
        let first_frame_index = self.header.frame_count;
        for (i, (page_num, data)) in pages.iter().enumerate() {
            let is_commit = i == last;
            let frame_index = self.header.frame_count;
            let offset = WAL_HEADER_SIZE as u64 + (frame_index as u64) * (FRAME_SIZE as u64);

            let mut fh = [0u8; FRAME_HEADER_SIZE];
            fh[0..4].copy_from_slice(&page_num.to_le_bytes());
            fh[4] = if is_commit { 1 } else { 0 };

            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&fh)?;
            self.file.write_all(data)?;

            self.header.frame_count += 1;
        }
        self.file.flush()?;
        // Rewrite header.
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&self.header.serialize())?;

        // fsync so the commit frame survives a power cut.
        self.file.sync_all()?;

        // Index the committed frames.
        for (i, (page_num, _)) in pages.iter().enumerate() {
            let fi = first_frame_index + i as u32;
            self.index.entry(*page_num).or_default().push(fi);
        }
        Ok(())
    }

    /// Read the latest version of a page from the WAL.
    /// Returns `None` if the page is not in the WAL.
    pub fn read_page(&mut self, page_num: u32) -> io::Result<Option<[u8; PAGE_SIZE]>> {
        if let Some(frames) = self.index.get(&page_num) {
            if let Some(&last_frame) = frames.last() {
                let offset = WAL_HEADER_SIZE as u64
                    + (last_frame as u64) * (FRAME_SIZE as u64)
                    + FRAME_HEADER_SIZE as u64;
                let mut data = [0u8; PAGE_SIZE];
                self.file.seek(SeekFrom::Start(offset))?;
                self.file.read_exact(&mut data)?;
                return Ok(Some(data));
            }
        }
        Ok(None)
    }

    /// Checkpoint: copy all committed WAL frames back to the main database file,
    /// then truncate the WAL.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        let mut db_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.db_path)?;

        // For each page in the index, write the latest frame to the DB.
        for (&page_num, frames) in &self.index {
            if let Some(&last_frame) = frames.last() {
                let wal_offset = WAL_HEADER_SIZE as u64
                    + (last_frame as u64) * (FRAME_SIZE as u64)
                    + FRAME_HEADER_SIZE as u64;
                let mut data = [0u8; PAGE_SIZE];
                self.file.seek(SeekFrom::Start(wal_offset))?;
                self.file.read_exact(&mut data)?;

                let db_offset = ((page_num.max(1) - 1) as u64) * (PAGE_SIZE as u64);
                db_file.seek(SeekFrom::Start(db_offset))?;
                db_file.write_all(&data)?;
            }
        }

        // Update the file header with correct page_count and file_change_counter.
        {
            let file_len = db_file.metadata()?.len();
            let actual_pages = (file_len / PAGE_SIZE as u64) as u32;

            // Read current header from page 1.
            let mut hdr_buf = [0u8; 100];
            db_file.seek(SeekFrom::Start(0))?;
            db_file.read_exact(&mut hdr_buf)?;

            // Update page_count (bytes 28-31).
            hdr_buf[28..32].copy_from_slice(&actual_pages.to_be_bytes());

            // Increment file_change_counter (bytes 24-27).
            let fcc = u32::from_be_bytes([hdr_buf[24], hdr_buf[25], hdr_buf[26], hdr_buf[27]]);
            let new_fcc = fcc.wrapping_add(1);
            hdr_buf[24..28].copy_from_slice(&new_fcc.to_be_bytes());

            // Set version_valid_for = file_change_counter (bytes 92-95).
            hdr_buf[92..96].copy_from_slice(&new_fcc.to_be_bytes());

            // Write header back.
            db_file.seek(SeekFrom::Start(0))?;
            db_file.write_all(&hdr_buf)?;
        }

        db_file.flush()?;
        drop(db_file);

        // Reset the WAL.
        self.header.frame_count = 0;
        self.header.salt1 = self.header.salt1.wrapping_add(1);
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&self.header.serialize())?;
        self.file.flush()?;

        self.index.clear();
        Ok(())
    }

    /// Returns true if there are committed frames in the WAL.
    pub fn has_frames(&self) -> bool {
        !self.index.is_empty()
    }

    /// Number of committed frames.
    pub fn committed_frame_count(&self) -> usize {
        self.index.values().map(|v| v.len()).sum()
    }

    /// Remove the WAL file from disk.
    pub fn remove(self) -> io::Result<()> {
        drop(self.file);
        if self.wal_path.exists() {
            fs::remove_file(&self.wal_path)?;
        }
        Ok(())
    }

    /// Return the path of the WAL file.
    pub fn path(&self) -> &Path {
        &self.wal_path
    }
}

// ───────────────────────────── Tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pager::Pager;

    fn tmp_db(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rustsqlite_test_{}", name));
        p
    }

    fn cleanup(db_path: &Path) {
        let _ = fs::remove_file(db_path);
        let wal = PathBuf::from(format!("{}-wal", db_path.display()));
        let _ = fs::remove_file(&wal);
    }

    #[test]
    fn test_wal_write_and_read() {
        let db = tmp_db("wal_rw");
        cleanup(&db);

        // Create the DB file.
        let _pager = Pager::open(&db).unwrap();
        drop(_pager);

        let mut wal = Wal::open(&db).unwrap();

        let mut page_data = [0u8; PAGE_SIZE];
        page_data[0] = 42;
        page_data[100] = 99;

        wal.write_frame(5, &page_data, true).unwrap();

        let read = wal.read_page(5).unwrap().unwrap();
        assert_eq!(read[0], 42);
        assert_eq!(read[100], 99);

        // Non-existent page.
        assert!(wal.read_page(999).unwrap().is_none());

        drop(wal);
        cleanup(&db);
    }

    #[test]
    fn test_wal_checkpoint() {
        let db = tmp_db("wal_cp");
        cleanup(&db);

        // Create the DB file with pager.
        let _pager = Pager::open(&db).unwrap();
        drop(_pager);

        let mut wal = Wal::open(&db).unwrap();

        let mut data = [0u8; PAGE_SIZE];
        data[0] = 0xBE;
        data[1] = 0xEF;
        wal.write_frame(2, &data, true).unwrap();
        assert!(wal.has_frames());

        wal.checkpoint().unwrap();
        assert!(!wal.has_frames());

        // Now read directly from the DB file — the page should be there.
        let mut pager = Pager::open(&db).unwrap();
        let page = pager.read_page(2).unwrap();
        assert_eq!(page[0], 0xBE);
        assert_eq!(page[1], 0xEF);

        drop(pager);
        drop(wal);
        cleanup(&db);
    }

    #[test]
    fn test_wal_recovery() {
        let db = tmp_db("wal_rec");
        cleanup(&db);

        // Create DB.
        let _pager = Pager::open(&db).unwrap();
        drop(_pager);

        // Write some committed frames.
        {
            let mut wal = Wal::open(&db).unwrap();
            let mut d1 = [0u8; PAGE_SIZE];
            d1[0] = 11;
            wal.write_frame(3, &d1, true).unwrap();
            // Simulate crash — drop without checkpoint.
        }

        // Reopen WAL — committed frames should be indexed.
        {
            let mut wal = Wal::open(&db).unwrap();
            let read = wal.read_page(3).unwrap().unwrap();
            assert_eq!(read[0], 11);

            // Checkpoint to move data into DB.
            wal.checkpoint().unwrap();
        }

        // Verify data is now in DB.
        let mut pager = Pager::open(&db).unwrap();
        let page = pager.read_page(3).unwrap();
        assert_eq!(page[0], 11);

        drop(pager);
        cleanup(&db);
    }

    #[test]
    fn test_wal_uncommitted_discarded() {
        let db = tmp_db("wal_uncommit");
        cleanup(&db);

        let _pager = Pager::open(&db).unwrap();
        drop(_pager);

        {
            let mut wal = Wal::open(&db).unwrap();
            // Write a frame WITHOUT commit.
            let mut d = [0u8; PAGE_SIZE];
            d[0] = 77;
            wal.write_frame(4, &d, false).unwrap();
            // Drop — "crash".
        }

        // Reopen — uncommitted frame should be absent.
        {
            let mut wal = Wal::open(&db).unwrap();
            assert!(wal.read_page(4).unwrap().is_none());
        }

        cleanup(&db);
    }

    #[test]
    fn test_wal_transaction_batch() {
        let db = tmp_db("wal_txn");
        cleanup(&db);

        let _pager = Pager::open(&db).unwrap();
        drop(_pager);

        let mut wal = Wal::open(&db).unwrap();

        let mut d1 = [0u8; PAGE_SIZE];
        d1[0] = 1;
        let mut d2 = [0u8; PAGE_SIZE];
        d2[0] = 2;

        wal.write_transaction(&[(10, d1), (11, d2)]).unwrap();

        assert_eq!(wal.read_page(10).unwrap().unwrap()[0], 1);
        assert_eq!(wal.read_page(11).unwrap().unwrap()[0], 2);

        drop(wal);
        cleanup(&db);
    }
}
