//! Fast indexer: reads the NTFS master file table straight from the volume, the same technique
//! Everything uses. Opening `\\.\C:` for reading requires administrator rights.

use crate::index::{ATTR_DIRECTORY, NONE, Source, VolumeBuilder, VolumeIndex, valid_name};
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
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;
const NAMESPACE_DOS: u8 = 2;
const RECORD_IN_USE: u16 = 0x1;
const RECORD_IS_DIRECTORY: u16 = 0x2;
const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// A further hard link of a file record: another non-DOS `$FILE_NAME` with its own parent and
/// name. The folder scan lists every link, so the NTFS index does too (MIN-04).
#[derive(Clone, Copy)]
struct Link {
    frn: usize,
    parent: u64,
    name_off: u32,
    name_len: u16,
}

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
    let mut read_raw = |offset: u64, buf: &mut [u8]| {
        offset.checked_add(buf.len() as u64).is_some_and(|end| end <= volume_bytes) && read_exact_at(&volume, buf, offset).is_ok()
    };
    let runs = mft_data_runs(&rec0, total_clusters, cluster, record_size, &mut read_raw)
        .map_err(|e| anyhow::anyhow!("cannot locate valid $MFT data runs: {e}"))?;

    let record_count = mft_bytes / record_size as u64;
    if record_count > MAX_MFT_RECORDS {
        anyhow::bail!("$MFT has more than {MAX_MFT_RECORDS} records");
    }
    // Fail closed when the runs cannot hold the whole table, so the folder scan runs instead of
    // an index that silently misses part of the volume (FC-14).
    if !runs_cover_records(&runs, cluster, record_size as u64, record_count) {
        anyhow::bail!("$MFT data runs do not cover the whole table");
    }
    let record_count = record_count as usize;
    total.store(record_count, Ordering::Relaxed);
    let mut recs = vec![Rec::default(); record_count];
    let mut names = String::with_capacity(record_count * 16);
    let mut links = Vec::new();
    let mut read = |offset: u64, buf: &mut [u8]| read_exact_at(&volume, buf, offset);
    read_records(&runs, cluster, record_size, &mut recs, &mut names, &mut links, progress, cancel, &mut read)?;

    build(letter, &recs, &names, &links, start)
}

#[cfg(not(windows))]
pub fn read_volume(_: char, _: &AtomicUsize, _: &AtomicUsize, _: &AtomicBool) -> anyhow::Result<VolumeIndex> {
    anyhow::bail!("MFT indexing is only available on Windows")
}

/// Parses records `0..recs.len()` of $MFT, stored in `runs` and read through `read` (volume
/// byte offset, buffer). Records are numbered by their byte position in $MFT: when clusters are
/// smaller than records, a record can straddle two runs, and is put together from both.
#[allow(clippy::too_many_arguments)]
fn read_records(
    runs: &[(Option<u64>, u64)],
    cluster: u64,
    record_size: usize,
    recs: &mut [Rec],
    names: &mut String,
    links: &mut Vec<Link>,
    progress: &AtomicUsize,
    cancel: &AtomicBool,
    read: &mut dyn FnMut(u64, &mut [u8]) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    let record_count = recs.len();
    // Bytes of $MFT that hold the records; `runs_cover_records` checked that they fit.
    let end = record_count as u64 * record_size as u64;
    let mut buf = vec![0u8; READ_CHUNK];
    // `buf[..filled]` holds $MFT from byte `frn * record_size`: at most a partial record
    // between reads.
    let (mut frn, mut filled) = (0usize, 0usize);
    // `mft_runs` keeps every run inside the volume, so these byte offsets cannot overflow.
    'runs: for &(lcn, clusters) in runs {
        let run_bytes = clusters * cluster;
        let mut done = 0u64;
        while done < run_bytes {
            let left = end - frn as u64 * record_size as u64 - filled as u64;
            if left == 0 {
                break 'runs;
            }
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("cancelled");
            }
            let want = (run_bytes - done).min((READ_CHUNK - filled) as u64).min(left) as usize;
            let chunk = &mut buf[filled..filled + want];
            match lcn {
                Some(lcn) => read(lcn * cluster + done, chunk)?,
                // Sparse run: no records stored.
                None => chunk.fill(0),
            }
            filled += want;
            done += want as u64;
            let whole = filled - filled % record_size;
            for record in buf[..whole].chunks_exact_mut(record_size) {
                parse_record(record, frn, recs, names, links);
                frn += 1;
            }
            buf.copy_within(whole..filled, 0);
            filled -= whole;
            progress.store(frn, Ordering::Relaxed);
        }
    }
    if frn < record_count {
        anyhow::bail!("$MFT data runs covered {frn} of {record_count} records");
    }
    Ok(())
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

fn parse_record(record: &mut [u8], frn: usize, recs: &mut [Rec], names: &mut String, links: &mut Vec<Link>) {
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
                // A second long name is another hard link; DOS names are only short aliases.
                let another_link = !better && namespace != NAMESPACE_DOS;
                if !better && !another_link {
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
                let name_off = names.len() as u32;
                let name_len = name.len().min(u16::MAX as usize) as u16;
                names.push_str(&name);
                let parent = u64_at(v, 0) & REF_MASK;
                if another_link {
                    links.push(Link { frn: target, parent, name_off, name_len });
                    continue;
                }
                rec.name_off = name_off;
                rec.name_len = name_len;
                rec.name_ns = namespace;
                rec.has_name = true;
                rec.parent = parent;
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

/// `(starting LCN or None when sparse, length in clusters)`, in VCN order.
type Runs = Vec<(Option<u64>, u64)>;

/// Decodes the data runs of the unnamed $DATA attribute in $MFT's own record that starts at
/// VCN 0. When $MFT is fragmented, that is only the first piece of its run list; see
/// [`mft_data_runs`] for the rest.
fn mft_runs(rec0: &[u8], total_clusters: u64) -> Option<Runs> {
    data_piece(rec0, 0).and_then(|attr| decode_runs(attr, total_clusters))
}

/// The unnamed, non-resident $DATA attribute in `record` whose run list starts at `vcn`.
fn data_piece(record: &[u8], vcn: u64) -> Option<&[u8]> {
    attributes(record)
        .map(|(_, a)| a)
        .find(|a| u32_at(a, 0) == ATTR_DATA && a[8] != 0 && a[9] == 0 && a.len() >= 0x22 && u64_at(a, 0x10) == vcn)
}

/// The whole run list of $MFT (FC-14). Without an `$ATTRIBUTE_LIST` in record 0 this is the
/// piece record 0 holds. With one, the list names every piece of the unnamed $DATA attribute,
/// by starting VCN and the record holding it; those records are read through `read` (raw volume
/// bytes at an offset), located with the runs decoded so far. The pieces must follow each other
/// without gaps or overlaps; anything else fails, and the caller falls back to the folder scan.
fn mft_data_runs(
    rec0: &[u8],
    total_clusters: u64,
    cluster: u64,
    record_size: usize,
    read: &mut dyn FnMut(u64, &mut [u8]) -> bool,
) -> Result<Runs, String> {
    let Some(pieces) = attribute_list_data_pieces(rec0, cluster, total_clusters, read)? else {
        return mft_runs(rec0, total_clusters).ok_or_else(|| "invalid $MFT data runs".to_string());
    };
    let mut runs: Runs = Vec::new();
    let mut next_vcn = 0u64;
    for (vcn, frn) in pieces {
        if vcn != next_vcn {
            return Err(format!("$MFT run list has a gap or overlap at VCN {vcn}"));
        }
        let piece_runs = if frn == 0 {
            data_piece(rec0, vcn).and_then(|a| decode_runs(a, total_clusters))
        } else {
            let mut record = vec![0u8; record_size];
            let offset = frn
                .checked_mul(record_size as u64)
                .and_then(|byte| volume_offset(&runs, cluster, byte))
                .ok_or_else(|| format!("$MFT extension record {frn} lies outside the runs read so far"))?;
            if !read(offset, &mut record) || !apply_fixup(&mut record) {
                return Err(format!("cannot read $MFT extension record {frn}"));
            }
            data_piece(&record, vcn).and_then(|a| decode_runs(a, total_clusters))
        };
        let piece_runs = piece_runs.ok_or_else(|| format!("invalid $MFT data runs at VCN {vcn}"))?;
        for &(_, clusters) in &piece_runs {
            next_vcn = next_vcn.checked_add(clusters).ok_or("$MFT run list overflows")?;
        }
        runs.extend(piece_runs);
    }
    if runs.is_empty() {
        return Err("$MFT has no data runs".into());
    }
    Ok(runs)
}

/// `(starting VCN, record number)` of every piece of $MFT's unnamed $DATA attribute, sorted by
/// VCN, when record 0 has an `$ATTRIBUTE_LIST`; `None` when it has none. A non-resident list is
/// read from the volume through `read`.
fn attribute_list_data_pieces(
    rec0: &[u8],
    cluster: u64,
    total_clusters: u64,
    read: &mut dyn FnMut(u64, &mut [u8]) -> bool,
) -> Result<Option<Vec<(u64, u64)>>, String> {
    let Some((_, list_attr)) = attributes(rec0).find(|(ty, _)| *ty == ATTR_ATTRIBUTE_LIST) else { return Ok(None) };
    let owned;
    let list: &[u8] = if list_attr[8] == 0 {
        resident_value(list_attr).ok_or("invalid $ATTRIBUTE_LIST")?
    } else {
        if list_attr.len() < 0x38 {
            return Err("invalid $ATTRIBUTE_LIST".into());
        }
        let size = usize::try_from(u64_at(list_attr, 0x30)).ok().filter(|&s| s <= 16 << 20).ok_or("invalid $ATTRIBUTE_LIST size")?;
        let runs = decode_runs(list_attr, total_clusters).ok_or("invalid $ATTRIBUTE_LIST runs")?;
        let mut bytes = Vec::with_capacity(size);
        for (lcn, clusters) in runs {
            // Only what the list still needs, in whole clusters: a run may be far longer.
            let missing = (size - bytes.len()) as u64;
            let len = clusters.checked_mul(cluster).ok_or("invalid $ATTRIBUTE_LIST runs")?.min(missing.next_multiple_of(cluster));
            let start = bytes.len();
            bytes.resize(start + len as usize, 0);
            match lcn {
                Some(lcn) if read(lcn * cluster, &mut bytes[start..]) => {}
                Some(_) => return Err("cannot read $ATTRIBUTE_LIST".into()),
                None => {}
            }
            if bytes.len() >= size {
                break;
            }
        }
        if bytes.len() < size {
            return Err("$ATTRIBUTE_LIST runs are shorter than the list".into());
        }
        bytes.truncate(size);
        owned = bytes;
        &owned
    };

    let mut pieces = Vec::new();
    let mut pos = 0;
    while pos + 0x1A <= list.len() {
        let entry_len = u16_at(list, pos + 4) as usize;
        if entry_len < 0x1A || pos + entry_len > list.len() {
            return Err("invalid $ATTRIBUTE_LIST entry".into());
        }
        let entry = &list[pos..pos + entry_len];
        if u32_at(entry, 0) == ATTR_DATA && entry[6] == 0 {
            pieces.push((u64_at(entry, 8), u64_at(entry, 0x10) & REF_MASK));
        }
        pos += entry_len;
    }
    pieces.sort_unstable();
    if pieces.first().is_none_or(|&(vcn, _)| vcn != 0) {
        return Err("$ATTRIBUTE_LIST lists no $DATA piece at VCN 0".into());
    }
    Ok(Some(pieces))
}

/// The volume byte offset of byte `byte` of $MFT, if `runs` cover it (not in a sparse run).
fn volume_offset(runs: &[(Option<u64>, u64)], cluster: u64, byte: u64) -> Option<u64> {
    let mut vcn = byte / cluster;
    for &(lcn, clusters) in runs {
        if vcn < clusters {
            return lcn?.checked_add(vcn)?.checked_mul(cluster)?.checked_add(byte % cluster);
        }
        vcn -= clusters;
    }
    None
}

/// Decodes the mapping pairs of a non-resident attribute. Fails on an empty run, a negative or
/// overflowing LCN, a run that does not fit inside a volume of `total_clusters` (FC-10), or a
/// sparse run longer than the volume.
fn decode_runs(attr: &[u8], total_clusters: u64) -> Option<Runs> {
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
            // Sparse: no clusters, but no longer than the volume either (it would overflow
            // byte counts later on).
            if length > total_clusters {
                return None;
            }
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

/// Whether `runs` (in clusters of `cluster` bytes) hold at least `record_count` records. When
/// $MFT is fragmented enough, record 0 holds only the first part of its run list and the rest
/// lives in extension records, so the runs found there can cover less than the whole table.
fn runs_cover_records(runs: &[(Option<u64>, u64)], cluster: u64, record_size: u64, record_count: u64) -> bool {
    let bytes = runs
        .iter()
        .try_fold(0u64, |acc, &(_, clusters)| acc.checked_add(clusters.checked_mul(cluster)?));
    bytes.is_some_and(|b| b / record_size >= record_count)
}

/// Keeps records reachable from the root through valid parents and turns them into an index.
fn build(letter: char, recs: &[Rec], names: &str, links: &[Link], start: Instant) -> anyhow::Result<VolumeIndex> {
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

    // POSIX-namespace names may hold characters Win32 paths cannot; keep the path structure
    // intact (and the saved cache loadable) by substituting them (FC-17).
    let name_at = |off: u32, len: u16| names.get(off as usize..off as usize + len as usize).filter(|n| valid_name(n)).unwrap_or("?");
    // Bit 0x10000000 in $STANDARD_INFORMATION marks directories but is not a Win32 attribute.
    let attrs_of = |r: &Rec| (r.attrs & 0x0FFF_FFFF) | if r.is_dir { ATTR_DIRECTORY } else { 0 };

    let mut builder = VolumeBuilder::new(letter);
    builder.reserve(next as usize + links.len());
    for i in 0..n {
        if ids[i] == NONE || i == ROOT_RECORD {
            continue;
        }
        let r = &recs[i];
        let size = if r.is_dir { 0 } else { r.size };
        builder.push(name_at(r.name_off, r.name_len), ids[r.parent as usize], attrs_of(r), size, r.mtime);
    }
    // Further hard links of indexed files, under folders that are indexed too. Pushed after
    // every record, so the ids above still match the builder's.
    for l in links {
        let parent = usize::try_from(l.parent).unwrap_or(usize::MAX);
        let Some(r) = recs.get(l.frn).filter(|r| !r.is_dir && ids[l.frn] != NONE) else { continue };
        let Some(&parent_id) = ids.get(parent).filter(|&&id| id != NONE) else { continue };
        builder.push(name_at(l.name_off, l.name_len), parent_id, attrs_of(r), r.size, r.mtime);
    }
    builder.check_capacity()?;
    Ok(builder.finish(Source::Mft, start.elapsed()))
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
        // Sparse runs: at most the volume's length (u64::MAX used to wrap byte counts).
        assert_eq!(runs(&[0x01, 0x20], 0x1F), None);
        assert_eq!(runs(&[0x01, 0x20], 0x20), Some(vec![(None, 0x20)]));
        assert_eq!(runs(&[0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], u64::MAX - 1), None);
    }

    /// FC-14: when $MFT's run list continues in an extension record ($ATTRIBUTE_LIST), record 0
    /// holds only part of it. Reading must fail (and fall back to the folder scan) instead of
    /// silently indexing the records the first runs happen to cover.
    #[test]
    fn partial_mft_run_lists_are_rejected() {
        // 0x20 clusters of 4 KiB hold 128 records of 1 KiB.
        let runs = [(Some(0x1000), 0x10), (None, 0x10)];
        assert!(runs_cover_records(&runs, 4096, 1024, 128));
        assert!(!runs_cover_records(&runs, 4096, 1024, 129));
        assert!(!runs_cover_records(&[(Some(1), u64::MAX)], 4096, 1024, 1));

        // An attribute that starts past VCN 0 is an extension piece, not the start of the list.
        let mut attr = non_resident_data(0, &[0x21, 0x20, 0x00, 0x10]);
        attr[0x10..0x18].copy_from_slice(&5u64.to_le_bytes());
        let mut rec = record(RECORD_IN_USE, 0, &[attr]);
        assert!(apply_fixup(&mut rec));
        assert_eq!(mft_runs(&rec, 0x2000), None);
    }

    /// An `$ATTRIBUTE_LIST` naming the pieces `(starting VCN, record)` of the unnamed $DATA.
    fn attribute_list(pieces: &[(u64, u64)]) -> Vec<u8> {
        let mut list = Vec::new();
        for &(vcn, frn) in pieces {
            let mut e = vec![0u8; 0x20];
            e[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
            e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
            e[7] = 0x1A;
            e[8..16].copy_from_slice(&vcn.to_le_bytes());
            e[0x10..0x18].copy_from_slice(&(frn | (1 << 48)).to_le_bytes());
            list.extend(e);
        }
        resident(ATTR_ATTRIBUTE_LIST, &list)
    }

    /// A $DATA piece whose run list starts at `vcn`.
    fn data_at(vcn: u64, runs: &[u8]) -> Vec<u8> {
        let mut a = non_resident_data(0, runs);
        a[0x10..0x18].copy_from_slice(&vcn.to_le_bytes());
        a
    }

    /// A non-resident `$ATTRIBUTE_LIST` is read only up to its declared size. Each of its runs
    /// was read whole first: a run of 2^28 clusters (1 TiB) allocated that much and aborted.
    #[test]
    fn attribute_lists_read_only_their_declared_size() {
        const CLUSTER: u64 = 4096;
        let list = attribute_list(&[(0, 0)]);
        let value = list[0x18..0x18 + 0x20].to_vec();
        // 0x1000_0000 clusters at LCN 30, on a volume of 2^40 clusters.
        let mut attr = non_resident_data(value.len() as u64, &[0x14, 0x00, 0x00, 0x00, 0x10, 0x1E]);
        attr[0..4].copy_from_slice(&ATTR_ATTRIBUTE_LIST.to_le_bytes());
        let mut rec0 = record(RECORD_IN_USE, 0, &[attr]);
        assert!(apply_fixup(&mut rec0));
        let mut longest = 0;
        let mut read = |offset: u64, buf: &mut [u8]| {
            longest = longest.max(buf.len());
            if offset == 30 * CLUSTER {
                buf[..value.len()].copy_from_slice(&value);
            }
            true
        };
        assert_eq!(attribute_list_data_pieces(&rec0, CLUSTER, 1 << 40, &mut read), Ok(Some(vec![(0, 0)])));
        assert_eq!(longest, CLUSTER as usize);
    }

    /// FC-14: a fragmented $MFT whose run list continues through `$ATTRIBUTE_LIST` is read in
    /// full: the second piece comes from extension record 16, located through the first piece.
    #[test]
    fn mft_run_lists_continue_through_the_attribute_list() {
        const CLUSTER: u64 = 4096;
        let mut disk = vec![0u8; 64 * CLUSTER as usize];
        // Piece 1: VCN 0-7 at LCN 2 (in record 0). Piece 2: VCN 8-15 at LCN 20 (in record 16).
        // Record 16 is byte 16 KiB of $MFT: VCN 4, so LCN 6.
        let rec16 = record(RECORD_IN_USE, 0, &[data_at(8, &[0x11, 0x08, 0x14])]);
        let at = 6 * CLUSTER as usize;
        disk[at..at + RECORD].copy_from_slice(&rec16);
        // The same list stored non-resident, in one cluster at LCN 30.
        let list = attribute_list(&[(0, 0), (8, 16)]);
        let list_value = &list[0x18..0x18 + 0x40];
        disk[30 * CLUSTER as usize..][..list_value.len()].copy_from_slice(list_value);
        let mut non_resident_list = non_resident_data(list_value.len() as u64, &[0x11, 0x01, 0x1E]);
        non_resident_list[0..4].copy_from_slice(&ATTR_ATTRIBUTE_LIST.to_le_bytes());
        let mut read = |offset: u64, buf: &mut [u8]| {
            let o = offset as usize;
            disk.get(o..o + buf.len()).map(|src| buf.copy_from_slice(src)).is_some()
        };
        let rec0 = |list: &[(u64, u64)]| {
            let mut r = record(RECORD_IN_USE, 0, &[attribute_list(list), data_at(0, &[0x11, 0x08, 0x02])]);
            assert!(apply_fixup(&mut r));
            r
        };

        let runs = mft_data_runs(&rec0(&[(0, 0), (8, 16)]), 64, CLUSTER, RECORD, &mut read).unwrap();
        assert_eq!(runs, vec![(Some(2), 8), (Some(20), 8)]);
        assert!(runs_cover_records(&runs, CLUSTER, RECORD as u64, 64));

        // A gap between the pieces, and a piece in a record past the runs read so far.
        assert!(mft_data_runs(&rec0(&[(0, 0), (9, 16)]), 64, CLUSTER, RECORD, &mut read).is_err());
        assert!(mft_data_runs(&rec0(&[(0, 0), (8, 100)]), 64, CLUSTER, RECORD, &mut read).is_err());
        let mut with_stored_list = record(RECORD_IN_USE, 0, &[non_resident_list, data_at(0, &[0x11, 0x08, 0x02])]);
        assert!(apply_fixup(&mut with_stored_list));
        assert_eq!(mft_data_runs(&with_stored_list, 64, CLUSTER, RECORD, &mut read).unwrap(), runs);
        // Without a list, record 0's piece is the whole run list, as before.
        let mut plain = record(RECORD_IN_USE, 0, &[data_at(0, &[0x11, 0x08, 0x02])]);
        assert!(apply_fixup(&mut plain));
        assert_eq!(mft_data_runs(&plain, 64, CLUSTER, RECORD, &mut read).unwrap(), vec![(Some(2), 8)]);
    }

    /// With 512-byte clusters and 1 KiB records, a run of an odd number of clusters ends in the
    /// middle of a record. That record is put together from both runs and the ones after it
    /// keep their numbers. Each run used to be cut into records from its own start, so every
    /// later record was misread and the table came up short (or an index came out partial).
    #[test]
    fn records_straddling_two_runs_are_read_whole() {
        const CLUSTER: u64 = 512;
        let mut disk = vec![0u8; 64 * CLUSTER as usize];
        let mft: Vec<u8> = (0..3).flat_map(|_| record(RECORD_IN_USE, 0, &[])).collect();
        // Clusters 0-2 of $MFT at LCN 10, clusters 3-5 at LCN 40: record 1 is split.
        disk[10 * 512..13 * 512].copy_from_slice(&mft[..3 * 512]);
        disk[40 * 512..43 * 512].copy_from_slice(&mft[3 * 512..]);
        let runs = [(Some(10), 3), (Some(40), 3)];
        let mut read = |offset: u64, buf: &mut [u8]| {
            let o = offset as usize;
            buf.copy_from_slice(&disk[o..o + buf.len()]);
            Ok(())
        };
        let mut recs = vec![Rec::default(); 3];
        let progress = AtomicUsize::new(0);
        read_records(&runs, CLUSTER, RECORD, &mut recs, &mut String::new(), &mut Vec::new(), &progress, &AtomicBool::new(false), &mut read)
            .unwrap();
        assert!(recs.iter().all(|r| r.in_use), "every record is parsed");
        assert_eq!(progress.load(Ordering::Relaxed), 3);
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
        parse_record(&mut short, 1, &mut recs, &mut String::new(), &mut Vec::new());
        assert!(!recs[1].in_use);
    }

    #[test]
    fn rejects_torn_records() {
        let mut rec = record(RECORD_IN_USE, 0, &[]);
        rec[1022] ^= 0xFF;
        assert!(!apply_fixup(&mut rec));
    }

    /// MIN-04: a file with several hard links is listed under each of them, as the folder scan
    /// lists it. Only one name per record used to be kept. DOS aliases are not extra links, and
    /// links under folders that are not indexed ($Extend) are dropped.
    #[test]
    fn hard_links_are_listed_under_every_name() {
        let mut recs = vec![Rec::default(); 20];
        let mut names = String::new();
        let mut links = Vec::new();
        let dir = RECORD_IN_USE | RECORD_IS_DIRECTORY;
        let mut raw = [
            (5, record(dir, 0, &[standard_info(1, 0), file_name(5, ".", 3)])),
            (16, record(dir, 0, &[standard_info(2, 0), file_name(5, "Users", 3)])),
            (
                17,
                record(
                    RECORD_IN_USE,
                    0,
                    &[
                        standard_info(3, 0x20),
                        file_name(16, "a.txt", 3),
                        file_name(5, "B~1.TXT", 2),
                        file_name(5, "b link.txt", 1),
                        file_name(11, "hidden link", 1),
                    ],
                ),
            ),
        ];
        for (frn, rec) in raw.iter_mut() {
            parse_record(rec, *frn, &mut recs, &mut names, &mut links);
        }
        assert_eq!(links.len(), 2);

        let v = build('C', &recs, &names, &links, Instant::now()).unwrap();
        let mut paths: Vec<String> = v.by_name.iter().map(|&i| v.full_path(i)).collect();
        paths.sort();
        assert_eq!(paths, [r"C:\Users", r"C:\Users\a.txt", r"C:\b link.txt"]);
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
        let mut links = Vec::new();
        for (frn, rec) in raw.iter_mut() {
            parse_record(rec, *frn, &mut recs, &mut names, &mut links);
        }
        assert!(links.is_empty(), "a DOS alias is not another link");

        let v = build('C', &recs, &names, &links, Instant::now()).unwrap();
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
