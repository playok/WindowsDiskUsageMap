//! Replace one subtree atomically, preserving the rest of the displayed snapshot.
use crate::scan::{self, Node, Progress, Report};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::Instant,
};

pub enum Event {
    Progress(Progress),
    Finished(Result<Report, String>),
}

pub fn start(base: Arc<Report>, target: usize) -> (Receiver<Event>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::sync_channel(2);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    thread::spawn(move || {
        let result = refresh(&base, target, &worker_cancel, |p| {
            let _ = tx.try_send(Event::Progress(p));
        });
        let _ = tx.send(Event::Finished(result));
    });
    (rx, cancel)
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("부분 재스캔을 중지했습니다. 기존 지도와 저장 결과를 유지합니다.".into())
    } else {
        Ok(())
    }
}

pub fn refresh(
    base: &Report,
    target: usize,
    cancel: &AtomicBool,
    mut publish: impl FnMut(Progress),
) -> Result<Report, String> {
    check_cancel(cancel)?;
    if target >= base.nodes.len() || base.scanning {
        return Err("재스캔할 완료된 폴더 결과가 없습니다.".into());
    }
    let started = Instant::now();
    let mut remove = None;
    let mut cursor = target;
    // A missing drive/root is not evidence that all its contents were deleted.
    // Ascend only within the old tree, and confirm deletion by listing its parent.
    loop {
        let n = &base.nodes[cursor];
        match fs::symlink_metadata(&n.path) {
            Ok(m) => {
                if !m.is_dir() || scan::is_link(&m) {
                    return Err("경로가 일반 폴더가 아닙니다. 상위 폴더를 재스캔하세요.".into());
                }
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && n.parent.is_some() => {
                remove = Some(cursor);
                cursor = n.parent.unwrap();
            }
            Err(e) => {
                return Err(format!(
                    "경로를 확인할 수 없어 기존 결과를 유지합니다: {}: {e}",
                    n.path.display()
                ));
            }
        }
    }
    let effective = remove.unwrap_or(target);
    // Prevent an ancestor replaced by a junction from redirecting this operation.
    let mut ancestor = Some(cursor);
    while let Some(id) = ancestor {
        let n = &base.nodes[id];
        let m = fs::symlink_metadata(&n.path)
            .map_err(|e| format!("상위 경로 확인 실패: {}: {e}", n.path.display()))?;
        if !m.is_dir() || scan::is_link(&m) {
            return Err(
                "상위 경로가 링크 또는 다른 형식으로 변경되어 기존 결과를 유지합니다.".into(),
            );
        }
        ancestor = n.parent;
    }
    if let Some(id) = remove {
        let parent = base.nodes[id].parent.unwrap();
        let name = base.nodes[id]
            .path
            .file_name()
            .ok_or("삭제 경로를 확인할 수 없습니다.")?;
        for entry in fs::read_dir(&base.nodes[parent].path).map_err(|e| e.to_string())? {
            check_cancel(cancel)?;
            if entry.map_err(|e| e.to_string())?.file_name() == name {
                return Err(
                    "경로가 다시 나타났습니다. 기존 결과를 유지합니다. 재스캔을 다시 실행하세요."
                        .into(),
                );
            }
        }
    }
    let replacement = if remove.is_none() {
        let mut on_event = |event| {
            let p = match event {
                scan::Event::Progress(p) | scan::Event::Restart(p) => Some(p),
                scan::Event::Preview(r) => Some(r.progress),
                _ => None,
            };
            if let Some(mut p) = p {
                p.note = format!(
                    "부분 재스캔 중: {} · 완료 후 지도에 적용",
                    base.nodes[target].path.display()
                );
                publish(p);
            }
        };
        let report = if target == 0 {
            scan::scan_auto_preserving(&base.nodes[target].path, cancel, &mut on_event, Some(base))?
        } else {
            scan::scan_refresh(&base.nodes[target].path, cancel, &mut on_event, base)?
        };
        check_cancel(cancel)?;
        if report.cancelled {
            return Err(format!(
                "재스캔 중 읽기 오류가 있어 기존 결과를 유지합니다. {}",
                report
                    .issues
                    .first()
                    .map(String::as_str)
                    .unwrap_or("스캔 중지")
            ));
        }
        Some(report)
    } else {
        None
    };
    if effective == 0 {
        return replacement.ok_or_else(|| "루트 삭제는 부분 갱신으로 처리하지 않습니다.".into());
    }

    // Older snapshots only store aggregated timestamps. Refresh direct files in
    // strict ancestors so deleting the newest child can reduce their dates too.
    let mut direct = HashMap::new();
    let mut ancestor_issues = vec![];
    let mut ancestor = base.nodes[effective].parent;
    while let Some(id) = ancestor {
        check_cancel(cancel)?;
        let n = &base.nodes[id];
        publish(Progress {
            engine: "폴더 열거",
            current: n.path.clone(),
            note: "상위 폴더의 직속 파일 정보 갱신 중…".into(),
            ..Default::default()
        });
        let updated = match read_direct(&n.path, cancel) {
            Ok(node) => node,
            Err(error) => {
                check_cancel(cancel)?;
                ancestor_issues.push(format!(
                    "{}: {error} (직속 파일은 이전 집계 유지)",
                    n.path.display()
                ));
                let mut node = n.clone();
                node.files = n
                    .files
                    .saturating_sub(n.children.iter().map(|&c| base.nodes[c].files).sum::<u64>());
                node.incomplete = true;
                node
            }
        };
        direct.insert(id, updated);
        ancestor = n.parent;
    }
    let mut merged = merge(base, effective, replacement.as_ref(), &direct, cancel)?;
    merged.progress.errors = merged
        .progress
        .errors
        .saturating_add(ancestor_issues.len() as u64);
    merged.issues.extend(ancestor_issues);
    merged.issues.truncate(100);
    merged.elapsed = started.elapsed();
    merged.progress.note = format!(
        "부분 갱신: {}{} · 읽기 실패한 항목과 범위 밖 폴더는 이전 집계 유지. 오류·제외 수와 상세는 누적 진단입니다.",
        base.nodes[effective].path.display(),
        if remove.is_some() {
            " (삭제 확인)"
        } else {
            ""
        }
    );
    Ok(merged)
}

fn read_direct(path: &Path, cancel: &AtomicBool) -> Result<Node, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_dir() || scan::is_link(&meta) {
        return Err("상위 폴더 형식이 변경되었습니다.".into());
    }
    let mut node = Node::new(path.to_path_buf(), None);
    node.modified = meta.modified().ok();
    node.missing_modified = node.modified.is_none();
    for entry in
        fs::read_dir(path).map_err(|e| format!("상위 폴더 읽기 실패: {}: {e}", path.display()))?
    {
        check_cancel(cancel)?;
        let entry = entry.map_err(|e| e.to_string())?;
        let m = entry
            .metadata()
            .map_err(|e| format!("상위 항목 읽기 실패: {}: {e}", entry.path().display()))?;
        if scan::is_link(&m) {
            node.incomplete = true;
        } else if m.is_file() {
            node.own_bytes = node.own_bytes.saturating_add(m.len());
            node.files += 1;
            node.modified = node.modified.max(m.modified().ok());
            node.accessed = node.accessed.max(m.accessed().ok());
            node.missing_modified |= m.modified().is_err();
            node.missing_accessed |= m.accessed().is_err();
        }
    }
    Ok(node)
}

#[derive(Clone, Copy)]
enum Source {
    Old(usize),
    New(usize),
}

fn merge(
    base: &Report,
    target: usize,
    replacement: Option<&Report>,
    direct: &HashMap<usize, Node>,
    cancel: &AtomicBool,
) -> Result<Report, String> {
    if let Some(r) = replacement
        && r.nodes[0].path != base.nodes[target].path
    {
        return Err("재스캔 경로가 일치하지 않습니다.".into());
    }
    let mut queue = VecDeque::from([(Source::Old(0), None)]);
    let mut nodes: Vec<Node> = vec![];
    let mut rebuild = vec![];
    while let Some((mut source, parent)) = queue.pop_front() {
        check_cancel(cancel)?;
        if matches!(source, Source::Old(id) if id == target) {
            if replacement.is_none() {
                continue;
            }
            source = Source::New(0);
        }
        let (original, changed) = match source {
            Source::Old(id) => (&base.nodes[id], direct.get(&id)),
            Source::New(id) => (&replacement.unwrap().nodes[id], None),
        };
        let mut node = original.clone();
        if let Some(d) = changed {
            node.own_bytes = d.own_bytes;
            node.bytes = d.own_bytes;
            node.files = d.files;
            node.modified = d.modified;
            node.accessed = d.accessed;
            node.missing_modified = d.missing_modified;
            node.missing_accessed = d.missing_accessed;
            // Retain earlier unresolved diagnostics outside the refreshed subtree.
            node.incomplete |= d.incomplete;
        }
        node.parent = parent;
        node.children.clear();
        let id = nodes.len();
        nodes.push(node);
        rebuild.push(changed.is_some());
        if let Some(parent) = parent {
            nodes[parent].children.push(id);
        }
        for &child in &original.children {
            queue.push_back((
                match source {
                    Source::Old(_) => Source::Old(child),
                    Source::New(_) => Source::New(child),
                },
                Some(id),
            ));
        }
    }
    for id in (0..nodes.len()).rev() {
        check_cancel(cancel)?;
        if !rebuild[id] {
            continue;
        }
        for child in nodes[id].children.clone() {
            let (head, tail) = nodes.split_at_mut(child);
            let n = &mut head[id];
            let c = &tail[0];
            n.bytes = n.bytes.saturating_add(c.bytes);
            n.files = n.files.saturating_add(c.files);
            n.modified = n.modified.max(c.modified);
            n.accessed = n.accessed.max(c.accessed);
            n.missing_modified |= c.missing_modified;
            n.missing_accessed |= c.missing_accessed;
            n.incomplete |= c.incomplete;
        }
    }
    let mut progress = base.progress.clone();
    progress.bytes = nodes[0].bytes;
    progress.files = nodes[0].files;
    progress.folders = nodes.len();
    progress.current = base.nodes[target].path.clone();
    progress.records = 0;
    progress.total_records = 0;
    let mut issues = base.issues.clone();
    if let Some(r) = replacement {
        progress.errors = progress.errors.saturating_add(r.progress.errors);
        progress.skipped = progress.skipped.saturating_add(r.progress.skipped);
        issues.extend(r.issues.iter().cloned());
        issues.truncate(100);
    }
    Ok(Report {
        nodes,
        progress,
        issues,
        cancelled: base.cancelled,
        scanning: false,
        elapsed: base.elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "diskmap-refresh-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(path.join("root/a/nested")).unwrap();
            fs::create_dir_all(path.join("root/b")).unwrap();
            fs::write(path.join("root/root.bin"), [0; 3]).unwrap();
            fs::write(path.join("root/a/a.bin"), [0; 5]).unwrap();
            fs::write(path.join("root/a/nested/n.bin"), [0; 7]).unwrap();
            fs::write(path.join("root/b/b.bin"), [0; 13]).unwrap();
            Self(path)
        }
        fn scan(&self) -> Report {
            scan::scan(&self.0.join("root"), &AtomicBool::new(false), |_| {}).unwrap()
        }
        fn id(&self, report: &Report, relative: &str) -> usize {
            report
                .nodes
                .iter()
                .position(|n| n.path == self.0.join(relative))
                .unwrap()
        }
        fn remove(&self, relative: &str) {
            let target = self.0.join(relative).canonicalize().unwrap();
            let root = self.0.canonicalize().unwrap();
            assert!(target.starts_with(&root) && target != root);
            fs::remove_dir_all(target).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let target = self.0.canonicalize().unwrap();
            let root = std::env::temp_dir().canonicalize().unwrap();
            assert_eq!(target.parent(), Some(root.as_path()));
            assert!(
                target
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("diskmap-refresh-")
            );
            fs::remove_dir_all(target).unwrap();
        }
    }
    fn check_tree(r: &Report) {
        for (id, n) in r.nodes.iter().enumerate() {
            let sum = n.children.iter().fold(n.own_bytes, |sum, &c| {
                assert!(c > id);
                assert_eq!(r.nodes[c].parent, Some(id));
                sum + r.nodes[c].bytes
            });
            assert_eq!(n.bytes, sum);
        }
        assert_eq!(r.progress.bytes, r.nodes[0].bytes);
        assert_eq!(r.progress.files, r.nodes[0].files);
    }

    #[test]
    fn replaces_only_selected_subtree_and_persists_whole_map() {
        let f = Fixture::new();
        let old = f.scan();
        let id = f.id(&old, "root/a");
        f.remove("root/a/nested");
        fs::write(f.0.join("root/a/a.bin"), [0; 9]).unwrap();
        fs::create_dir(f.0.join("root/a/new")).unwrap();
        fs::write(f.0.join("root/a/new/new.bin"), [0; 17]).unwrap();
        fs::write(f.0.join("root/b/b.bin"), [0; 99]).unwrap(); // outside selected scope
        let r = refresh(&old, id, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.nodes[0].bytes, 42);
        assert_eq!(r.nodes[0].files, 4);
        assert_eq!(r.nodes[f.id(&r, "root/b")].bytes, 13);
        assert!(!r.nodes.iter().any(|n| n.path.ends_with("nested")));
        check_tree(&r);
        let db = f.0.join("result.sqlite3");
        crate::cache::save(&db, &r, SystemTime::now()).unwrap();
        let restored = crate::cache::load(&db, Some(&old.nodes[0].path))
            .unwrap()
            .unwrap();
        assert_eq!(restored.report.nodes[0].bytes, 42);
        assert_eq!(restored.report.nodes[0].path, old.nodes[0].path);
        assert!(restored.report.progress.note.starts_with("부분 갱신:"));
        check_tree(&restored.report);
    }

    #[test]
    fn deleted_current_folder_is_removed_and_totals_shrink() {
        let f = Fixture::new();
        let old = f.scan();
        let id = f.id(&old, "root/a/nested");
        f.remove("root/a/nested");
        let r = refresh(&old, id, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.nodes[0].bytes, 21);
        assert_eq!(r.nodes[0].files, 3);
        assert_eq!(r.nodes[f.id(&r, "root/a")].bytes, 5);
        check_tree(&r);
    }

    #[test]
    fn deleted_ancestor_is_removed_but_missing_root_is_not() {
        let f = Fixture::new();
        let old = f.scan();
        let id = f.id(&old, "root/a/nested");
        f.remove("root/a");
        let r = refresh(&old, id, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.nodes[0].bytes, 16);
        assert_eq!(r.nodes.len(), 2);
        check_tree(&r);
        fs::rename(f.0.join("root"), f.0.join("offline")).unwrap();
        assert!(refresh(&old, id, &AtomicBool::new(false), |_| {}).is_err());
        assert_eq!(old.nodes[0].bytes, 28);
    }

    #[test]
    fn deleting_newest_file_recomputes_ancestor_dates() {
        let f = Fixture::new();
        let future = SystemTime::UNIX_EPOCH + Duration::from_secs(2_500_000_000);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(f.0.join("root/a/nested/n.bin"))
            .unwrap();
        file.set_times(
            fs::FileTimes::new()
                .set_modified(future)
                .set_accessed(future),
        )
        .unwrap();
        drop(file);
        let old = f.scan();
        assert_eq!(old.nodes[0].modified, Some(future));
        let id = f.id(&old, "root/a");
        f.remove("root/a/nested");
        let r = refresh(&old, id, &AtomicBool::new(false), |_| {}).unwrap();
        assert!(r.nodes[0].modified.unwrap() < future);
        assert!(r.nodes[0].accessed.unwrap() < future);
    }

    #[test]
    fn cancellation_or_changed_type_never_replaces_original() {
        let f = Fixture::new();
        let old = f.scan();
        let id = f.id(&old, "root/a");
        assert!(refresh(&old, id, &AtomicBool::new(true), |_| {}).is_err());
        let cancel = AtomicBool::new(false);
        assert!(
            refresh(&old, id, &cancel, |_| {
                cancel.store(true, Ordering::Relaxed);
            })
            .is_err()
        );
        fs::rename(f.0.join("root/a"), f.0.join("moved")).unwrap();
        fs::write(f.0.join("root/a"), [0; 5]).unwrap();
        assert!(refresh(&old, id, &AtomicBool::new(false), |_| {}).is_err());
        assert_eq!(old.nodes[0].bytes, 28);
    }

    #[test]
    #[cfg(windows)]
    fn locked_child_keeps_old_data_while_siblings_refresh_and_cache_roundtrips() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = Fixture::new();
        fs::create_dir(f.0.join("root/a/ok")).unwrap();
        fs::create_dir(f.0.join("root/a/nested/deeper")).unwrap();
        fs::write(f.0.join("root/a/nested/deeper/file.bin"), [0; 2]).unwrap();
        fs::create_dir(f.0.join("root/a/deleted")).unwrap();
        fs::write(f.0.join("root/a/deleted/file.bin"), [0; 19]).unwrap();
        fs::write(f.0.join("root/a/ok/file.bin"), [0; 11]).unwrap();
        let old = f.scan();
        f.remove("root/a/deleted");
        fs::write(f.0.join("root/a/ok/file.bin"), [0; 31]).unwrap();
        fs::write(f.0.join("root/a/a.bin"), [0; 9]).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .custom_flags(0x02000000) // FILE_FLAG_BACKUP_SEMANTICS: directory handle
            .open(f.0.join("root/a/nested"))
            .unwrap();
        let r = refresh(&old, f.id(&old, "root/a"), &AtomicBool::new(false), |_| {}).unwrap();
        assert!(r.progress.errors > 0);
        assert_eq!(r.nodes[f.id(&r, "root/a/nested")].bytes, 9);
        assert_eq!(r.nodes[f.id(&r, "root/a/nested/deeper")].bytes, 2);
        assert!(!r.nodes.iter().any(|n| n.path.ends_with("deleted")));
        assert!(r.nodes[f.id(&r, "root/a/nested")].incomplete);
        assert_eq!(r.nodes[f.id(&r, "root/a/ok")].bytes, 31);
        assert!(!r.nodes[f.id(&r, "root/a/ok")].incomplete);
        assert_eq!(r.nodes[0].bytes, 65);
        assert_eq!(r.nodes[0].files, 6);
        check_tree(&r);
        let db = f.0.join("partial.sqlite3");
        crate::cache::save(&db, &r, SystemTime::now()).unwrap();
        let restored = crate::cache::load(&db, Some(&old.nodes[0].path))
            .unwrap()
            .unwrap();
        check_tree(&restored.report);
        assert_eq!(restored.report.nodes[0].bytes, 65);
        let root_refresh = refresh(&old, 0, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(root_refresh.nodes[0].bytes, 65);
        check_tree(&root_refresh);
        drop(lock);
        let recovered = refresh(&r, f.id(&r, "root/a"), &AtomicBool::new(false), |_| {}).unwrap();
        assert!(!recovered.nodes[f.id(&recovered, "root/a/nested")].incomplete);
    }

    #[test]
    #[cfg(windows)]
    fn ancestor_read_failure_does_not_discard_successful_subtree_refresh() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = Fixture::new();
        let old = f.scan();
        fs::write(f.0.join("root/a/a.bin"), [0; 25]).unwrap();
        let mut lock = None;
        let r = refresh(&old, f.id(&old, "root/a"), &AtomicBool::new(false), |p| {
            if p.note.starts_with("상위 폴더의") && lock.is_none() {
                lock = Some(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(0)
                        .custom_flags(0x02000000)
                        .open(f.0.join("root"))
                        .unwrap(),
                );
            }
        })
        .unwrap();
        drop(lock);
        assert_eq!(r.nodes[0].bytes, 48);
        assert_eq!(r.nodes[0].files, 4);
        assert!(r.nodes[0].incomplete);
        assert!(r.progress.errors > 0);
        check_tree(&r);
    }

    #[test]
    fn root_refresh_replaces_the_entire_map() {
        let f = Fixture::new();
        let old = f.scan();
        f.remove("root/a");
        let r = refresh(&old, 0, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(r.nodes[0].bytes, 16);
        assert_eq!(r.nodes.len(), 2);
        check_tree(&r);
    }
}
