//! Fast indexer: reads the NTFS master file table straight from the volume, the same technique
//! Everything uses. Opening `\\.\C:` for reading requires administrator rights.

use crate::index::{ATTR_DIRECTORY, NONE, Source, VolumeBuilder, VolumeIndex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

const ROOT_RECORD: usize = 5;
/// Records below this number are NTFS metadata files ($MFT, $LogFile, $Extend, …).
const FIRST_USER_RECORD: usize = 16;
const READ_CHUNK: usize = 4 << 20;
/// Fixed FILE record header (through the record number at 0x2C); shorter records are rejected.
const RECORD_HEADER_LEN: usize = 0x30;
/// Most $MFT records read (about 2.5 GB of record state). Bigger volumes fall back to the
/// folder scan instead of allocating without a bound.
const MAX_MFT_RECORDS: u64 = 64 << 20;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;
const NAMESPACE_DOS: u8 = 2;
const RECORD_IN_USE: u16 = 0x1;
const RECORD_IS_DIRECTORY: u16 = 0x2;
const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[derive(Clone, Copy, Default)]
struct Rec {
    parent: u64,
    name_off: u32,
    name_len: u16,
    name_ns: u8,
    has_name: bool,
    in_use: bool,
    is_dir: bool,
    attrs: u32,
    size: u64,
    mtime: i64,
}

#[cfg(windows)]
pub fn read_volume(
    letter: char,
    progress: &AtomicUsize,
    total: &AtomicUsize,
    cancel: &AtomicBool,
) -> anyhow::Result<VolumeIndex> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let start = Instant::now();
    let volume = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(format!(r"\\.\{letter}:"))?;
    let info = volume_data(&volume)?;

    let geometry = || anyhow::anyhow!("unsupported NTFS geometry");
    let cluster = info.BytesPerCluster as u64;
    let record_size = info.BytesPerFileRecordSegment as usize;
    let total_clusters = u64::try_from(info.TotalClusters).map_err(|_| geometry())?;
    if record_size < RECORD_HEADER_LEN || cluster == 0 || !READ_CHUNK.is_multiple_of(record_size) {
        return Err(geometry());
    }
    let volume_bytes = total_clusters.checked_mul(cluster).ok_or_else(geometry)?;
    let mft_start = u64::try_from(info.MftStartLcn).ok().filter(|&lcn| lcn < total_clusters).ok_or_else(geometry)?;
    let mft_bytes = u64::try_from(info.MftValidDataLength).ok().filter(|&n| n <= volume_bytes).ok_or_else(geometry)?;

    let mut rec0 = vec![0u8; record_size];
    read_exact_at(&volume, &mut rec0, mft_start * cluster)?;
    if !apply_fixup(&mut rec0) {
        anyhow::bail!("corrupt $MFT record");
    }
    let runs = mft_runs(&rec0, total_clusters).ok_or_else(|| anyhow::anyhow!("cannot locate valid $MFT data runs"))?;

    let record_count = mft_bytes / record_size as u64;
    if record_count > MAX_MFT_RECORDS {
        anyhow::bail!("$MFT has more than {MAX_MFT_RECORDS} records");
    }
    let record_count = record_count as usize;
    total.store(record_count, Ordering::Relaxed);
    let mut recs = vec![Rec::default(); record_count];
    let mut names = String::with_capacity(record_count * 16);
    let mut buf = vec![0u8; READ_CHUNK];
    let mut frn = 0usize;

    // `mft_runs` keeps every run inside the volume, so these byte offsets cannot overflow.
    for (lcn, clusters) in runs {
        let mut remaining = clusters * cluster;
        let Some(lcn) = lcn else {
            // Sparse run: no records stored.
            let skipped = usize::try_from(remaining / record_size as u64).unwrap_or(usize::MAX);
            frn = frn.saturating_add(skipped).min(record_count);
            continue;
        };
        let mut offset = lcn * cluster;
        while remaining > 0 && frn < record_count {
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("cancelled");
            }
            let want = remaining
                .min(READ_CHUNK as u64)
                .min((record_count - frn) as u64 * record_size as u64) as usize;
            read_exact_at(&volume, &mut buf[..want], offset)?;
            for record in buf[..want].chunks_exact_mut(record_size) {
                parse_record(record, frn, &mut recs, &mut names);
                frn += 1;
            }
            offset += want as u64;
            remaining -= want as u64;
            progress.store(frn, Ordering::Relaxed);
        }
    }

    Ok(build(letter, &recs, &names, start))
}

#[cfg(not(windows))]
pub fn read_volume(_: char, _: &AtomicUsize, _: &AtomicUsize, _: &AtomicBool) -> anyhow::Result<VolumeIndex> {
    anyhow::bail!("MFT indexing is only available on Windows")
}

#[cfg(windows)]
fn volume_data(
    volume: &std::fs::File,
) -> std::io::Result<windows_sys::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};

    // SAFETY: plain-old-data struct, fully written by the call on success.
    let mut out: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: valid volume handle and an output buffer of the advertised size.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle(),
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            (&mut out as *mut NTFS_VOLUME_DATA_BUFFER).cast(),
            size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if (returned as usize) < size_of::<NTFS_VOLUME_DATA_BUFFER>() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "short NTFS volume data"));
    }
    Ok(out)
}

#[cfg(windows)]
fn read_exact_at(file: &std::fs::File, buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
        offset += n as u64;
    }
    Ok(())
}

#[inline]
fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

#[inline]
fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// Restores the last two bytes of every stride from the update sequence array, verifying that
/// each stride carries the expected sequence number (detects torn writes).
fn apply_fixup(record: &mut [u8]) -> bool {
    if record.len() < 8 || &record[0..4] != b"FILE" {
        return false;
    }
    let usa_off = u16_at(record, 4) as usize;
    let usa_count = u16_at(record, 6) as usize;
    if usa_count < 2 || usa_off + usa_count * 2 > record.len() {
        return false;
    }
    let stride = record.len() / (usa_count - 1);
    let usn = [record[usa_off], record[usa_off + 1]];
    for i in 1..usa_count {
        let pos = i * stride - 2;
        if record[pos..pos + 2] != usn {
            return false;
        }
        record[pos] = record[usa_off + 2 * i];
        record[pos + 1] = record[usa_off + 2 * i + 1];
    }
    true
}

/// Iterates `(type, attribute bytes)` over a fixed-up record.
fn attributes(record: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut off = if record.len() >= 0x16 { u16_at(record, 0x14) as usize } else { usize::MAX };
    std::iter::from_fn(move || {
        if off.checked_add(16).is_none_or(|end| end > record.len()) {
            return None;
        }
        let ty = u32_at(record, off);
        if ty == ATTR_END {
            return None;
        }
        let len = u32_at(record, off + 4) as usize;
        if len < 16 || off + len > record.len() {
            return None;
        }
        let attr = &record[off..off + len];
        off += len;
        Some((ty, attr))
    })
}

fn resident_value(attr: &[u8]) -> Option<&[u8]> {
    if attr[8] != 0 || attr.len() < 0x18 {
        return None;
    }
    let len = u32_at(attr, 0x10) as usize;
    let off = u16_at(attr, 0x14) as usize;
    attr.get(off..off.checked_add(len)?)
}

fn parse_record(record: &mut [u8], frn: usize, recs: &mut [Rec], names: &mut String) {
    // The flags (0x16) and base reference (0x20) below must lie inside the record.
    if record.len() < RECORD_HEADER_LEN || !apply_fixup(record) {
        return;
    }
    let flags = u16_at(record, 0x16);
    if flags & RECORD_IN_USE == 0 {
        return;
    }
    // Extension records hold overflow attributes of a base record.
    let base = (u64_at(record, 0x20) & REF_MASK) as usize;
    let target = if base != 0 { base } else { frn };
    let Some(rec) = recs.get_mut(target) else { return };
    if base == 0 {
        rec.in_use = true;
        rec.is_dir = flags & RECORD_IS_DIRECTORY != 0;
    }

    for (ty, attr) in attributes(record) {
        let non_resident = attr[8] != 0;
        let attr_name_len = attr[9];
        match ty {
            ATTR_STANDARD_INFORMATION => {
                if let Some(v) = resident_value(attr).filter(|v| v.len() >= 0x24) {
                    rec.mtime = u64_at(v, 0x08) as i64;
                    rec.attrs = u32_at(v, 0x20);
                }
            }
            ATTR_FILE_NAME => {
                let Some(v) = resident_value(attr).filter(|v| v.len() >= 0x42) else { continue };
                let name_chars = v[0x40] as usize;
                let namespace = v[0x41];
                if v.len() < 0x42 + name_chars * 2 {
                    continue;
                }
                let better = !rec.has_name || (rec.name_ns == NAMESPACE_DOS && namespace != NAMESPACE_DOS);
                if !better {
                    continue;
                }
                let units: Vec<u16> = v[0x42..0x42 + name_chars * 2]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| u16::from_le_bytes(c))
                    .collect();
                let name = String::from_utf16_lossy(&units);
                if names.len() + name.len() > u32::MAX as usize {
                    continue; // name offsets are 32-bit
                }
                rec.name_off = names.len() as u32;
                rec.name_len = name.len().min(u16::MAX as usize) as u16;
                names.push_str(&name);
                rec.name_ns = namespace;
                rec.has_name = true;
                rec.parent = u64_at(v, 0) & REF_MASK;
            }
            ATTR_DATA if attr_name_len == 0 => {
                if non_resident {
                    // Only the first extent (starting VCN 0) carries the real size.
                    if attr.len() >= 0x38 && u64_at(attr, 0x10) == 0 {
                        rec.size = u64_at(attr, 0x30);
                    }
                } else if attr.len() >= 0x14 {
                    rec.size = u32_at(attr, 0x10) as u64;
                }
            }
            _ => {}
        }
    }
}

/// Decodes the data runs of the unnamed $DATA attribute in $MFT's own record:
/// `(starting LCN or None when sparse, length in clusters)`. Fails on an empty run, a negative or
/// overflowing LCN, or a run that does not fit inside a volume of `total_clusters`.
fn mft_runs(rec0: &[u8], total_clusters: u64) -> Option<Vec<(Option<u64>, u64)>> {
    let (_, attr) = attributes(rec0).find(|(ty, a)| *ty == ATTR_DATA && a[8] != 0 && a[9] == 0)?;
    if attr.len() < 0x22 {
        return None;
    }
    let mut pos = u16_at(attr, 0x20) as usize;
    let mut lcn: i64 = 0;
    let mut runs = Vec::new();
    while pos < attr.len() {
        let header = attr[pos];
        if header == 0 {
            break;
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        pos += 1;
        if len_size == 0 || len_size > 8 || off_size > 8 || pos + len_size + off_size > attr.len() {
            return None;
        }
        let mut length = 0u64;
        for i in 0..len_size {
            length |= (attr[pos + i] as u64) << (8 * i);
        }
        pos += len_size;
        if length == 0 {
            return None;
        }
        if off_size == 0 {
            runs.push((None, length));
        } else {
            let mut delta = 0i64;
            for i in 0..off_size {
                delta |= (attr[pos + i] as i64) << (8 * i);
            }
            // Sign-extend the variable-width delta.
            let shift = 64 - 8 * off_size as u32;
            delta = (delta << shift) >> shift;
            lcn = lcn.checked_add(delta)?;
            let start = u64::try_from(lcn).ok()?;
            if start.checked_add(length)? > total_clusters {
                return None;
            }
            runs.push((Some(start), length));
        }
        pos += off_size;
    }
    (!runs.is_empty()).then_some(runs)
}

/// Keeps records reachable from the root through valid parents and turns them into an index.
fn build(letter: char, recs: &[Rec], names: &str, start: Instant) -> VolumeIndex {
    const UNKNOWN: u8 = 0;
    const VALID: u8 = 1;
    const INVALID: u8 = 2;
    const VISITING: u8 = 3;

    let n = recs.len();
    let mut state = vec![UNKNOWN; n];
    if ROOT_RECORD < n {
        state[ROOT_RECORD] = VALID;
    }
    let mut stack = Vec::new();
    for i in 0..n {
        let mut cur = i;
        while state[cur] == UNKNOWN {
            let r = &recs[cur];
            let parent = r.parent as usize;
            // No depth limit: a loop ends at a VISITING record and is rejected, so deep folders
            // are indexed like the folder scan does instead of being dropped.
            if !r.in_use || !r.has_name || cur < FIRST_USER_RECORD || parent >= n {
                state[cur] = INVALID;
                break;
            }
            state[cur] = VISITING;
            stack.push(cur);
            cur = parent;
        }
        let verdict = if state[cur] == VALID { VALID } else { INVALID };
        for s in stack.drain(..) {
            state[s] = verdict;
        }
    }

    let mut ids = vec![NONE; n];
    if ROOT_RECORD < n {
        ids[ROOT_RECORD] = 0;
    }
    let mut next = 1u32;
    for i in 0..n {
        if state[i] == VALID && i != ROOT_RECORD {
            ids[i] = next;
            next += 1;
        }
    }

    let mut builder = VolumeBuilder::new(letter);
    builder.reserve(next as usize);
    for i in 0..n {
        if ids[i] == NONE || i == ROOT_RECORD {
            continue;
        }
        let r = &recs[i];
        let name = names
            .get(r.name_off as usize..r.name_off as usize + r.name_len as usize)
            .unwrap_or("?");
        // Bit 0x10000000 in $STANDARD_INFORMATION marks directories but is not a Win32 attribute.
        let mut attrs = r.attrs & 0x0FFF_FFFF;
        if r.is_dir {
            attrs |= ATTR_DIRECTORY;
        }
        let size = if r.is_dir { 0 } else { r.size };
        builder.push(name, ids[r.parent as usize], attrs, size, r.mtime);
    }
    builder.finish(Source::Mft, start.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: usize = 1024;

    /// Builds a fixed-up-on-disk style FILE record with the given attributes.
    fn record(flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; RECORD];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes()); // update sequence array offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 USN + 2 strides
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut off = 0x38;
        for a in attrs {
            r[off..off + a.len()].copy_from_slice(a);
            off += a.len();
        }
        r[off..off + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        // Move the real stride tails into the USA and stamp the sequence number.
        r[0x30..0x32].copy_from_slice(&7u16.to_le_bytes());
        for i in 1..3 {
            let pos = i * 512 - 2;
            r[0x30 + 2 * i] = r[pos];
            r[0x30 + 2 * i + 1] = r[pos + 1];
            r[pos..pos + 2].copy_from_slice(&7u16.to_le_bytes());
        }
        r
    }

    fn resident(ty: u32, value: &[u8]) -> Vec<u8> {
        let len = (0x18 + value.len()).next_multiple_of(8);
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a[0x18..0x18 + value.len()].copy_from_slice(value);
        a
    }

    fn file_name(parent: u64, name: &str, namespace: u8) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + units.len() * 2];
        v[0..8].copy_from_slice(&(parent | (1 << 48)).to_le_bytes()); // sequence number in the high bits
        v[0x40] = units.len() as u8;
        v[0x41] = namespace;
        for (i, u) in units.iter().enumerate() {
            v[0x42 + 2 * i..0x44 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        resident(ATTR_FILE_NAME, &v)
    }

    fn standard_info(mtime: i64, attrs: u32) -> Vec<u8> {
        let mut v = vec![0u8; 0x48];
        v[8..16].copy_from_slice(&mtime.to_le_bytes());
        v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
        resident(ATTR_STANDARD_INFORMATION, &v)
    }

    fn non_resident_data(real_size: u64, runs: &[u8]) -> Vec<u8> {
        let len = (0x40 + runs.len() + 1).next_multiple_of(8);
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&real_size.to_le_bytes());
        a[0x40..0x40 + runs.len()].copy_from_slice(runs);
        a
    }

    #[test]
    fn decodes_data_runs_with_negative_deltas() {
        // run 1: 0x20 clusters at LCN 0x1000; run 2: 0x10 clusters at LCN 0x1000 - 0x100; sparse run of 8
        let runs = [0x21, 0x20, 0x00, 0x10, 0x21, 0x10, 0x00, 0xFF, 0x01, 0x08];
        let mut rec = record(RECORD_IN_USE, 0, &[non_resident_data(0, &runs)]);
        assert!(apply_fixup(&mut rec));
        let decoded = mft_runs(&rec, 0x2000).unwrap();
        assert_eq!(decoded, vec![(Some(0x1000), 0x20), (Some(0x0F00), 0x10), (None, 8)]);
    }

    /// FC-10: invalid run geometry fails closed instead of wrapping or panicking.
    #[test]
    fn rejects_invalid_data_runs() {
        let runs = |bytes: &[u8], total_clusters: u64| {
            let mut rec = record(RECORD_IN_USE, 0, &[non_resident_data(0, bytes)]);
            assert!(apply_fixup(&mut rec));
            mft_runs(&rec, total_clusters)
        };
        // A negative LCN used to become u64::MAX.
        assert_eq!(runs(&[0x11, 0x01, 0xFF], 0x2000), None);
        // i64::MAX then +1 used to overflow (a panic with overflow checks).
        let mut overflow = vec![0x81, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
        overflow.extend([0x11, 0x01, 0x01]);
        assert_eq!(runs(&overflow, u64::MAX), None);
        // Zero-length run.
        assert_eq!(runs(&[0x11, 0x00, 0x05], 0x2000), None);
        // 0x20 clusters at LCN 0x1000 do not fit a volume of 0x1010 clusters.
        assert_eq!(runs(&[0x21, 0x20, 0x00, 0x10], 0x1010), None);
        assert_eq!(runs(&[0x21, 0x20, 0x00, 0x10], 0x1020), Some(vec![(Some(0x1000), 0x20)]));
    }

    /// FC-10: a record shorter than the fixed header is ignored rather than read out of bounds.
    #[test]
    fn short_records_are_ignored() {
        let mut short = vec![0u8; 0x20];
        short[0..4].copy_from_slice(b"FILE");
        short[4..6].copy_from_slice(&8u16.to_le_bytes());
        short[6..8].copy_from_slice(&2u16.to_le_bytes());
        short[0x16..0x18].copy_from_slice(&RECORD_IN_USE.to_le_bytes()); // so the base reference at 0x20 is read
        short[0x1E..0x20].copy_from_slice(&[0, 0]); // matches the zero sequence number at 8
        let mut recs = vec![Rec::default(); 4];
        parse_record(&mut short, 1, &mut recs, &mut String::new());
        assert!(!recs[1].in_use);
    }

    #[test]
    fn rejects_torn_records() {
        let mut rec = record(RECORD_IN_USE, 0, &[]);
        rec[1022] ^= 0xFF;
        assert!(!apply_fixup(&mut rec));
    }

    #[test]
    fn parses_records_into_a_tree() {
        let n = 20;
        let mut recs = vec![Rec::default(); n];
        let mut names = String::new();
        let dir = RECORD_IN_USE | RECORD_IS_DIRECTORY;

        let mut raw = [
            (5, record(dir, 0, &[standard_info(1, 0), file_name(5, ".", 3)])),
            (16, record(dir, 0, &[standard_info(2, 0x10000000), file_name(5, "Users", 3)])),
            // DOS short name first, then the long name: the long name must win.
            (17, record(RECORD_IN_USE, 0, &[standard_info(3, 0x20), file_name(16, "REPORT~1.DOC", 2), file_name(16, "Report final.docx", 1)])),
            // Data attribute stored in an extension record (18) of base record 17.
            (18, record(RECORD_IN_USE, 17, &[non_resident_data(12345, &[0x11, 0x01, 0x05])])),
            // Child of a metadata file ($Extend = 11) must be dropped.
            (19, record(RECORD_IN_USE, 0, &[standard_info(4, 0), file_name(11, "$ObjId", 3)])),
        ];
        for (frn, rec) in raw.iter_mut() {
            parse_record(rec, *frn, &mut recs, &mut names);
        }

        let v = build('C', &recs, &names, Instant::now());
        assert_eq!(v.len(), 2);
        let report = v.by_name.iter().copied().find(|&i| v.name(i) == "Report final.docx").unwrap();
        assert_eq!(v.full_path(report), r"C:\Users\Report final.docx");
        assert_eq!(v.entry(report).size, 12345);
        assert_eq!(v.entry(report).mtime, 3);
        let users = v.entry(v.entry(report).parent);
        assert!(users.is_dir());
        assert_eq!(users.attrs & 0x10000000, 0);
        assert_eq!(users.size, 12345);
    }
}
