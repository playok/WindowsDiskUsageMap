//! UI language is independent of scan data and database engine identifiers.
use std::{fs, io, path::Path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    Korean,
    English,
}

impl Language {
    pub fn load() -> Self {
        crate::cache::default_path()
            .and_then(|p| fs::read_to_string(p.with_file_name("language.txt")).ok())
            .map(|s| Self::parse(&s))
            .unwrap_or_default()
    }

    fn parse(value: &str) -> Self {
        match value.trim() {
            "en" => Self::English,
            _ => Self::Korean,
        }
    }

    pub fn save(self) -> io::Result<()> {
        let path = crate::cache::default_path()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Settings directory unavailable")
            })?
            .with_file_name("language.txt");
        self.save_to(&path)
    }

    fn save_to(self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            match self {
                Self::Korean => "ko",
                Self::English => "en",
            },
        )
    }

    // Persisted diagnostics use their original wording. Translate only known
    // message templates at display time; filesystem paths are never translated.
    pub fn message(self, value: &str) -> String {
        if self == Self::Korean {
            return value.to_owned();
        }
        for &(ko, en) in MESSAGES {
            if ko == value {
                return en.to_owned();
            }
        }
        for &(ko, en) in MESSAGES {
            if let Some((prefix, suffix)) = ko.split_once("{detail}")
                && let Some(detail) = value
                    .strip_prefix(prefix)
                    .and_then(|s| s.strip_suffix(suffix))
            {
                return en.replace("{detail}", &self.message(detail));
            }
        }
        value.to_owned()
    }
}

macro_rules! tr {
    ($lang:expr, $ko:literal, $en:literal) => {
        match $lang {
            $crate::i18n::Language::Korean => $ko,
            $crate::i18n::Language::English => $en,
        }
    };
}
pub(crate) use tr;

macro_rules! tr_format {
    ($lang:expr, $ko:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        match $lang {
            $crate::i18n::Language::Korean => format!($ko $(, $arg)*),
            $crate::i18n::Language::English => format!($en $(, $arg)*),
        }
    };
}
pub(crate) use tr_format;

const MESSAGES: &[(&str, &str)] = &[
    ("폴더 열거", "Directory enumeration"),
    ("MFT 고속", "Fast MFT"),
    (
        "저장 DB 위치를 찾을 수 없습니다.",
        "Cannot locate the database directory.",
    ),
    ("스캔할 경로를 입력하세요.", "Enter a path to scan."),
    (
        "스캔 완료 후 결과를 자동 저장합니다.",
        "Results will be saved automatically after scanning.",
    ),
    (
        "재스캔 중: {detail} · 완료 전까지 기존 지도를 유지합니다.",
        "Rescanning: {detail} · Showing the previous map until completion.",
    ),
    (
        "부분 재스캔 작업이 종료되었습니다. 기존 결과를 유지합니다.",
        "The refresh worker stopped. Previous results are retained.",
    ),
    (
        "현재 폴더 재스캔 결과를 반영했습니다.",
        "Folder refresh results applied.",
    ),
    (
        "기존 지도와 DB를 유지했습니다.",
        "The previous map and database were preserved.",
    ),
    (
        "스캔 작업이 예기치 않게 종료되었습니다.",
        "The scan worker stopped unexpectedly.",
    ),
    (
        "저장 결과를 찾을 경로를 입력하세요.",
        "Enter the path of the saved map to open.",
    ),
    ("저장된 지도 불러오는 중…", "Loading saved map…"),
    ("DB 작업이 종료되었습니다.", "The database worker stopped."),
    (
        "중지된 결과는 저장하지 않습니다. 이전 저장 결과가 유지됩니다.",
        "Stopped scans are not saved. The previous saved map is retained.",
    ),
    (
        "스캔 완료 · DB 저장 중…",
        "Scan complete · Saving to database…",
    ),
    (
        "DB 작업이 종료되어 결과를 저장하지 못했습니다.",
        "Results could not be saved because the database worker stopped.",
    ),
    (
        "DB 작업이 종료되었습니다. 저장 여부를 확인하세요.",
        "The database worker stopped. Check whether results were saved.",
    ),
    (
        "저장된 지도를 복원했습니다. ‘현재 폴더 재스캔’ 또는 ‘전체 다시 스캔’으로 갱신하세요.",
        "Saved map restored. Use Rescan current folder or Rescan all to update it.",
    ),
    (
        "저장된 결과가 없습니다. 스캔 완료 후 자동 저장됩니다.",
        "No saved map found. Results are saved automatically after scanning.",
    ),
    (
        "저장된 지도를 읽지 못했습니다: {detail}",
        "Cannot load saved map: {detail}",
    ),
    ("DB 저장 실패: {detail}", "Database save failed: {detail}"),
    ("DB 저장 완료 · {detail}", "Database saved · {detail}"),
    (
        "DB 저장 실패 (스캔 결과는 화면에서 확인 가능): {detail}",
        "Database save failed (scan results remain visible): {detail}",
    ),
    (
        "언어 설정 저장 실패: {detail}",
        "Cannot save language preference: {detail}",
    ),
    (
        "데모 경로는 탐색기로 열 수 없습니다.",
        "Demo paths cannot be opened in Explorer.",
    ),
    (
        "탐색기를 열 수 없습니다: {detail}",
        "Cannot open Explorer: {detail}",
    ),
    (
        "MFT를 사용할 수 없어 폴더 열거로 전환: {detail}",
        "MFT unavailable; falling back to directory enumeration: {detail}",
    ),
    (
        "폴더 경로는 기존 방식으로 스캔합니다. MFT는 로컬 드라이브 루트에서 사용합니다.",
        "Folders use directory enumeration. MFT scanning is available for local drive roots.",
    ),
    (
        "폴더 경로는 기존 방식으로 스캔합니다. MFT는 로컬 드라이브 루트에서 사용합니다. · 읽기 실패한 부분은 이전 집계를 유지했습니다.",
        "Folders use directory enumeration. MFT scanning is available for local drive roots. Previous totals were retained for unreadable items.",
    ),
    (
        "경로를 읽을 수 없습니다: {detail}",
        "Cannot read path: {detail}",
    ),
    (
        "일반 디렉터리 또는 드라이브 루트를 선택하세요 (링크 제외).",
        "Select a regular directory or drive root (not a link).",
    ),
    (
        "부분 재스캔을 중지했습니다. 기존 지도와 저장 결과를 유지합니다.",
        "Refresh stopped. The previous map and saved results are retained.",
    ),
    (
        "재스캔할 완료된 폴더 결과가 없습니다.",
        "No completed folder scan is available to refresh.",
    ),
    (
        "경로가 일반 폴더가 아닙니다. 상위 폴더를 재스캔하세요.",
        "The path is no longer a regular folder. Rescan its parent.",
    ),
    (
        "경로를 확인할 수 없어 기존 결과를 유지합니다: {detail}",
        "Cannot verify the path; previous results retained: {detail}",
    ),
    (
        "상위 경로 확인 실패: {detail}",
        "Cannot verify ancestor path: {detail}",
    ),
    (
        "상위 경로가 링크 또는 다른 형식으로 변경되어 기존 결과를 유지합니다.",
        "An ancestor became a link or another type; previous results retained.",
    ),
    (
        "삭제 경로를 확인할 수 없습니다.",
        "Cannot verify the deleted path.",
    ),
    (
        "경로가 다시 나타났습니다. 기존 결과를 유지합니다. 재스캔을 다시 실행하세요.",
        "The path reappeared. Previous results retained; try refreshing again.",
    ),
    (
        "부분 재스캔 중: {detail} · 완료 후 지도에 적용",
        "Refreshing: {detail} · The map will update after completion",
    ),
    (
        "재스캔 중 읽기 오류가 있어 기존 결과를 유지합니다. {detail}",
        "Refresh did not complete; previous results retained. {detail}",
    ),
    ("스캔 중지", "Scan stopped"),
    (
        "루트 삭제는 부분 갱신으로 처리하지 않습니다.",
        "Root deletion cannot be handled by a partial refresh.",
    ),
    (
        "상위 폴더의 직속 파일 정보 갱신 중…",
        "Refreshing direct-file metadata in ancestor folders…",
    ),
    (
        "부분 갱신: {detail} · 읽기 실패한 항목과 범위 밖 폴더는 이전 집계 유지. 오류·제외 수와 상세는 누적 진단입니다.",
        "Partial refresh: {detail} · Unreadable items and folders outside this scope retain previous totals. Diagnostics are cumulative.",
    ),
    (
        "상위 폴더 형식이 변경되었습니다.",
        "An ancestor folder changed type.",
    ),
    (
        "상위 폴더 읽기 실패: {detail}",
        "Cannot read ancestor folder: {detail}",
    ),
    (
        "상위 항목 읽기 실패: {detail}",
        "Cannot read ancestor entry: {detail}",
    ),
    (
        "재스캔 경로가 일치하지 않습니다.",
        "Refresh path does not match.",
    ),
    (
        "저장 DB 형식이 이 버전과 호환되지 않습니다. DB를 다른 이름으로 옮긴 뒤 다시 스캔하세요.",
        "The saved database format is incompatible. Rename the database and scan again.",
    ),
    (
        "중지되었거나 진행 중인 스캔은 저장하지 않습니다.",
        "Stopped or ongoing scans cannot be saved.",
    ),
    ("NTFS 조회 실패: {detail}", "NTFS query failed: {detail}"),
    ("지원하지 않는 NTFS 버전", "Unsupported NTFS version"),
    (
        "지원하지 않는 NTFS 볼륨 구조",
        "Unsupported NTFS volume layout",
    ),
    (
        "요청한 MFT 확장 레코드가 없거나 변경되었습니다",
        "The requested MFT extension record is missing or changed",
    ),
    ("MFT 레코드 시퀀스 변경 감지", "MFT record sequence changed"),
    (
        "MFT 데이터 구간이 누락되었습니다",
        "Missing MFT data extent",
    ),
    (
        "MFT 디스크 읽기 실패: {detail}",
        "MFT disk read failed: {detail}",
    ),
    (
        "MFT 속성 목록이 지원 범위를 벗어났습니다",
        "MFT attribute list exceeds supported limits",
    ),
    (
        "MFT 확장 레코드가 너무 많습니다",
        "Too many MFT extension records",
    ),
    (
        "MFT 확장 레코드의 부모가 변경되었습니다",
        "MFT extension record parent changed",
    ),
    (
        "MFT 크기가 스캔 준비 중 변경되었습니다",
        "MFT size changed during scan preparation",
    ),
    (
        "로컬 고정/이동식 드라이브만 지원합니다",
        "Only local fixed and removable drives are supported",
    ),
    (
        "볼륨 읽기 권한이 필요합니다. 관리자 권한으로 실행하세요 ({detail})",
        "Volume read permission is required. Run as administrator ({detail})",
    ),
    (
        "NTFS MFT 직접 읽기 · 시스템 메타파일 제외 · 하드 링크는 경로별 집계",
        "Direct NTFS MFT scan · System metafiles excluded · Hard links counted per path",
    ),
    (
        "MFT 레코드 {detail}의 서명을 확인할 수 없습니다",
        "Cannot verify the signature of MFT record {detail}",
    ),
    (
        "스캔 중 MFT 크기 또는 저장 구간이 변경되었습니다",
        "MFT size or storage extents changed during scanning",
    ),
    (
        "MFT: 부모 참조를 확인하지 못한 항목 {detail}개 (스캔 중 변경 또는 아직 읽지 않은 레코드)",
        "MFT: {detail} unresolved parent references (records changed or not yet read)",
    ),
    (
        "MFT: 확장 속성 목록을 완전히 검증하지 못한 레코드 {detail}개. 해당 폴더는 부분 집계입니다.",
        "MFT: {detail} records have unverified extension attributes. Their folders have incomplete totals.",
    ),
    (
        "{detail} (직속 파일은 이전 집계 유지)",
        "{detail} (previous direct-file totals retained)",
    ),
    ("{detail} (삭제 확인)", "{detail} (deletion confirmed)"),
    (
        "{detail} · 읽기 실패한 부분은 이전 집계를 유지했습니다.",
        "{detail} · Previous totals retained for unreadable items.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_preference_roundtrips_and_invalid_value_defaults_to_korean() {
        let path = std::env::temp_dir().join(format!(
            "diskmap-language-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for language in [Language::English, Language::Korean] {
            language.save_to(&path).unwrap();
            assert_eq!(
                Language::parse(&fs::read_to_string(&path).unwrap()),
                language
            );
        }
        fs::remove_file(&path).unwrap();
        assert_eq!(Language::parse("invalid"), Language::Korean);
    }

    #[test]
    fn saved_diagnostics_switch_language_without_changing_paths() {
        let path = r"D:\자료\스캔 완료\한글.txt";
        let message = format!("부분 재스캔 중: {path} · 완료 후 지도에 적용");
        assert_eq!(Language::Korean.message(&message), message);
        assert_eq!(
            Language::English.message(&message),
            format!("Refreshing: {path} · The map will update after completion")
        );
        assert_eq!(Language::English.message(path), path);
        assert_eq!(
            Language::English
                .message("MFT를 사용할 수 없어 폴더 열거로 전환: 지원하지 않는 NTFS 버전"),
            "MFT unavailable; falling back to directory enumeration: Unsupported NTFS version"
        );
    }
}
