//! Transactional, versioned snapshots of the directory tree (not individual files).
use crate::scan::{Node, Progress, Report};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, SystemTime},
};

type Result<T> = std::result::Result<T, String>;
const VERSION: i64 = 1;
const APPLICATION: i64 = 0x44554d50;

pub struct Cached {
    pub report: Report,
    pub saved_at: SystemTime,
}
pub enum Request {
    Load {
        generation: u64,
        root: Option<PathBuf>,
    },
    Save {
        generation: u64,
        report: Arc<Report>,
        saved_at: SystemTime,
    },
}
pub enum Event {
    Loaded {
        generation: u64,
        result: Result<Option<Cached>>,
    },
    Saved {
        generation: u64,
        result: Result<SystemTime>,
    },
}
pub struct Worker {
    pub tx: Sender<Request>,
    pub rx: Receiver<Event>,
    pub path: PathBuf,
}

pub fn default_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("DiskUsageMap/snapshots.sqlite3"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
            .map(|p| p.join("diskusagemap/snapshots.sqlite3"))
    }
}

pub fn start(path: PathBuf) -> Worker {
    let (tx, requests) = mpsc::channel();
    let (events, rx) = mpsc::channel();
    let db = path.clone();
    thread::spawn(move || {
        while let Ok(request) = requests.recv() {
            let event = match request {
                Request::Load { generation, root } => Event::Loaded {
                    generation,
                    result: load(&db, root.as_deref()),
                },
                Request::Save {
                    generation,
                    report,
                    saved_at,
                } => Event::Saved {
                    generation,
                    result: save(&db, &report, saved_at).map(|()| saved_at),
                },
            };
            if events.send(event).is_err() {
                break;
            }
        }
    });
    Worker { tx, rx, path }
}

fn number(bytes: Vec<u8>) -> Result<u64> {
    let data: [u8; 8] = bytes.try_into().map_err(|_| "Invalid cached integer")?;
    Ok(u64::from_le_bytes(data))
}

fn index(value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| "Invalid cached index".into())
}
fn encode_time(time: SystemTime) -> Vec<u8> {
    let (negative, d) = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => (0, d),
        Err(e) => (1, e.duration()),
    };
    let mut bytes = vec![negative];
    bytes.extend_from_slice(&d.as_secs().to_le_bytes());
    bytes.extend_from_slice(&d.subsec_nanos().to_le_bytes());
    bytes
}
fn decode_time(bytes: Vec<u8>) -> Result<SystemTime> {
    if bytes.len() != 13 || bytes[0] > 1 {
        return Err("Invalid cached timestamp".into());
    }
    let seconds = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
    let nanos = u32::from_le_bytes(bytes[9..13].try_into().unwrap());
    if nanos >= 1_000_000_000 {
        return Err("Invalid cached nanoseconds".into());
    }
    let d = Duration::new(seconds, nanos);
    (if bytes[0] == 0 {
        SystemTime::UNIX_EPOCH.checked_add(d)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(d)
    })
    .ok_or_else(|| "Cached timestamp out of range".into())
}

fn encode_path(path: &Path) -> Vec<u8> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
}
fn decode_path(bytes: Vec<u8>) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        if !bytes.len().is_multiple_of(2) {
            return Err("Invalid UTF-16 path".into());
        }
        let wide: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| u16::from_le_bytes(*v))
            .collect();
        if wide.contains(&0) {
            return Err("Invalid path with NUL".into());
        }
        Ok(PathBuf::from(OsString::from_wide(&wide)))
    }
    #[cfg(not(windows))]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        if bytes.contains(&0) {
            return Err("Invalid path with NUL".into());
        }
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }
}
fn key(path: &Path) -> Result<Vec<u8>> {
    let path = std::path::absolute(path).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        use std::{
            ffi::OsString,
            os::windows::ffi::{OsStrExt, OsStringExt},
        };
        let mut wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .map(|c| match c {
                65..=90 => c + 32,
                47 => 92,
                other => other,
            })
            .collect();
        while wide.len() > 3 && wide.last() == Some(&92) {
            wide.pop();
        }
        Ok(encode_path(Path::new(&OsString::from_wide(&wide))))
    }
    #[cfg(not(windows))]
    {
        Ok(encode_path(&path))
    }
}

fn check_schema(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let app: i64 = conn
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if version != VERSION || app != APPLICATION {
        return Err("저장 DB 형식이 이 버전과 호환되지 않습니다. DB를 다른 이름으로 옮긴 뒤 다시 스캔하세요.".into());
    }
    Ok(())
}

fn connection(db: &Path, create: bool) -> Result<Connection> {
    if create && let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let flags = if create {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut conn = Connection::open_with_flags(db, flags).map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(3))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;")
        .map_err(|e| e.to_string())?;
    if create {
        conn.execute_batch("PRAGMA synchronous=FULL;")
            .map_err(|e| e.to_string())?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let app: i64 = tx
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if count == 0 && version == 0 && app == 0 {
            tx.execute_batch("CREATE TABLE snapshots (
                id INTEGER PRIMARY KEY, root_key BLOB NOT NULL UNIQUE, saved_seconds INTEGER NOT NULL, saved_nanos INTEGER NOT NULL,
                engine TEXT NOT NULL, note TEXT NOT NULL, elapsed_seconds BLOB NOT NULL, elapsed_nanos INTEGER NOT NULL,
                files BLOB NOT NULL, bytes BLOB NOT NULL, errors BLOB NOT NULL, skipped BLOB NOT NULL,
                records BLOB NOT NULL, total_records BLOB NOT NULL, node_count INTEGER NOT NULL);
                CREATE TABLE nodes (
                snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE, id INTEGER NOT NULL,
                parent INTEGER, path BLOB NOT NULL, bytes BLOB NOT NULL, own_bytes BLOB NOT NULL, files BLOB NOT NULL,
                modified BLOB, accessed BLOB, flags INTEGER NOT NULL, PRIMARY KEY(snapshot_id, id)) WITHOUT ROWID;
                CREATE TABLE issues (snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE, id INTEGER NOT NULL, text TEXT NOT NULL, PRIMARY KEY(snapshot_id,id)) WITHOUT ROWID;") .map_err(|e| e.to_string())?;
            tx.pragma_update(None, "user_version", VERSION)
                .map_err(|e| e.to_string())?;
            tx.pragma_update(None, "application_id", APPLICATION)
                .map_err(|e| e.to_string())?;
        }
        check_schema(&tx)?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    check_schema(&conn)?;
    Ok(conn)
}

fn validate(nodes: &[Node]) -> Result<()> {
    if nodes.is_empty() || nodes[0].parent.is_some() || !nodes[0].path.is_absolute() {
        return Err("Invalid cached root".into());
    }
    let mut sizes: Vec<_> = nodes.iter().map(|n| n.own_bytes).collect();
    for id in (1..nodes.len()).rev() {
        let parent = nodes[id].parent.ok_or("Missing cached parent")?;
        if parent >= id || nodes[id].path.parent() != Some(nodes[parent].path.as_path()) {
            return Err("Invalid cached hierarchy".into());
        }
        sizes[parent] = sizes[parent].saturating_add(nodes[id].bytes);
    }
    if nodes.iter().zip(sizes).any(|(n, sum)| n.bytes != sum) {
        return Err("Cached size totals do not match".into());
    }
    Ok(())
}

pub fn save(db: &Path, report: &Report, saved_at: SystemTime) -> Result<()> {
    if report.cancelled || report.scanning {
        return Err("중지되었거나 진행 중인 스캔은 저장하지 않습니다.".into());
    }
    validate(&report.nodes)?;
    let root_key = key(&report.nodes[0].path)?;
    let saved = saved_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| e.to_string())?;
    let mut conn = connection(db, true)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM snapshots WHERE root_key=?1", params![root_key])
        .map_err(|e| e.to_string())?;
    let p = &report.progress;
    tx.execute("INSERT INTO snapshots(root_key,saved_seconds,saved_nanos,engine,note,elapsed_seconds,elapsed_nanos,files,bytes,errors,skipped,records,total_records,node_count)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)", params![root_key, saved.as_secs() as i64, saved.subsec_nanos(), p.engine, p.note,
            report.elapsed.as_secs().to_le_bytes().as_slice(), report.elapsed.subsec_nanos(), p.files.to_le_bytes().as_slice(), p.bytes.to_le_bytes().as_slice(),
            p.errors.to_le_bytes().as_slice(), p.skipped.to_le_bytes().as_slice(), p.records.to_le_bytes().as_slice(), p.total_records.to_le_bytes().as_slice(), report.nodes.len() as i64]).map_err(|e| e.to_string())?;
    let snapshot = tx.last_insert_rowid();
    {
        let mut stmt = tx
            .prepare_cached("INSERT INTO nodes VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)")
            .map_err(|e| e.to_string())?;
        for (id, n) in report.nodes.iter().enumerate() {
            let flags = i64::from(n.missing_modified)
                | (i64::from(n.missing_accessed) << 1)
                | (i64::from(n.incomplete) << 2);
            stmt.execute(params![
                snapshot,
                id as i64,
                n.parent.map(|p| p as i64),
                encode_path(&n.path),
                n.bytes.to_le_bytes().as_slice(),
                n.own_bytes.to_le_bytes().as_slice(),
                n.files.to_le_bytes().as_slice(),
                n.modified.map(encode_time),
                n.accessed.map(encode_time),
                flags
            ])
            .map_err(|e| e.to_string())?;
        }
        let mut stmt = tx
            .prepare_cached("INSERT INTO issues VALUES (?1,?2,?3)")
            .map_err(|e| e.to_string())?;
        for (id, issue) in report.issues.iter().enumerate() {
            stmt.execute(params![snapshot, id as i64, issue])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())
}

pub fn load(db: &Path, root: Option<&Path>) -> Result<Option<Cached>> {
    if !db.try_exists().map_err(|e| e.to_string())? {
        return Ok(None);
    }
    let mut conn = connection(db, false)?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let root_key = root.map(key).transpose()?;
    let id: Option<i64> = tx.query_row("SELECT id FROM snapshots WHERE (?1 IS NULL OR root_key=?1) ORDER BY saved_seconds DESC,saved_nanos DESC,id DESC LIMIT 1", params![root_key], |r| r.get(0)).optional().map_err(|e| e.to_string())?;
    let Some(id) = id else {
        return Ok(None);
    };
    let mut stmt = tx
        .prepare("SELECT * FROM snapshots WHERE id=?1")
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query([id]).map_err(|e| e.to_string())?;
    let row = rows
        .next()
        .map_err(|e| e.to_string())?
        .ok_or("Snapshot disappeared")?;
    let get_blob = |name| row.get::<_, Vec<u8>>(name).map_err(|e| e.to_string());
    let stored_key = get_blob("root_key")?;
    let saved_sec: i64 = row.get("saved_seconds").map_err(|e| e.to_string())?;
    let saved_nano: u32 = row.get("saved_nanos").map_err(|e| e.to_string())?;
    if saved_sec < 0 || saved_nano >= 1_000_000_000 {
        return Err("Invalid saved date".into());
    }
    let saved_at = SystemTime::UNIX_EPOCH
        .checked_add(Duration::new(saved_sec as u64, saved_nano))
        .ok_or("Invalid saved date")?;
    let engine: String = row.get("engine").map_err(|e| e.to_string())?;
    let engine = match engine.as_str() {
        "MFT 고속" => "MFT 고속",
        "폴더 열거" => "폴더 열거",
        _ => return Err("Unknown cached scan engine".into()),
    };
    let elapsed_nanos: u32 = row.get("elapsed_nanos").map_err(|e| e.to_string())?;
    if elapsed_nanos >= 1_000_000_000 {
        return Err("Invalid cached duration".into());
    }
    let elapsed = Duration::new(number(get_blob("elapsed_seconds")?)?, elapsed_nanos);
    let expected = index(row.get("node_count").map_err(|e| e.to_string())?)?;
    let mut p = Progress {
        engine,
        note: row.get("note").map_err(|e| e.to_string())?,
        bytes: number(get_blob("bytes")?)?,
        files: number(get_blob("files")?)?,
        errors: number(get_blob("errors")?)?,
        skipped: number(get_blob("skipped")?)?,
        records: number(get_blob("records")?)?,
        total_records: number(get_blob("total_records")?)?,
        ..Default::default()
    };
    let mut nodes: Vec<Node> = vec![];
    let mut stmt = tx.prepare("SELECT id,parent,path,bytes,own_bytes,files,modified,accessed,flags FROM nodes WHERE snapshot_id=?1 ORDER BY id").map_err(|e| e.to_string())?;
    let mut rows = stmt.query([id]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let id = index(row.get(0).map_err(|e| e.to_string())?)?;
        if id != nodes.len() {
            return Err("Non-contiguous cached node IDs".into());
        }
        let parent = row
            .get::<_, Option<i64>>(1)
            .map_err(|e| e.to_string())?
            .map(index)
            .transpose()?;
        if (id == 0 && parent.is_some()) || (id > 0 && parent.is_none_or(|p| p >= id)) {
            return Err("Invalid cached parent index".into());
        }
        let flags: i64 = row.get(8).map_err(|e| e.to_string())?;
        if !(0..=7).contains(&flags) {
            return Err("Invalid cached node flags".into());
        }
        let blob = |index| row.get::<_, Vec<u8>>(index).map_err(|e| e.to_string());
        let time = |index| {
            row.get::<_, Option<Vec<u8>>>(index)
                .map_err(|e| e.to_string())
                .and_then(|v| v.map(decode_time).transpose())
        };
        let n = Node {
            path: decode_path(blob(2)?)?,
            parent,
            children: vec![],
            bytes: number(blob(3)?)?,
            own_bytes: number(blob(4)?)?,
            files: number(blob(5)?)?,
            modified: time(6)?,
            accessed: time(7)?,
            missing_modified: flags & 1 != 0,
            missing_accessed: flags & 2 != 0,
            incomplete: flags & 4 != 0,
        };
        nodes.push(n);
        if let Some(parent) = parent {
            nodes[parent].children.push(id);
        }
    }
    if nodes.len() != expected {
        return Err("Incomplete cached snapshot".into());
    }
    validate(&nodes)?;
    if key(&nodes[0].path)? != stored_key {
        return Err("Cached root path does not match its key".into());
    }
    if p.bytes != nodes[0].bytes || p.files != nodes[0].files {
        return Err("Cached progress totals do not match".into());
    }
    p.folders = nodes.len();
    p.current = nodes[0].path.clone();
    let mut stmt = tx
        .prepare("SELECT text FROM issues WHERE snapshot_id=?1 ORDER BY id")
        .map_err(|e| e.to_string())?;
    let issues = stmt
        .query_map([id], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .collect::<std::result::Result<Vec<String>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(Some(Cached {
        report: Report {
            nodes,
            progress: p,
            issues,
            elapsed,
            cancelled: false,
            scanning: false,
        },
        saved_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "diskmap-cache-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(path.join("data/한글 폴더")).unwrap();
            std::fs::write(path.join("data/root.bin"), [0; 7]).unwrap();
            std::fs::write(path.join("data/한글 폴더/file.bin"), [0; 13]).unwrap();
            Self(path)
        }
        fn db(&self) -> PathBuf {
            self.0.join("snapshots.sqlite3")
        }
        fn report(&self) -> Report {
            crate::scan::scan(&self.0.join("data"), &AtomicBool::new(false), |_| {}).unwrap()
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
                    .starts_with("diskmap-cache-")
            );
            std::fs::remove_dir_all(target).unwrap();
        }
    }

    #[test]
    fn snapshot_roundtrip_preserves_tree_dates_errors_and_full_u64() {
        let f = Fixture::new();
        let mut report = f.report();
        report.nodes[1].modified = Some(SystemTime::UNIX_EPOCH - Duration::new(2, 123));
        report.nodes[1].missing_accessed = true;
        report.nodes[1].incomplete = true;
        report.nodes[0].incomplete = true;
        report.progress.errors = 1;
        report.issues.push("읽기 오류 예시".into());
        // SQLite INTEGER is signed. Byte/count fields must survive without narrowing.
        report.nodes[0].own_bytes = u64::MAX;
        report.nodes[0].bytes = u64::MAX;
        report.progress.bytes = u64::MAX;
        let at = SystemTime::now();
        save(&f.db(), &report, at).unwrap();
        let loaded = load(&f.db(), None).unwrap().unwrap();
        assert_eq!(loaded.saved_at, at);
        assert_eq!(loaded.report.nodes[0].bytes, u64::MAX);
        assert_eq!(loaded.report.nodes[0].children, report.nodes[0].children);
        assert_eq!(loaded.report.nodes[1].modified, report.nodes[1].modified);
        assert_eq!(loaded.report.nodes[1].path, report.nodes[1].path);
        assert!(loaded.report.nodes[1].missing_accessed && loaded.report.nodes[1].incomplete);
        assert_eq!(loaded.report.issues, report.issues);
        assert_eq!(loaded.report.progress.errors, 1);
    }

    #[test]
    fn newest_snapshot_and_explicit_root_selection_are_independent() {
        let f = Fixture::new();
        let first = f.report();
        let second =
            crate::scan::scan(&f.0.join("data/한글 폴더"), &AtomicBool::new(false), |_| {})
                .unwrap();
        let at = SystemTime::now();
        save(&f.db(), &first, at).unwrap();
        save(&f.db(), &second, at + Duration::from_secs(1)).unwrap();
        assert_eq!(
            load(&f.db(), None).unwrap().unwrap().report.nodes[0].path,
            second.nodes[0].path
        );
        assert_eq!(
            load(&f.db(), Some(&first.nodes[0].path))
                .unwrap()
                .unwrap()
                .report
                .nodes[0]
                .bytes,
            20
        );
        assert!(load(&f.db(), Some(&f.0.join("unknown"))).unwrap().is_none());
        // Offline results must not touch the original scanned tree during restore.
        std::fs::rename(f.0.join("data"), f.0.join("offline")).unwrap();
        assert_eq!(
            load(&f.db(), None).unwrap().unwrap().report.nodes[0].bytes,
            13
        );
    }

    #[test]
    fn cancellation_and_failed_transaction_keep_previous_snapshot() {
        let f = Fixture::new();
        let mut report = f.report();
        let at = SystemTime::now();
        save(&f.db(), &report, at).unwrap();
        report.cancelled = true;
        assert!(save(&f.db(), &report, at + Duration::from_secs(1)).is_err());
        report.cancelled = false;
        report.scanning = true;
        assert!(save(&f.db(), &report, at).is_err());
        report.scanning = false;
        let conn = Connection::open(f.db()).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_insert BEFORE INSERT ON nodes BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END;").unwrap();
        assert!(save(&f.db(), &report, at + Duration::from_secs(2)).is_err());
        drop(conn);
        let cached = load(&f.db(), None).unwrap().unwrap();
        assert_eq!(cached.saved_at, at);
        assert_eq!(cached.report.nodes[0].bytes, 20);
    }

    #[test]
    fn corrupted_version_and_hierarchy_are_rejected() {
        let f = Fixture::new();
        save(&f.db(), &f.report(), SystemTime::now()).unwrap();
        let conn = Connection::open(f.db()).unwrap();
        conn.execute_batch("UPDATE nodes SET parent=id WHERE id=1;")
            .unwrap();
        assert!(load(&f.db(), None).is_err());
        conn.execute_batch("PRAGMA user_version=999;").unwrap();
        assert!(load(&f.db(), None).is_err());
        assert!(save(&f.db(), &f.report(), SystemTime::now()).is_err());
        drop(conn);
    }

    #[test]
    fn missing_and_invalid_database_do_not_create_or_overwrite_files() {
        let f = Fixture::new();
        assert!(load(&f.db(), None).unwrap().is_none());
        assert!(!f.db().exists());
        std::fs::write(f.db(), b"not sqlite").unwrap();
        assert!(load(&f.db(), None).is_err());
        assert!(save(&f.db(), &f.report(), SystemTime::now()).is_err());
        assert_eq!(std::fs::read(f.db()).unwrap(), b"not sqlite");
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_encoding_is_lossless_and_drive_keys_ignore_ascii_case() {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        let path = PathBuf::from(OsString::from_wide(&[67, 58, 92, 0xd800, 46, 116]));
        assert_eq!(decode_path(encode_path(&path)).unwrap(), path);
        assert_eq!(
            key(Path::new("C:\\Users\\Test\\")).unwrap(),
            key(Path::new("c:/users/test")).unwrap()
        );
        assert!(decode_path(vec![0]).is_err());
    }

    #[test]
    fn worker_serializes_save_before_load() {
        let f = Fixture::new();
        let worker = start(f.db());
        worker
            .tx
            .send(Request::Save {
                generation: 1,
                report: Arc::new(f.report()),
                saved_at: SystemTime::now(),
            })
            .unwrap();
        worker
            .tx
            .send(Request::Load {
                generation: 2,
                root: None,
            })
            .unwrap();
        assert!(matches!(
            worker.rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            Event::Saved {
                generation: 1,
                result: Ok(_)
            }
        ));
        assert!(matches!(
            worker.rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            Event::Loaded {
                generation: 2,
                result: Ok(Some(_))
            }
        ));
        // All DB connections are dropped before event publication.
        drop(worker);
    }

    #[test]
    #[ignore = "Manual startup snapshot load timing"]
    fn benchmark_restore_50000_directories() {
        let f = Fixture::new();
        let mut report = f.report();
        report.nodes.truncate(1);
        report.nodes[0].children.clear();
        report.nodes[0].own_bytes = 0;
        for i in 1..=50_000 {
            let mut n = Node::new(report.nodes[0].path.join(format!("folder-{i}")), Some(0));
            n.own_bytes = 4096;
            n.bytes = 4096;
            n.files = 1;
            report.nodes.push(n);
            report.nodes[0].children.push(i);
        }
        report.nodes[0].bytes = 50_000 * 4096;
        report.nodes[0].files = 50_000;
        report.progress.bytes = report.nodes[0].bytes;
        report.progress.files = report.nodes[0].files;
        save(&f.db(), &report, SystemTime::now()).unwrap();
        let started = std::time::Instant::now();
        let cached = load(&f.db(), None).unwrap().unwrap();
        println!(
            "Restored {} directories in {:?}; DB {} bytes",
            cached.report.nodes.len(),
            started.elapsed(),
            std::fs::metadata(f.db()).unwrap().len()
        );
        assert_eq!(cached.report.nodes.len(), 50_001);
    }
}
