//! Read-only volume access. No disk writes, journal creation, or volume locking.
use super::*;
use crate::scan::Event;
use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

const GET_VOLUME: u32 = 0x0009_0064;
const GET_RECORD: u32 = 0x0009_0068;
const CHUNK: usize = 4 * 1024 * 1024;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        handle: *mut c_void,
        code: u32,
        input: *const c_void,
        input_size: u32,
        output: *mut c_void,
        output_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
}

fn ioctl(file: &File, code: u32, input: &[u8], capacity: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; capacity];
    let mut returned = 0;
    // Buffers live for this synchronous call; Windows only writes within capacity.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(format!(
            "NTFS 조회 실패: {}",
            std::io::Error::last_os_error()
        ));
    }
    if returned as usize > out.len() {
        return Err("Invalid DeviceIoControl output length".into());
    }
    out.truncate(returned as usize);
    Ok(out)
}

#[derive(Debug)]
struct Geometry {
    sector: u64,
    cluster: u64,
    record: usize,
    length: u64,
    volume_bytes: u64,
}

fn geometry(file: &File) -> Result<Geometry> {
    let data = ioctl(file, GET_VOLUME, &[], 128)?;
    decode_geometry(&data)
}

fn decode_geometry(data: &[u8]) -> Result<Geometry> {
    // NTFS_VOLUME_DATA_BUFFER is 96 bytes; extended version follows it.
    if u16_at(data, 100)? != 3 || u16_at(data, 102)? > 1 {
        return Err("지원하지 않는 NTFS 버전".into());
    }
    let sector = u32_at(data, 40)? as u64;
    let cluster = u32_at(data, 44)? as u64;
    let record = u32_at(data, 48)? as usize;
    let length = u64_at(data, 56)?;
    if !(512..=65536).contains(&sector)
        || !sector.is_power_of_two()
        || cluster < sector
        || cluster > 2 * 1024 * 1024
        || !cluster.is_power_of_two()
        || !(512..=65536).contains(&record)
        || !record.is_power_of_two()
        || length == 0
        || !length.is_multiple_of(record as u64)
    {
        return Err("지원하지 않는 NTFS 볼륨 구조".into());
    }
    let volume_bytes = u64_at(data, 8)?
        .checked_mul(sector)
        .ok_or("Volume size overflow")?;
    Ok(Geometry {
        sector,
        cluster,
        record,
        length,
        volume_bytes,
    })
}

fn get_record(file: &File, reference: u64, record_size: usize) -> Result<Vec<u8>> {
    let out = ioctl(file, GET_RECORD, &reference.to_le_bytes(), record_size + 32)?;
    // FSCTL can return the nearest LOWER in-use record. Never accept that substitution.
    if u64_at(&out, 0)? & ID_MASK != reference & ID_MASK || u32_at(&out, 8)? as usize != record_size
    {
        return Err("요청한 MFT 확장 레코드가 없거나 변경되었습니다".into());
    }
    let mut data = slice(&out, 12, record_size)?.to_vec();
    fixup(&mut data, true)?;
    if reference >> 48 != 0 && u16_at(&data, 16)? as u64 != reference >> 48 {
        return Err("MFT 레코드 시퀀스 변경 감지".into());
    }
    Ok(data)
}

struct Aligned {
    ptr: NonNull<u8>,
    layout: Layout,
}
impl Aligned {
    fn new(len: usize, alignment: usize) -> Result<Self> {
        let layout = Layout::from_size_align(len, alignment).map_err(|e| e.to_string())?;
        // Raw volume reads may require sector-aligned buffers even with buffered handles.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) })
            .ok_or("MFT read buffer allocation failed")?;
        Ok(Self { ptr, layout })
    }
    fn bytes(&mut self) -> &mut [u8] {
        // ptr owns exactly layout.size() initialized bytes, uniquely borrowed here.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}
impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

fn read_stream(
    file: &mut File,
    mapping: &[Run],
    g: &Geometry,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>> {
    let mut out = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let logical = offset
            .checked_add(done as u64)
            .ok_or("Read offset overflow")?;
        let vcn = logical / g.cluster;
        let run = mapping
            .iter()
            .find(|r| vcn >= r.vcn && vcn - r.vcn < r.clusters)
            .ok_or("MFT 데이터 구간이 누락되었습니다")?;
        let lcn = run
            .lcn
            .ok_or("Sparse MFT/attribute-list streams are unsupported")?;
        let delta = logical
            .checked_sub(run.vcn.checked_mul(g.cluster).ok_or("VCN overflow")?)
            .ok_or("Invalid VCN")?;
        let available = run
            .clusters
            .checked_mul(g.cluster)
            .ok_or("Run length overflow")?
            .checked_sub(delta)
            .ok_or("Invalid run offset")?;
        let count = (len - done).min(usize::try_from(available).unwrap_or(usize::MAX));
        let physical = lcn
            .checked_mul(g.cluster)
            .and_then(|v| v.checked_add(delta))
            .ok_or("LCN overflow")?;
        let prefix = physical % g.sector;
        let aligned_offset = physical - prefix;
        let read_len = (prefix + count as u64).div_ceil(g.sector) * g.sector;
        if aligned_offset
            .checked_add(read_len)
            .is_none_or(|end| end > g.volume_bytes)
        {
            return Err("MFT run exceeds volume bounds".into());
        }
        let mut buffer = Aligned::new(read_len as usize, g.sector as usize)?;
        file.seek(SeekFrom::Start(aligned_offset))
            .map_err(|e| e.to_string())?;
        file.read_exact(buffer.bytes())
            .map_err(|e| format!("MFT 디스크 읽기 실패: {e}"))?;
        out[done..done + count]
            .copy_from_slice(&buffer.bytes()[prefix as usize..prefix as usize + count]);
        done += count;
    }
    Ok(out)
}

fn mft_mapping(file: &mut File, g: &Geometry) -> Result<Vec<Run>> {
    let base = get_record(file, 0, g.record)?;
    let base_reference = (u16_at(&base, 16)? as u64) << 48;
    let mut records = vec![base];
    let mut references = vec![];
    for attr in attributes(&records[0])? {
        if u32_at(attr, 0)? == 0x20 {
            let data = if attr[8] == 0 {
                resident(attr)?.to_vec()
            } else {
                let len = u64_at(attr, 48)?;
                if len > 16 * 1024 * 1024 || u64_at(attr, 16)? != 0 {
                    return Err("MFT 속성 목록이 지원 범위를 벗어났습니다".into());
                }
                read_stream(file, &runs(attr)?, g, 0, len as usize)?
            };
            references = list_references(&data)?;
        }
    }
    if references.len() > 4096 {
        return Err("MFT 확장 레코드가 너무 많습니다".into());
    }
    for reference in references {
        if reference & ID_MASK == 0 {
            continue;
        }
        let record = get_record(file, reference, g.record)?;
        if u64_at(&record, 32)? != base_reference {
            return Err("MFT 확장 레코드의 부모가 변경되었습니다".into());
        }
        records.push(record);
    }
    let mut mapping = vec![];
    let mut data_size = 0;
    for record in &records {
        for attr in attributes(record)? {
            if u32_at(attr, 0)? == 0x80 && attr[9] == 0 {
                if attr[8] != 1 || u16_at(attr, 12)? != 0 {
                    return Err("Unsupported MFT DATA attribute".into());
                }
                if u64_at(attr, 16)? == 0 {
                    data_size = u64_at(attr, 48)?;
                }
                mapping.extend(runs(attr)?);
            }
        }
    }
    mapping.sort_unstable_by_key(|r| r.vcn);
    let mut end: u64 = 0;
    for run in &mapping {
        if run.vcn != end || run.lcn.is_none() {
            return Err("MFT runlist contains gaps/overlap/sparse regions".into());
        }
        end = end.checked_add(run.clusters).ok_or("MFT length overflow")?;
    }
    if data_size < g.length
        || end
            .checked_mul(g.cluster)
            .is_none_or(|bytes| bytes < g.length)
    {
        return Err("MFT 크기가 스캔 준비 중 변경되었습니다".into());
    }
    Ok(mapping)
}

pub fn scan(root: &Path, cancel: &AtomicBool, publish: &mut impl FnMut(Event)) -> Result<Report> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let drive_type = unsafe { GetDriveTypeW(wide.as_ptr()) };
    if !matches!(drive_type, 2 | 3) {
        return Err("로컬 고정/이동식 드라이브만 지원합니다".into());
    }
    let device = format!("\\\\.\\{}", &root.to_string_lossy()[..2]);
    let mut volume = OpenOptions::new()
        .read(true)
        .share_mode(1 | 2 | 4)
        .open(device)
        .map_err(|e| format!("볼륨 읽기 권한이 필요합니다. 관리자 권한으로 실행하세요 ({e})"))?;
    let started = Instant::now();
    let g = geometry(&volume)?;
    let mapping = mft_mapping(&mut volume, &g)?;
    let mut index = Index::default();
    let mut p = Progress {
        engine: "MFT 고속",
        note: "NTFS MFT 직접 읽기 · 시스템 메타파일 제외 · 하드 링크는 경로별 집계".into(),
        total_records: g.length / g.record as u64,
        current: root.to_path_buf(),
        ..Default::default()
    };
    publish(Event::Progress(p.clone()));
    let mut next_preview = Instant::now() + Duration::from_millis(250);
    let mut last_progress = Instant::now();
    let mut offset = 0;
    while offset < g.length && !cancel.load(Ordering::Relaxed) {
        let count = (g.length - offset).min(CHUNK as u64) as usize;
        let mut data = read_stream(&mut volume, &mapping, &g, offset, count)?;
        for (i, record) in data.chunks_exact_mut(g.record).enumerate() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let number = offset / g.record as u64 + i as u64;
            // Deleted slots need not have valid fixups. Never interpret their attributes.
            if record.starts_with(b"FILE") && u16_at(record, 22)? & 1 != 0 {
                fixup(record, false)?;
                if let Some(rec) = parse_record(record, number)? {
                    index.insert(rec)?;
                }
            } else if !record.starts_with(b"FILE") && record.iter().any(|&b| b != 0) {
                return Err(format!("MFT 레코드 {number}의 서명을 확인할 수 없습니다"));
            }
            p.records = number + 1;
        }
        offset += count as u64;
        if Instant::now() >= next_preview {
            let snapshot_started = Instant::now();
            let preview = index.report(root, p.clone(), true, false, started.elapsed());
            p = preview.progress.clone();
            publish(Event::Preview(preview));
            next_preview =
                Instant::now() + Duration::from_secs(1).max(snapshot_started.elapsed() * 20);
            last_progress = Instant::now();
        } else if last_progress.elapsed() >= Duration::from_millis(100) {
            publish(Event::Progress(p.clone()));
            last_progress = Instant::now();
        }
    }
    let cancelled = cancel.load(Ordering::Relaxed);
    // The live MFT may grow/move. If its mapping changed, discard this scan and
    // let the caller fall back instead of presenting an apparently complete map.
    if !cancelled {
        let after = geometry(&volume)?;
        if after.length != g.length || mft_mapping(&mut volume, &after)? != mapping {
            return Err("스캔 중 MFT 크기 또는 저장 구간이 변경되었습니다".into());
        }
    }
    Ok(index.report(root, p, false, cancelled, started.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_fragmented_stream_across_extents_and_unaligned_ranges() {
        let path = std::env::temp_dir().join(format!(
            "diskmap-mft-reader-{}-{}.bin",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut disk = vec![0; 8192];
        disk[1024..1536].fill(11);
        disk[4096..4608].fill(22);
        std::fs::write(&path, disk).unwrap();
        let mut file = File::open(&path).unwrap();
        let g = Geometry {
            sector: 512,
            cluster: 512,
            record: 1024,
            length: 1024,
            volume_bytes: 8192,
        };
        let runs = [
            Run {
                vcn: 0,
                lcn: Some(2),
                clusters: 1,
            },
            Run {
                vcn: 1,
                lcn: Some(8),
                clusters: 1,
            },
        ];
        let data = read_stream(&mut file, &runs, &g, 500, 24).unwrap();
        assert_eq!(&data[..12], &[11; 12]);
        assert_eq!(&data[12..], &[22; 12]);
        assert!(read_stream(&mut file, &runs, &g, 1024, 1).is_err());
        let outside = [Run {
            vcn: 0,
            lcn: Some(u64::MAX),
            clusters: 1,
        }];
        assert!(read_stream(&mut file, &outside, &g, 0, 512).is_err());
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn volume_geometry_offsets_and_version_are_checked() {
        let mut data = vec![0; 128];
        for (offset, value) in [(40, 512u32), (44, 4096), (48, 1024)] {
            data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        data[56..64].copy_from_slice(&8192u64.to_le_bytes());
        data[8..16].copy_from_slice(&10000u64.to_le_bytes());
        data[100..102].copy_from_slice(&3u16.to_le_bytes());
        data[102..104].copy_from_slice(&1u16.to_le_bytes());
        let g = decode_geometry(&data).unwrap();
        assert_eq!(g.record, 1024);
        assert_eq!(g.length, 8192);
        data[100] = 4;
        assert!(decode_geometry(&data).is_err());
        assert!(decode_geometry(&data[..96]).is_err());
    }

    #[test]
    #[ignore = "Requires administrator rights and DISKMAP_MFT_VOLUME, e.g. C:\\"]
    fn live_mft_volume_scan() {
        let root = std::env::var("DISKMAP_MFT_VOLUME")
            .expect("Set DISKMAP_MFT_VOLUME to the test drive root");
        let root = volume_root(Path::new(&root)).expect("Use a drive root");
        let report = scan(&root, &AtomicBool::new(false), &mut |_| {})
            .expect("Raw MFT read must succeed; this test never falls back");
        assert_eq!(report.progress.engine, "MFT 고속");
        assert_eq!(report.progress.records, report.progress.total_records);
        assert!(report.nodes.len() > 1);
        assert!(!report.cancelled && !report.scanning);
        println!(
            "MFT: {:?}, {} files, {} directories, {} bytes",
            report.elapsed, report.progress.files, report.progress.folders, report.progress.bytes
        );
    }
}
