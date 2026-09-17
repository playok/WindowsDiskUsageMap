//! NTFS 3.x decoder. Raw bytes never become Rust structs; every offset is checked.
//! Layout references: Microsoft Win32 DevNotes ATTRIBUTE_RECORD_HEADER,
//! FILE_RECORD_SEGMENT_HEADER, FILE_NAME, STANDARD_INFORMATION.
#![cfg(any(windows, test))]

use crate::scan::{Node, Progress, Report, aggregate};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

#[cfg(windows)]
pub mod windows;

pub type Result<T> = std::result::Result<T, String>;
const ID_MASK: u64 = (1 << 48) - 1;

pub fn volume_root(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    let b = text.as_bytes();
    if b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/') {
        Some(PathBuf::from(format!(
            "{}:\\",
            (b[0] as char).to_ascii_uppercase()
        )))
    } else {
        None
    }
}

pub fn slice(data: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    data.get(offset..offset.checked_add(len).ok_or("NTFS offset overflow")?)
        .ok_or_else(|| "NTFS record is truncated".into())
}
pub fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        slice(data, offset, 2)?.try_into().unwrap(),
    ))
}
pub fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        slice(data, offset, 4)?.try_into().unwrap(),
    ))
}
pub fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        slice(data, offset, 8)?.try_into().unwrap(),
    ))
}

pub fn fixup(data: &mut [u8], allow_restored: bool) -> Result<()> {
    if slice(data, 0, 4)? != b"FILE" {
        return Err("Invalid FILE signature".into());
    }
    let offset = u16_at(data, 4)? as usize;
    let count = u16_at(data, 6)? as usize;
    if count < 2 || !data.len().is_multiple_of(count - 1) {
        return Err("Invalid update sequence array".into());
    }
    let stride = data.len() / (count - 1);
    if stride < 512 || !stride.is_power_of_two() || offset < 8 || offset + count * 2 > stride - 2 {
        return Err("Invalid sector fixup geometry".into());
    }
    let values = slice(data, offset, count * 2)?.to_vec();
    let raw = (1..count).all(|i| data[i * stride - 2..i * stride] == values[..2]);
    let restored = (1..count).all(|i| data[i * stride - 2..i * stride] == values[i * 2..i * 2 + 2]);
    if !raw {
        return if allow_restored && restored {
            Ok(())
        } else {
            Err("NTFS sector fixup mismatch (record changed or damaged)".into())
        };
    }
    for i in 1..count {
        data[i * stride - 2..i * stride].copy_from_slice(&values[i * 2..i * 2 + 2]);
    }
    Ok(())
}

pub fn attributes(record: &[u8]) -> Result<Vec<&[u8]>> {
    let used = u32_at(record, 24)? as usize;
    if used > record.len() {
        return Err("Invalid record length".into());
    }
    let mut offset = u16_at(record, 20)? as usize;
    if offset < 42 || !offset.is_multiple_of(8) {
        return Err("Invalid attribute offset".into());
    }
    let data = slice(record, 0, used)?;
    let mut attrs = vec![];
    loop {
        let kind = u32_at(data, offset)?;
        if kind == u32::MAX {
            break;
        }
        let len = u32_at(data, offset + 4)? as usize;
        if len < 24 || !len.is_multiple_of(8) {
            return Err("Invalid attribute length".into());
        }
        let attr = slice(data, offset, len)?;
        match attr[8] {
            0 => {
                resident(attr)?;
            }
            1 => {
                slice(attr, 0, 64)?;
            }
            _ => return Err("Invalid attribute form".into()),
        }
        if attr[9] != 0 {
            slice(attr, u16_at(attr, 10)? as usize, attr[9] as usize * 2)?;
        }
        attrs.push(attr);
        offset += len;
    }
    Ok(attrs)
}

pub fn resident(attr: &[u8]) -> Result<&[u8]> {
    if attr[8] != 0 {
        return Err("Expected resident attribute".into());
    }
    let offset = u16_at(attr, 20)? as usize;
    if offset < 24 {
        return Err("Invalid resident value offset".into());
    }
    slice(attr, offset, u32_at(attr, 16)? as usize)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub vcn: u64,
    pub lcn: Option<u64>,
    pub clusters: u64,
}

pub fn runs(attr: &[u8]) -> Result<Vec<Run>> {
    if attr[8] != 1 {
        return Err("Expected nonresident attribute".into());
    }
    let mut vcn = u64_at(attr, 16)?;
    let end = u64_at(attr, 24)?.checked_add(1).ok_or("VCN overflow")?;
    let offset = u16_at(attr, 32)? as usize;
    if offset < 64 {
        return Err("Invalid runlist offset".into());
    }
    let mut pairs = slice(
        attr,
        offset,
        attr.len().checked_sub(offset).ok_or("Invalid runlist")?,
    )?;
    let mut lcn: i128 = 0;
    let mut result = vec![];
    loop {
        let head = *pairs.first().ok_or("Unterminated runlist")?;
        if head == 0 {
            break;
        }
        let n = (head & 15) as usize;
        let d = (head >> 4) as usize;
        if n == 0 || n > 8 || d > 8 {
            return Err("Invalid mapping pair width".into());
        }
        let bytes = slice(pairs, 1, n + d)?;
        let mut count = [0; 8];
        count[..n].copy_from_slice(&bytes[..n]);
        let clusters = u64::from_le_bytes(count);
        if clusters == 0 {
            return Err("Zero length mapping pair".into());
        }
        let physical = if d == 0 {
            None
        } else {
            let mut delta = [if bytes[n + d - 1] & 0x80 != 0 { 255 } else { 0 }; 8];
            delta[..d].copy_from_slice(&bytes[n..]);
            lcn += i64::from_le_bytes(delta) as i128;
            Some(u64::try_from(lcn).map_err(|_| "Invalid LCN")?)
        };
        result.push(Run {
            vcn,
            lcn: physical,
            clusters,
        });
        vcn = vcn.checked_add(clusters).ok_or("VCN overflow")?;
        if vcn > end {
            return Err("Mapping pairs exceed attribute range".into());
        }
        pairs = &pairs[1 + n + d..];
    }
    if vcn != end {
        return Err("Incomplete mapping pairs".into());
    }
    Ok(result)
}

pub fn list_references(data: &[u8]) -> Result<Vec<u64>> {
    let mut offset = 0;
    let mut refs = vec![];
    while offset < data.len() {
        if data[offset..].iter().all(|&b| b == 0) {
            break;
        }
        let len = u16_at(data, offset + 4)? as usize;
        if len < 26 {
            return Err("Invalid attribute list entry".into());
        }
        let entry = slice(data, offset, len)?;
        refs.push(u64_at(entry, 16)?);
        offset += len;
    }
    refs.sort_unstable();
    refs.dedup();
    Ok(refs)
}

fn file_time(ticks: u64) -> Option<SystemTime> {
    if ticks == 0 {
        return None;
    }
    const EPOCH: u64 = 116_444_736_000_000_000;
    let delta = ticks.abs_diff(EPOCH);
    let duration = Duration::new(delta / 10_000_000, ((delta % 10_000_000) * 100) as u32);
    if ticks >= EPOCH {
        SystemTime::UNIX_EPOCH.checked_add(duration)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(duration)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub parent: u64,
    pub name: OsString,
}

#[derive(Clone, Debug, Default)]
pub struct Record {
    pub reference: u64,
    pub base: u64,
    pub base_seen: bool,
    pub directory: bool,
    pub reparse: bool,
    pub links: Vec<Link>,
    pub bytes: Option<u64>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub expected: Vec<u64>,
    pub incomplete: bool,
}

pub fn parse_record(data: &[u8], number: u64) -> Result<Option<Record>> {
    if slice(data, 0, 4)? != b"FILE" {
        return if data.iter().all(|&b| b == 0) {
            Ok(None)
        } else {
            Err("Invalid MFT record signature".into())
        };
    }
    let flags = u16_at(data, 22)?;
    if flags & 1 == 0 {
        return Ok(None);
    }
    let base = u64_at(data, 32)?;
    let mut rec = Record {
        reference: number | ((u16_at(data, 16)? as u64) << 48),
        base,
        base_seen: base == 0,
        directory: flags & 2 != 0,
        ..Default::default()
    };
    for attr in attributes(data)? {
        match u32_at(attr, 0)? {
            0x10 => {
                let value = resident(attr)?;
                rec.modified = file_time(u64_at(value, 8)?);
                rec.accessed = file_time(u64_at(value, 24)?);
                rec.reparse |= u32_at(value, 32)? & 0x400 != 0;
            }
            0x20 => {
                if attr[8] == 0 {
                    rec.expected = list_references(resident(attr)?)?;
                }
                // Rare nonresident lists cannot be fully validated without more I/O.
                // Merge observed extension records, but mark the result partial.
                else {
                    rec.incomplete = true;
                }
            }
            0x30 => {
                let value = resident(attr)?;
                let header = slice(value, 0, 66)?;
                if header[65] == 2 {
                    continue;
                } // DOS 8.3 alias, not a second hard link
                if header[65] > 3 {
                    return Err("Unsupported filename namespace".into());
                }
                let raw = slice(value, 66, header[64] as usize * 2)?;
                let name: Vec<u16> = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|v| u16::from_le_bytes([v[0], v[1]]))
                    .collect();
                if name.is_empty() || name.iter().any(|&c| matches!(c, 0 | 47 | 92 | 58)) {
                    return Err("Invalid filename component".into());
                }
                #[cfg(windows)]
                let name = {
                    use std::os::windows::ffi::OsStringExt;
                    OsString::from_wide(&name)
                };
                #[cfg(not(windows))]
                let name = OsString::from(String::from_utf16_lossy(&name));
                if name == "." || name == ".." {
                    if number != 5 {
                        return Err("Invalid directory name".into());
                    }
                } else {
                    let link = Link {
                        parent: u64_at(value, 0)?,
                        name,
                    };
                    if !rec.links.contains(&link) {
                        rec.links.push(link);
                    }
                }
            }
            0x80 if attr[9] == 0 => {
                if attr[8] == 0 {
                    rec.bytes = Some(resident(attr)?.len() as u64);
                } else if u64_at(attr, 16)? == 0 {
                    rec.bytes = Some(u64_at(attr, 48)?);
                }
            }
            0xc0 => rec.reparse = true,
            _ => {}
        }
    }
    Ok(Some(rec))
}

#[derive(Default)]
pub struct Index {
    records: HashMap<u64, Record>,
    extensions: HashMap<u64, u64>,
}

impl Index {
    pub fn insert(&mut self, mut rec: Record) -> Result<()> {
        let key = if rec.base == 0 {
            rec.reference
        } else {
            self.extensions.insert(rec.reference, rec.base);
            rec.base
        };
        let Some(base) = self.records.get_mut(&key) else {
            self.records.insert(key, rec);
            return Ok(());
        };
        if base.bytes.is_some() && rec.bytes.is_some() && base.bytes != rec.bytes {
            return Err("Conflicting DATA lengths (MFT changed during scan)".into());
        }
        base.bytes = base.bytes.or(rec.bytes);
        base.modified = base.modified.max(rec.modified);
        base.accessed = base.accessed.max(rec.accessed);
        base.reparse |= rec.reparse;
        base.incomplete |= rec.incomplete;
        base.expected.append(&mut rec.expected);
        for link in rec.links {
            if !base.links.contains(&link) {
                base.links.push(link);
            }
        }
        if rec.base_seen {
            base.reference = rec.reference;
            base.base = 0;
            base.base_seen = true;
            base.directory = rec.directory;
        }
        Ok(())
    }

    pub fn report(
        &self,
        root: &Path,
        mut p: Progress,
        scanning: bool,
        cancelled: bool,
        elapsed: Duration,
    ) -> Report {
        let partial = scanning || cancelled;
        let mut nodes = vec![Node::new(root.to_path_buf(), None)];
        let mut by_parent: HashMap<u64, Vec<u64>> = HashMap::new();
        let root_ref = self
            .records
            .iter()
            .find(|(key, r)| **key & ID_MASK == 5 && r.base_seen)
            .map(|(&key, _)| key);
        for (&key, r) in &self.records {
            if r.base_seen
                && r.directory
                && key & ID_MASK >= 16
                && !r.reparse
                && let Some(link) = r.links.first()
            {
                by_parent.entry(link.parent).or_default().push(key);
            }
        }
        let mut mapping = HashMap::new();
        let mut queue = VecDeque::new();
        if let Some(root_ref) = root_ref {
            mapping.insert(root_ref, 0);
            queue.push_back(root_ref);
        }
        while let Some(key) = queue.pop_front() {
            let id = mapping[&key];
            let rec = &self.records[&key];
            nodes[id].modified = rec.modified;
            nodes[id].missing_modified = rec.modified.is_none();
            nodes[id].incomplete = self.is_incomplete(rec);
            if let Some(children) = by_parent.get_mut(&key) {
                children.sort_unstable();
                for &child in children.iter() {
                    if mapping.contains_key(&child) {
                        nodes[id].incomplete = true;
                        continue;
                    }
                    let child_id = nodes.len();
                    let path = nodes[id].path.join(&self.records[&child].links[0].name);
                    nodes.push(Node::new(path, Some(id)));
                    nodes[id].children.push(child_id);
                    mapping.insert(child, child_id);
                    queue.push_back(child);
                }
            }
        }
        p.bytes = 0;
        p.files = 0;
        p.skipped = 0;
        p.errors = 0;
        // Validate unreachable directories as well. Cycles and stale sequence
        // references must not silently turn an incomplete tree into a complete one.
        for (&key, rec) in &self.records {
            if key & ID_MASK < 16 || mapping.contains_key(&key) {
                continue;
            }
            if !rec.base_seen || rec.links.is_empty() {
                nodes[0].incomplete = true;
                p.errors += 1;
                continue;
            }
            if !rec.directory {
                continue;
            }
            let mut seen = HashSet::new();
            let mut ancestor = key;
            loop {
                if (ancestor & ID_MASK < 16 && ancestor & ID_MASK != 5)
                    || mapping.contains_key(&ancestor)
                {
                    break;
                }
                let Some(r) = self.records.get(&ancestor) else {
                    nodes[0].incomplete = true;
                    p.errors += 1;
                    break;
                };
                if r.reparse {
                    break;
                }
                if !seen.insert(ancestor) || r.links.is_empty() {
                    nodes[0].incomplete = true;
                    p.errors += 1;
                    break;
                }
                ancestor = r.links[0].parent;
            }
        }
        for (&key, rec) in &self.records {
            if !rec.base_seen || key & ID_MASK < 16 {
                continue;
            }
            for link in &rec.links {
                let Some(&parent) = mapping.get(&link.parent) else {
                    // Existing excluded parents (system/reparse trees) are intentionally skipped.
                    if !self.records.contains_key(&link.parent) {
                        p.errors += 1;
                        nodes[0].incomplete = true;
                    }
                    continue;
                };
                if rec.reparse {
                    p.skipped += 1;
                    nodes[parent].incomplete = true;
                    continue;
                }
                if rec.directory {
                    continue;
                }
                let n = &mut nodes[parent];
                let size = rec.bytes.unwrap_or_default();
                n.own_bytes = n.own_bytes.saturating_add(size);
                n.files += 1;
                n.modified = n.modified.max(rec.modified);
                n.accessed = n.accessed.max(rec.accessed);
                n.missing_modified |= rec.modified.is_none();
                n.missing_accessed |= rec.accessed.is_none();
                n.incomplete |= self.is_incomplete(rec) || rec.bytes.is_none();
                p.files += 1;
                p.bytes = p.bytes.saturating_add(size);
            }
        }
        if root_ref.is_none() {
            nodes[0].incomplete = true;
            p.errors += 1;
        }
        p.folders = nodes.len();
        let mut issues = vec![];
        if p.errors > 0 {
            issues.push(format!("MFT: 부모 참조를 확인하지 못한 항목 {}개 (스캔 중 변경 또는 아직 읽지 않은 레코드)", p.errors));
        }
        let unverified = self
            .records
            .values()
            .filter(|r| r.base_seen && self.is_incomplete(r))
            .count();
        if unverified > 0 {
            issues.push(format!("MFT: 확장 속성 목록을 완전히 검증하지 못한 레코드 {unverified}개. 해당 폴더는 부분 집계입니다."));
        }
        aggregate(&mut nodes, partial);
        Report {
            nodes,
            progress: p,
            issues,
            scanning,
            cancelled,
            elapsed,
        }
    }

    fn is_incomplete(&self, r: &Record) -> bool {
        r.incomplete
            || r.expected
                .iter()
                .any(|key| *key != r.reference && self.extensions.get(key) != Some(&r.reference))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn put16(b: &mut [u8], o: usize, x: u16) {
        b[o..o + 2].copy_from_slice(&x.to_le_bytes());
    }
    fn put32(b: &mut [u8], o: usize, x: u32) {
        b[o..o + 4].copy_from_slice(&x.to_le_bytes());
    }
    fn put64(b: &mut [u8], o: usize, x: u64) {
        b[o..o + 8].copy_from_slice(&x.to_le_bytes());
    }
    fn resident_attr(kind: u32, value: &[u8]) -> Vec<u8> {
        let len = (24 + value.len()).next_multiple_of(8);
        let mut a = vec![0; len];
        put32(&mut a, 0, kind);
        put32(&mut a, 4, len as u32);
        put32(&mut a, 16, value.len() as u32);
        put16(&mut a, 20, 24);
        a[24..24 + value.len()].copy_from_slice(value);
        a
    }
    fn nonresident_attr(vcn: u64, high: u64, length: u64, pairs: &[u8]) -> Vec<u8> {
        let len = (64 + pairs.len()).next_multiple_of(8);
        let mut a = vec![0; len];
        put32(&mut a, 0, 0x80);
        put32(&mut a, 4, len as u32);
        a[8] = 1;
        put64(&mut a, 16, vcn);
        put64(&mut a, 24, high);
        put16(&mut a, 32, 64);
        put64(&mut a, 48, length);
        a[64..64 + pairs.len()].copy_from_slice(pairs);
        a
    }
    fn name_attr(parent: u64, name: &str, namespace: u8) -> Vec<u8> {
        let name: Vec<u16> = name.encode_utf16().collect();
        let mut value = vec![0; 66 + name.len() * 2];
        put64(&mut value, 0, parent);
        value[64] = name.len() as u8;
        value[65] = namespace;
        for (i, c) in name.iter().enumerate() {
            put16(&mut value, 66 + i * 2, *c);
        }
        resident_attr(0x30, &value)
    }
    fn record(attrs: Vec<Vec<u8>>, base: u64) -> Vec<u8> {
        let mut b = vec![0; 1024];
        b[..4].copy_from_slice(b"FILE");
        put16(&mut b, 4, 48);
        put16(&mut b, 6, 3);
        put16(&mut b, 16, 7);
        put16(&mut b, 20, 56);
        put16(&mut b, 22, 1);
        put64(&mut b, 32, base);
        let mut offset = 56;
        for attr in attrs {
            b[offset..offset + attr.len()].copy_from_slice(&attr);
            offset += attr.len();
        }
        put32(&mut b, offset, u32::MAX);
        put32(&mut b, 24, offset as u32 + 8);
        put16(&mut b, 48, 0xaaaa);
        put16(&mut b, 50, 0x1234);
        put16(&mut b, 52, 0x5678);
        put16(&mut b, 510, 0xaaaa);
        put16(&mut b, 1022, 0xaaaa);
        b
    }
    fn frn(number: u64) -> u64 {
        number | (7 << 48)
    }
    fn dir(number: u64, parent: u64, name: &str) -> Record {
        Record {
            reference: frn(number),
            base_seen: true,
            directory: true,
            links: vec![Link {
                parent: frn(parent),
                name: name.into(),
            }],
            modified: Some(SystemTime::UNIX_EPOCH),
            ..Default::default()
        }
    }
    fn report(index: &Index) -> Report {
        index.report(
            Path::new("C:\\"),
            Progress::default(),
            false,
            false,
            Duration::from_secs(1),
        )
    }

    #[test]
    fn roots_are_strict_to_avoid_scanning_wrong_volume() {
        assert_eq!(volume_root(Path::new("c:/")), Some(PathBuf::from("C:\\")));
        for path in [
            "C:",
            "C:\\Users",
            "\\\\server\\share",
            "C:\\..\\",
            "\\\\?\\C:\\",
        ] {
            assert!(volume_root(Path::new(path)).is_none());
        }
    }
    #[test]
    fn raw_fixups_are_validated_and_restored() {
        let mut b = record(vec![], 0);
        fixup(&mut b, false).unwrap();
        assert_eq!(u16_at(&b, 510).unwrap(), 0x1234);
        assert_eq!(u16_at(&b, 1022).unwrap(), 0x5678);
        fixup(&mut b, true).unwrap();
        assert!(fixup(&mut b, false).is_err());
        let mut b = record(vec![], 0);
        b[1023] ^= 1;
        assert!(fixup(&mut b, false).is_err());
    }
    #[test]
    fn fragmented_signed_and_sparse_runs_decode() {
        let a = nonresident_attr(0, 5, 24000, &[0x11, 2, 10, 0x11, 3, 0xfe, 0x01, 1, 0]);
        assert_eq!(
            runs(&a).unwrap(),
            vec![
                Run {
                    vcn: 0,
                    lcn: Some(10),
                    clusters: 2
                },
                Run {
                    vcn: 2,
                    lcn: Some(8),
                    clusters: 3
                },
                Run {
                    vcn: 5,
                    lcn: None,
                    clusters: 1
                }
            ]
        );
        let a = nonresident_attr(0, 0, 1, &[0x11, 1, 0xff, 0]);
        assert!(runs(&a).is_err());
        let a = nonresident_attr(0, 0, 1, &[0x99, 1, 0]);
        assert!(runs(&a).is_err());
    }
    #[test]
    fn sizes_dates_and_dos_aliases_are_decoded() {
        let mut si = vec![0; 48];
        put64(&mut si, 8, 116_444_736_000_000_000 + 10_000_000);
        put64(&mut si, 24, 116_444_736_000_000_000 + 20_000_000);
        let mut b = record(
            vec![
                resident_attr(0x10, &si),
                name_attr(frn(5), "한글.txt", 1),
                name_attr(frn(5), "ALIAS~1.TXT", 2),
                resident_attr(0x80, &[1; 17]),
            ],
            0,
        );
        fixup(&mut b, false).unwrap();
        let r = parse_record(&b, 42).unwrap().unwrap();
        assert_eq!(r.bytes, Some(17));
        assert_eq!(r.links.len(), 1);
        assert_eq!(r.links[0].name, "한글.txt");
        assert_eq!(
            r.modified,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        );
        assert_eq!(
            r.accessed,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(2))
        );
        let mut b = record(vec![nonresident_attr(0, 2, 9876, &[0x11, 3, 20, 0])], 0);
        fixup(&mut b, false).unwrap();
        assert_eq!(parse_record(&b, 42).unwrap().unwrap().bytes, Some(9876));
    }
    #[test]
    fn extension_before_base_and_hard_links_are_merged_once() {
        let mut index = Index::default();
        index.insert(dir(5, 5, "root")).unwrap();
        index.insert(dir(20, 5, "child")).unwrap();
        let mut b = record(
            vec![
                resident_attr(0x80, &[0; 19]),
                name_attr(frn(20), "link.txt", 1),
            ],
            frn(40),
        );
        fixup(&mut b, false).unwrap();
        index
            .insert(parse_record(&b, 41).unwrap().unwrap())
            .unwrap();
        let mut b = record(vec![name_attr(frn(5), "file.txt", 1)], 0);
        fixup(&mut b, false).unwrap();
        let mut base = parse_record(&b, 40).unwrap().unwrap();
        base.expected = vec![frn(40), frn(41)];
        index.insert(base).unwrap();
        let r = report(&index);
        assert_eq!(r.nodes[0].bytes, 38);
        assert_eq!(r.nodes[0].files, 2);
        assert!(!r.nodes[0].incomplete);
        let r2 = report(&index);
        assert_eq!(r2.nodes[0].bytes, 38);
    }
    #[test]
    fn missing_extension_and_stale_parent_are_partial() {
        let mut index = Index::default();
        index.insert(dir(5, 5, "root")).unwrap();
        let mut child = dir(20, 5, "child");
        child.expected = vec![frn(99)];
        index.insert(child).unwrap();
        let mut stale = dir(21, 20, "stale");
        stale.links[0].parent = 20 | (8 << 48);
        index.insert(stale).unwrap();
        let r = report(&index);
        assert!(r.nodes[0].incomplete);
        assert!(r.progress.errors > 0);
        assert_eq!(r.nodes.len(), 2);
    }
    #[test]
    fn reparse_system_and_deleted_records_are_not_counted() {
        let mut index = Index::default();
        index.insert(dir(5, 5, "root")).unwrap();
        let mut link = dir(20, 5, "junction");
        link.reparse = true;
        index.insert(link).unwrap();
        index.insert(dir(21, 20, "not-followed")).unwrap();
        index.insert(dir(11, 5, "$Extend")).unwrap();
        let r = report(&index);
        assert_eq!(r.nodes.len(), 1);
        assert_eq!(r.progress.skipped, 1);
        let mut b = record(vec![], 0);
        put16(&mut b, 22, 0);
        assert!(parse_record(&b, 30).unwrap().is_none());
    }
    #[test]
    fn malformed_records_never_panic() {
        let seed = record(
            vec![name_attr(frn(5), "a", 1), resident_attr(0x80, &[0; 5])],
            0,
        );
        for len in 0..seed.len() {
            let _ = parse_record(&seed[..len], 40);
        }
        for i in 0..seed.len() {
            let mut b = seed.clone();
            b[i] = 0xff;
            let _ = fixup(&mut b, false);
            let _ = parse_record(&b, 40);
        }
    }
    #[test]
    fn attribute_list_references_are_bounds_checked() {
        let mut list = vec![0; 32];
        put32(&mut list, 0, 0x80);
        put16(&mut list, 4, 32);
        put64(&mut list, 16, frn(41));
        assert_eq!(list_references(&list).unwrap(), vec![frn(41)]);
        put16(&mut list, 4, 0);
        assert!(list_references(&list).is_err());
    }
}
