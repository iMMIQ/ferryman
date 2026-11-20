// ── sfnt plumbing (TTC face extraction, flavor detection) ───────────────────

fn be16(d: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([d[at], d[at + 1]])
}
fn be32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

/// One table directory record of an sfnt font.
pub(super) struct TableRec {
    offset: usize,
    len: usize,
}

/// Read the table directory of the sfnt starting at `start`.
pub(super) fn sfnt_tables(d: &[u8], start: usize) -> Option<Vec<([u8; 4], TableRec)>> {
    if d.len() < start + 12 {
        return None;
    }
    let num = be16(d, start + 4) as usize;
    if d.len() < start + 12 + num * 16 {
        return None;
    }
    let mut out = Vec::with_capacity(num);
    for i in 0..num {
        let r = start + 12 + i * 16;
        let tag: [u8; 4] = d.get(r..r + 4)?.try_into().ok()?;
        out.push((
            tag,
            TableRec {
                offset: be32(d, r + 8) as usize,
                len: be32(d, r + 12) as usize,
            },
        ));
    }
    Some(out)
}

/// Copy one face out of a TrueType/OpenType collection into a standalone
/// sfnt font (tables byte-identical, only the directory is rebuilt — a PDF
/// FontFile must be a single-face font program). Non-collections return a
/// plain copy of the bytes.
pub(super) fn extract_face(data: &[u8], index: u32) -> Vec<u8> {
    let face_off = (|| {
        if data.len() < 12 || &data[..4] != b"ttcf" {
            return None;
        }
        let num = be32(data, 8);
        if index >= num {
            return None;
        }
        Some(be32(data, 12 + 4 * index as usize) as usize)
    })();
    let Some(face_off) = face_off else {
        return data.to_vec();
    };
    let Some(tables) = sfnt_tables(data, face_off) else {
        return data.to_vec();
    };

    // 12-byte sfnt header + one 16-byte record per table, tables 4-aligned.
    let mut head = Vec::with_capacity(12 + tables.len() * 16);
    head.extend_from_slice(&data[face_off..face_off + 12]);
    let mut body = Vec::new();
    let mut recs: Vec<(&[u8; 4], u32, u32)> = Vec::with_capacity(tables.len());
    for (tag, rec) in &tables {
        if rec.offset + rec.len > data.len() {
            return data.to_vec(); // malformed directory; embed raw bytes
        }
        let new_off = 12 + tables.len() * 16 + body.len();
        recs.push((tag, new_off as u32, rec.len as u32));
        body.extend_from_slice(&data[rec.offset..rec.offset + rec.len]);
        while body.len() % 4 != 0 {
            body.push(0);
        }
    }
    for (tag, off, len) in recs {
        head.extend_from_slice(tag);
        head.extend_from_slice(&0u32.to_be_bytes()); // checksum: stale is fine for PDF
        head.extend_from_slice(&off.to_be_bytes());
        head.extend_from_slice(&len.to_be_bytes());
    }
    let mut out = head;
    out.extend_from_slice(&body);
    out
}

/// Whether the (standalone) font carries CFF outlines ('OTTO' sfnt or a
/// `CFF ` table) vs TrueType `glyf` outlines.
pub(super) fn face_is_cff(f: &[u8]) -> bool {
    if f.len() >= 4 && &f[..4] == b"OTTO" {
        return true;
    }
    sfnt_tables(f, 0)
        .map(|t| t.iter().any(|(tag, _)| tag == b"CFF "))
        .unwrap_or(false)
}
