use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Debug)]
pub struct Node {
    pub path: PathBuf,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub bytes: u64,
    pub own_bytes: u64,
    pub files: u64,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub missing_modified: bool,
    pub missing_accessed: bool,
    pub incomplete: bool,
}

impl Node {
    pub(crate) fn new(path: PathBuf, parent: Option<usize>) -> Self {
        Self {
            path,
            parent,
            children: vec![],
            bytes: 0,
            own_bytes: 0,
            files: 0,
            modified: None,
            accessed: None,
            missing_modified: false,
            missing_accessed: false,
            incomplete: false,
        }
    }
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }
    pub fn age_days(&self, access: bool, now: SystemTime) -> Option<u64> {
        if (access && self.missing_accessed) || (!access && self.missing_modified) {
            return None;
        }
        let date = if access { self.accessed } else { self.modified }?;
        Some(now.duration_since(date).unwrap_or_default().as_secs() / 86400)
    }
}

#[derive(Default, Clone)]
pub struct Progress {
    pub engine: &'static str,
    pub note: String,
    pub records: u64,
    pub total_records: u64,
    pub folders: usize,
    pub files: u64,
    pub bytes: u64,
    pub errors: u64,
    pub skipped: u64,
    pub current: PathBuf,
}

pub struct Report {
    pub nodes: Vec<Node>,
    pub progress: Progress,
    pub issues: Vec<String>,
    pub cancelled: bool,
    pub scanning: bool,
    pub elapsed: Duration,
}

pub enum Event {
    Restart(Progress),
    Progress(Progress),
    Preview(Report),
    Finished(Report),
    Failed(String),
}

pub fn start(path: PathBuf) -> (Receiver<Event>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::sync_channel(2);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    thread::spawn(move || {
        let result = scan_auto(&path, &worker_cancel, |event| {
            if matches!(event, Event::Restart(_)) {
                let _ = tx.send(event);
                return;
            }
            let _ = tx.try_send(event);
        });
        let event = match result {
            Ok(r) => Event::Finished(r),
            Err(e) => Event::Failed(e),
        };
        let _ = tx.send(event);
    });
    (rx, cancel)
}

pub fn scan_auto(
    path: &Path,
    cancel: &AtomicBool,
    publish: impl FnMut(Event),
) -> Result<Report, String> {
    scan_auto_preserving(path, cancel, publish, None)
}

pub(crate) fn scan_auto_preserving(
    path: &Path,
    cancel: &AtomicBool,
    mut publish: impl FnMut(Event),
    base: Option<&Report>,
) -> Result<Report, String> {
    let mut note = String::new();
    #[cfg(windows)]
    if let Some(root) = crate::mft::volume_root(path) {
        match crate::mft::windows::scan(&root, cancel, &mut publish) {
            Ok(report) => return Ok(report),
            Err(error) => {
                note = format!("MFT를 사용할 수 없어 폴더 열거로 전환: {error}");
                publish(Event::Restart(Progress {
                    engine: "폴더 열거",
                    note: note.clone(),
                    ..Default::default()
                }));
            }
        }
    }
    if note.is_empty() {
        note =
            "폴더 경로는 기존 방식으로 스캔합니다. MFT는 로컬 드라이브 루트에서 사용합니다.".into();
    }
    let on_event = |mut event| {
        match &mut event {
            Event::Progress(p) | Event::Restart(p) => p.note.clone_from(&note),
            Event::Preview(r) | Event::Finished(r) => r.progress.note.clone_from(&note),
            Event::Failed(_) => {}
        }
        publish(event);
    };
    let mut report = if let Some(base) = base {
        scan_refresh(path, cancel, on_event, base)
    } else {
        scan(path, cancel, on_event)
    }?;
    if base.is_some() && report.progress.errors > 0 {
        note.push_str(" · 읽기 실패한 부분은 이전 집계를 유지했습니다.");
    }
    report.progress.note = note;
    Ok(report)
}

#[cfg(windows)]
pub(crate) fn is_link(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT (junction 포함)
}
#[cfg(not(windows))]
pub(crate) fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

fn latest(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    a.max(b)
}

pub fn scan(
    path: &Path,
    cancel: &AtomicBool,
    publish: impl FnMut(Event),
) -> Result<Report, String> {
    scan_with_interval(path, cancel, Duration::from_millis(250), publish)
}

fn scan_with_interval(
    path: &Path,
    cancel: &AtomicBool,
    first_preview: Duration,
    publish: impl FnMut(Event),
) -> Result<Report, String> {
    scan_preserving(path, cancel, first_preview, publish, None)
}

pub(crate) fn scan_refresh(
    path: &Path,
    cancel: &AtomicBool,
    publish: impl FnMut(Event),
    base: &Report,
) -> Result<Report, String> {
    scan_preserving(
        path,
        cancel,
        Duration::from_millis(250),
        publish,
        Some(base),
    )
}

fn scan_preserving(
    path: &Path,
    cancel: &AtomicBool,
    first_preview: Duration,
    mut publish: impl FnMut(Event),
    base: Option<&Report>,
) -> Result<Report, String> {
    let started = Instant::now();
    let meta = fs::symlink_metadata(path).map_err(|e| format!("경로를 읽을 수 없습니다: {e}"))?;
    if !meta.is_dir() || is_link(&meta) {
        return Err("일반 디렉터리 또는 드라이브 루트를 선택하세요 (링크 제외).".into());
    }
    let mut nodes = vec![Node::new(path.to_path_buf(), None)];
    // Breadth first: discover the top-level branches before drilling into one subtree.
    let mut pending = VecDeque::from([0]);
    let mut p = Progress {
        engine: "폴더 열거",
        ..Default::default()
    };
    let mut issues = vec![];
    let mut failed = vec![];
    let mut updates = Updates::new(first_preview, &mut publish);
    while let Some(id) = pending.pop_front() {
        updates.emit(&nodes, &p, &issues, started);
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        p.current = nodes[id].path.clone();
        p.folders += 1;
        // Directory modification dates include creation/removal of child entries.
        if let Ok(m) = fs::symlink_metadata(&p.current) {
            if is_link(&m) {
                nodes[id].incomplete = true;
                p.skipped += 1;
                continue;
            }
            nodes[id].modified = m.modified().ok();
            nodes[id].missing_modified = nodes[id].modified.is_none();
        }
        let entries = match fs::read_dir(&p.current) {
            Ok(entries) => entries,
            Err(e) => {
                record_error(&mut nodes[id], &mut p, &mut issues, e.to_string());
                failed.push(id);
                continue;
            }
        };
        let errors_before = p.errors;
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    record_error(&mut nodes[id], &mut p, &mut issues, e.to_string());
                    continue;
                }
            };
            // On Windows DirEntry caches metadata from directory enumeration;
            // querying the path again would perform another I/O for every file.
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    record_error(
                        &mut nodes[id],
                        &mut p,
                        &mut issues,
                        format!("{}: {e}", entry.path().display()),
                    );
                    continue;
                }
            };
            if is_link(&meta) {
                p.skipped += 1;
                nodes[id].incomplete = true;
            } else if meta.is_dir() {
                let child = nodes.len();
                nodes.push(Node::new(entry.path(), Some(id)));
                nodes[id].children.push(child);
                pending.push_back(child);
            } else if meta.is_file() {
                let n = &mut nodes[id];
                n.own_bytes = n.own_bytes.saturating_add(meta.len());
                n.files += 1;
                n.modified = latest(n.modified, meta.modified().ok());
                n.accessed = latest(n.accessed, meta.accessed().ok());
                n.missing_modified |= meta.modified().is_err();
                n.missing_accessed |= meta.accessed().is_err();
                p.files += 1;
                p.bytes = p.bytes.saturating_add(meta.len());
            }
            updates.emit(&nodes, &p, &issues, started);
        }
        if p.errors != errors_before {
            failed.push(id);
        }
    }
    let cancelled = cancel.load(Ordering::Relaxed);
    if let Some(base) = base {
        preserve_failed(&mut nodes, &failed, base);
    }
    aggregate(&mut nodes, cancelled);
    if base.is_some() {
        p.bytes = nodes[0].bytes;
        p.files = nodes[0].files;
        p.folders = nodes.len();
    }
    Ok(Report {
        nodes,
        progress: p,
        issues,
        cancelled,
        scanning: false,
        elapsed: started.elapsed(),
    })
}

// Only directories whose own enumeration failed need fallback. An error in a
// descendant must not discard successful refreshes in its siblings.
fn preserve_failed(nodes: &mut Vec<Node>, failed: &[usize], base: &Report) {
    let old: std::collections::HashMap<_, _> =
        base.nodes.iter().map(|n| (n.path.as_path(), n)).collect();
    for &id in failed {
        let Some(previous) = old.get(nodes[id].path.as_path()) else {
            continue;
        };
        let node = &mut nodes[id];
        node.own_bytes = previous.own_bytes;
        node.files = previous.files.saturating_sub(
            previous
                .children
                .iter()
                .map(|&c| base.nodes[c].files)
                .sum::<u64>(),
        );
        node.modified = node.modified.max(previous.modified);
        node.accessed = node.accessed.max(previous.accessed);
        node.missing_modified |= previous.missing_modified;
        node.missing_accessed |= previous.missing_accessed;
        node.incomplete = true;
        let known: std::collections::HashSet<_> = nodes[id]
            .children
            .iter()
            .map(|&c| nodes[c].path.clone())
            .collect();
        let mut queue = VecDeque::new();
        for &child in &previous.children {
            if !known.contains(&base.nodes[child].path) {
                queue.push_back((child, id));
            }
        }
        while let Some((old_id, parent)) = queue.pop_front() {
            let previous = &base.nodes[old_id];
            let mut node = previous.clone();
            node.bytes = 0;
            node.files = previous.files.saturating_sub(
                previous
                    .children
                    .iter()
                    .map(|&c| base.nodes[c].files)
                    .sum::<u64>(),
            );
            node.parent = Some(parent);
            node.children.clear();
            node.incomplete = true;
            let id = nodes.len();
            nodes.push(node);
            nodes[parent].children.push(id);
            queue.extend(previous.children.iter().map(|&c| (c, id)));
        }
    }
}

// Raw nodes retain direct-file totals. Aggregate only a snapshot or the final
// owned tree, so repeated previews never double count descendant sizes.
pub(crate) fn aggregate(nodes: &mut [Node], partial: bool) {
    // Children are always created after parents: reverse aggregation avoids recursion.
    for id in (0..nodes.len()).rev() {
        nodes[id].bytes = nodes[id].bytes.saturating_add(nodes[id].own_bytes);
        nodes[id].incomplete |= partial;
        if let Some(parent) = nodes[id].parent {
            let bytes = nodes[id].bytes;
            let files = nodes[id].files;
            let modified = nodes[id].modified;
            let accessed = nodes[id].accessed;
            let mm = nodes[id].missing_modified;
            let ma = nodes[id].missing_accessed;
            let incomplete = nodes[id].incomplete;
            let n = &mut nodes[parent];
            n.bytes = n.bytes.saturating_add(bytes);
            n.files += files;
            n.modified = latest(n.modified, modified);
            n.accessed = latest(n.accessed, accessed);
            n.missing_modified |= mm;
            n.missing_accessed |= ma;
            n.incomplete |= incomplete;
        }
    }
}

struct Updates<F> {
    publish: F,
    progress_at: Instant,
    preview_at: Instant,
}

impl<F: FnMut(Event)> Updates<F> {
    fn new(first_preview: Duration, publish: F) -> Self {
        Self {
            publish,
            progress_at: Instant::now(),
            preview_at: Instant::now() + first_preview,
        }
    }

    fn emit(&mut self, nodes: &[Node], p: &Progress, issues: &[String], started: Instant) {
        let now = Instant::now();
        if now >= self.preview_at && p.bytes > 0 {
            let mut snapshot = nodes.to_vec();
            aggregate(&mut snapshot, true);
            (self.publish)(Event::Preview(Report {
                nodes: snapshot,
                progress: p.clone(),
                issues: issues.to_vec(),
                cancelled: false,
                scanning: true,
                elapsed: started.elapsed(),
            }));
            // Bound snapshot overhead on huge trees: at least 1 second between
            // previews, or 20 times the last snapshot cost when that is larger.
            self.preview_at = Instant::now() + Duration::from_secs(1).max(now.elapsed() * 20);
            self.progress_at = Instant::now();
        } else if self.progress_at.elapsed() >= Duration::from_millis(100) {
            (self.publish)(Event::Progress(p.clone()));
            self.progress_at = Instant::now();
        }
    }
}

fn record_error(node: &mut Node, p: &mut Progress, issues: &mut Vec<String>, error: String) {
    node.incomplete = true;
    p.errors += 1;
    if issues.len() < 100 {
        issues.push(format!("{}: {error}", node.path.display()));
    }
}

pub fn drives() -> Vec<String> {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetLogicalDrives() -> u32;
        }
        // No pointers or ownership involved; Windows returns a drive-letter bitmask.
        let mask = unsafe { GetLogicalDrives() };
        (0..26)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| format!("{}:\\", (b'A' + i) as char))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec!["/".into()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "diskusagemap-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(p.join("a/b")).unwrap();
            Self(p)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let root = std::env::temp_dir().canonicalize().unwrap();
            if let Ok(target) = self.0.canonicalize() {
                assert_eq!(target.parent(), Some(root.as_path()));
                assert!(
                    target
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("diskusagemap-")
                );
                let _ = fs::remove_dir_all(target);
            }
        }
    }
    #[test]
    fn aggregates_nested_files_and_empty_folders() {
        let f = Fixture::new();
        fs::write(f.0.join("root.bin"), [0; 11]).unwrap();
        fs::write(f.0.join("a/b/data.bin"), [0; 29]).unwrap();
        fs::create_dir(f.0.join("empty")).unwrap();
        let r = scan(&f.0, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.nodes[0].bytes, 40);
        assert_eq!(r.nodes[0].own_bytes, 11);
        assert_eq!(r.nodes[0].files, 2);
        assert_eq!(r.nodes.len(), 4);
        assert!(!r.nodes[0].incomplete);
        assert_eq!(r.nodes.iter().find(|n| n.name() == "a").unwrap().bytes, 29);
    }

    #[test]
    fn preview_precedes_completion_without_corrupting_final_totals() {
        let f = Fixture::new();
        fs::write(f.0.join("first.bin"), [0; 11]).unwrap();
        fs::write(f.0.join("a/b/last.bin"), [0; 29]).unwrap();
        let mut previews = 0;
        let r = scan_with_interval(&f.0, &AtomicBool::new(false), Duration::ZERO, |event| {
            if let Event::Preview(p) = event {
                previews += 1;
                assert!(p.scanning && !p.cancelled);
                assert!(p.nodes[0].incomplete);
                assert_eq!(p.nodes[0].bytes, 11);
                assert_eq!(p.nodes[0].files, 1);
                assert_eq!(p.progress.bytes, p.nodes[0].bytes);
            }
        })
        .unwrap();
        assert_eq!(previews, 1);
        assert_eq!(r.nodes[0].bytes, 40);
        assert_eq!(r.nodes[0].files, 2);
        assert!(!r.scanning && !r.nodes[0].incomplete);
    }

    #[test]
    fn cancellation_after_preview_keeps_partial_totals() {
        let f = Fixture::new();
        fs::write(f.0.join("first.bin"), [0; 11]).unwrap();
        fs::write(f.0.join("a/b/last.bin"), [0; 29]).unwrap();
        let cancel = AtomicBool::new(false);
        let r = scan_with_interval(&f.0, &cancel, Duration::ZERO, |event| {
            if matches!(event, Event::Preview(_)) {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();
        assert!(r.cancelled && !r.scanning && r.nodes[0].incomplete);
        assert_eq!(r.nodes[0].bytes, 11);
        assert_eq!(r.nodes[0].files, 1);
    }

    #[test]
    #[ignore = "Manual metadata throughput comparison on a synthetic directory"]
    fn benchmark_windows_metadata_queries() {
        let f = Fixture::new();
        for i in 0..5000 {
            fs::write(f.0.join(format!("{i}.bin")), [0; 8]).unwrap();
        }
        let mut path_times = vec![];
        let mut entry_times = vec![];
        for round in 0..6 {
            for cached in if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let started = Instant::now();
                let mut bytes = 0;
                for entry in fs::read_dir(&f.0).unwrap() {
                    let entry = entry.unwrap();
                    let m = if cached {
                        entry.metadata().unwrap()
                    } else {
                        fs::symlink_metadata(entry.path()).unwrap()
                    };
                    if m.is_file() {
                        bytes += m.len();
                    }
                }
                assert_eq!(bytes, 40000);
                if cached {
                    entry_times.push(started.elapsed());
                } else {
                    path_times.push(started.elapsed());
                }
            }
        }
        path_times.sort();
        entry_times.sort();
        println!(
            "5000 files, warm-cache median: path queries {:?}; directory metadata {:?}",
            path_times[3], entry_times[3]
        );
    }
    #[test]
    fn cancellation_marks_partial_results() {
        let f = Fixture::new();
        let r = scan(&f.0, &AtomicBool::new(true), |_| {}).unwrap();
        assert!(r.cancelled && r.nodes[0].incomplete);
    }
    #[test]
    fn invalid_root_is_an_error() {
        let f = Fixture::new();
        assert!(scan(&f.0.join("missing"), &AtomicBool::new(false), |_| {}).is_err());
    }

    #[test]
    fn automatic_scan_uses_directory_backend_for_folder_paths() {
        let f = Fixture::new();
        fs::write(f.0.join("file.bin"), [0; 7]).unwrap();
        let r = scan_auto(&f.0, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.progress.engine, "폴더 열거");
        assert_eq!(r.nodes[0].bytes, 7);
        assert!(!r.progress.note.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn unprivileged_volume_scan_falls_back_and_respects_cancellation() {
        let root = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()) + "\\";
        let r = scan_auto(Path::new(&root), &AtomicBool::new(true), |_| {}).unwrap();
        // On an elevated test runner MFT may open successfully; otherwise the
        // fallback must have an explanation and must not traverse the drive.
        assert!(r.cancelled);
        assert_eq!(r.progress.files, 0);
        if r.progress.engine == "폴더 열거" {
            assert!(r.progress.note.contains("MFT"));
        }
    }
    #[test]
    fn latest_descendant_timestamp_reaches_root() {
        let f = Fixture::new();
        let file = fs::File::create(f.0.join("a/b/new.bin")).unwrap();
        let future = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        file.set_times(
            fs::FileTimes::new()
                .set_modified(future)
                .set_accessed(future),
        )
        .unwrap();
        drop(file);
        let r = scan(&f.0, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.nodes[0].modified, Some(future));
        assert_eq!(r.nodes[0].accessed, Some(future));
    }
    #[test]
    fn worker_delivers_a_finished_report() {
        let f = Fixture::new();
        let (rx, _) = start(f.0.clone());
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Event::Finished(r) => {
                    assert_eq!(r.nodes.len(), 3);
                    break;
                }
                Event::Failed(e) => panic!("{e}"),
                Event::Progress(_) | Event::Preview(_) | Event::Restart(_) => {}
            }
        }
    }
    #[test]
    fn unknown_and_future_dates_are_conservative() {
        let mut n = Node::new(PathBuf::new(), None);
        let now = SystemTime::now();
        assert_eq!(n.age_days(true, now), None);
        n.accessed = Some(now + Duration::from_secs(100));
        assert_eq!(n.age_days(true, now), Some(0));
        n.missing_accessed = true;
        assert_eq!(n.age_days(true, now), None);
    }

    #[cfg(windows)]
    #[test]
    fn junction_cycles_are_skipped_and_flagged() {
        let f = Fixture::new();
        let link = f.0.join("a/b/cycle");
        let result = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference = 'Stop'; $null = New-Item -ItemType Junction -Path $env:DISKMAP_TEST_LINK -Target $env:DISKMAP_TEST_TARGET"])
            .env("DISKMAP_TEST_LINK", &link).env("DISKMAP_TEST_TARGET", &f.0)
            .output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let r = scan(&f.0, &AtomicBool::new(false), |_| {}).unwrap();
        fs::remove_dir(&link).unwrap();
        assert_eq!(r.nodes.len(), 3);
        assert_eq!(r.progress.skipped, 1);
        assert!(r.nodes[0].incomplete);
    }
}
