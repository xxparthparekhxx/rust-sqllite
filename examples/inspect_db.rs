use std::fs;
use std::io::Read;

fn main() {
    let mut f = fs::File::open("test_compat.db").unwrap();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();

    let page_size = 4096usize;
    println!(
        "File size: {} bytes ({} pages on disk)",
        buf.len(),
        buf.len() / page_size
    );

    // Header fields
    println!("\n=== File Header ===");
    let fcc = u32::from_be_bytes([buf[24], buf[25], buf[26], buf[27]]);
    println!("File change counter: {}", fcc);
    let page_count = u32::from_be_bytes([buf[28], buf[29], buf[30], buf[31]]);
    println!("Page count (header): {}", page_count);

    for page_num in 0..(buf.len() / page_size) {
        let page_start = page_num * page_size;
        let bt_off = if page_num == 0 { 100 } else { 0 };
        let abs_off = page_start + bt_off;

        let node_type = buf[abs_off];
        if node_type != 0x0D && node_type != 0x05 {
            println!(
                "\nPage {} (offset {}): not a B-tree page (type=0x{:02X})",
                page_num + 1,
                page_start,
                node_type
            );
            continue;
        }

        let cell_count = u16::from_be_bytes([buf[abs_off + 3], buf[abs_off + 4]]);
        let cell_area = u16::from_be_bytes([buf[abs_off + 5], buf[abs_off + 6]]);

        println!(
            "\nPage {} (offset {}, bt_hdr at {}):",
            page_num + 1,
            page_start,
            abs_off
        );
        println!(
            "  Type: 0x{:02X} ({})",
            node_type,
            if node_type == 0x0D {
                "leaf"
            } else {
                "interior"
            }
        );
        println!("  Cells: {}, Area start: {}", cell_count, cell_area);

        let hdr_size = bt_off + if node_type == 0x0D { 8 } else { 12 };
        let ptr_base = page_start + hdr_size;

        for i in 0..cell_count {
            let p = u16::from_be_bytes([
                buf[ptr_base + i as usize * 2],
                buf[ptr_base + i as usize * 2 + 1],
            ]);
            let cell_abs = page_start + p as usize;
            let end = (cell_abs + 80).min(buf.len());
            println!("  Cell {} at page_offset {} (abs {}):", i, p, cell_abs);
            if cell_abs < buf.len() {
                println!("    {:02X?}", &buf[cell_abs..end]);
                // Try to decode: payload_len (varint), rowid (varint), payload
                let (payload_len, vl1) = decode_varint(&buf[cell_abs..]);
                let (rowid, vl2) = decode_varint(&buf[cell_abs + vl1..]);
                println!(
                    "    payload_len={}, rowid={}, header_overhead={}",
                    payload_len,
                    rowid,
                    vl1 + vl2
                );
                if cell_abs + vl1 + vl2 + payload_len as usize <= buf.len() {
                    let payload =
                        &buf[cell_abs + vl1 + vl2..cell_abs + vl1 + vl2 + payload_len as usize];
                    // Try to decode record header
                    let (rec_hdr_size, rhl) = decode_varint(payload);
                    println!("    record_header_size={}", rec_hdr_size);
                    let mut off = rhl;
                    let mut col = 0;
                    while off < rec_hdr_size as usize {
                        let (st, stl) = decode_varint(&payload[off..]);
                        println!("    col {} serial_type={}", col, st);
                        off += stl;
                        col += 1;
                    }
                }
            }
        }
    }
}

fn decode_varint(buf: &[u8]) -> (u64, usize) {
    let mut val: u64 = 0;
    for i in 0..9 {
        if i >= buf.len() {
            return (val, i);
        }
        if i < 8 {
            val = (val << 7) | (buf[i] as u64 & 0x7F);
            if buf[i] & 0x80 == 0 {
                return (val, i + 1);
            }
        } else {
            val = (val << 8) | buf[i] as u64;
            return (val, 9);
        }
    }
    (val, 9)
}
