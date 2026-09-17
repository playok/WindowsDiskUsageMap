use crate::i18n::{Language, tr, tr_format};
use crate::{
    cache,
    scan::{self, Event, Node, Progress, Report},
    subtree, treemap,
};
use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Stroke, StrokeKind, Vec2};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, TryRecvError},
    },
    time::{Duration, SystemTime},
};

pub struct DiskApp {
    language: Language,
    path: String,
    drives: Vec<String>,
    receiver: Option<Receiver<Event>>,
    refresh_receiver: Option<Receiver<subtree::Event>>,
    cancel: Option<Arc<AtomicBool>>,
    progress: Progress,
    report: Option<Arc<Report>>,
    cache: Option<cache::Worker>,
    cache_generation: u64,
    cache_loading: bool,
    cache_saves: usize,
    cache_status: String,
    cached_at: Option<SystemTime>,
    closing: bool,
    error: Option<String>,
    focus: usize,
    selected_path: Option<PathBuf>,
    access: bool,
    days: u64,
    min_mb: u64,
    depth: usize,
    query: String,
    candidates: Vec<usize>,
    candidate_count: usize,
    children: Vec<usize>,
    now: SystemTime,
}

impl DiskApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = Vec2::new(10.0, 9.0);
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        cc.egui_ctx.set_style(style);
        // Windows' Korean system font, with egui's embedded fonts as fallback.
        if let Some(font) = std::env::var_os("WINDIR")
            .and_then(|p| std::fs::read(PathBuf::from(p).join("Fonts/malgun.ttf")).ok())
        {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("korean".into(), egui::FontData::from_owned(font).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "korean".into());
            cc.egui_ctx.set_fonts(fonts);
        }
        let drives = scan::drives();
        let explicit_path = std::env::args().nth(1).filter(|p| p != "--demo");
        let path = explicit_path
            .clone()
            .unwrap_or_else(|| drives.first().cloned().unwrap_or_default());
        let mut app = Self {
            language: Language::load(),
            path,
            drives,
            receiver: None,
            refresh_receiver: None,
            cancel: None,
            progress: Progress::default(),
            report: None,
            cache: None,
            cache_generation: 0,
            cache_loading: false,
            cache_saves: 0,
            cache_status: String::new(),
            cached_at: None,
            closing: false,
            error: None,
            focus: 0,
            selected_path: None,
            access: false,
            days: 180,
            min_mb: 1024,
            depth: 2,
            query: String::new(),
            candidates: vec![],
            candidate_count: 0,
            children: vec![],
            now: SystemTime::now(),
        };
        if std::env::args().any(|arg| arg == "--demo") {
            app.report = Some(Arc::new(demo_report()));
            app.navigate(0);
            app.refresh_candidates();
        } else if let Some(db) = cache::default_path() {
            app.cache = Some(cache::start(db));
            app.load_cache(explicit_path.map(PathBuf::from));
        } else {
            app.cache_status = "저장 DB 위치를 찾을 수 없습니다.".into();
        }
        app
    }

    fn begin_scan(&mut self) {
        if self.busy() {
            return;
        }
        let path = PathBuf::from(self.path.trim());
        if path.as_os_str().is_empty() {
            self.error = Some("스캔할 경로를 입력하세요.".into());
            return;
        }
        let path = match std::path::absolute(path) {
            Ok(path) => path,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        self.cache_generation += 1;
        self.cache_loading = false;
        self.cached_at = None;
        if self.cache.is_some() {
            self.cache_status = "스캔 완료 후 결과를 자동 저장합니다.".into();
        }
        let (rx, cancel) = scan::start(path);
        self.receiver = Some(rx);
        self.cancel = Some(cancel);
        self.progress = Progress::default();
        self.report = None;
        self.candidates.clear();
        self.candidate_count = 0;
        self.children.clear();
        self.error = None;
        self.focus = 0;
        self.selected_path = None;
    }

    fn busy(&self) -> bool {
        self.receiver.is_some() || self.refresh_receiver.is_some()
    }

    fn begin_subtree_scan(&mut self, target: usize) {
        if self.busy() {
            return;
        }
        let Some(base) = &self.report else {
            return;
        };
        if base.scanning || base.elapsed == Duration::ZERO || target >= base.nodes.len() {
            return;
        }
        self.cache_generation += 1;
        self.cache_loading = false;
        self.cache_status = format!(
            "재스캔 중: {} · 완료 전까지 기존 지도를 유지합니다.",
            base.nodes[target].path.display()
        );
        self.error = None;
        let (rx, cancel) = subtree::start(Arc::clone(base), target);
        self.refresh_receiver = Some(rx);
        self.cancel = Some(cancel);
    }

    fn poll_subtree(&mut self) {
        loop {
            let event = match self.refresh_receiver.as_ref().map(|rx| rx.try_recv()) {
                Some(Ok(event)) => event,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.finish_subtree(Err(
                        "부분 재스캔 작업이 종료되었습니다. 기존 결과를 유지합니다.".into(),
                    ));
                    break;
                }
                _ => break,
            };
            match event {
                subtree::Event::Progress(p) => self.progress = p,
                subtree::Event::Finished(result) => {
                    self.finish_subtree(result);
                    break;
                }
            }
        }
    }

    fn finish_subtree(&mut self, result: Result<Report, String>) {
        self.refresh_receiver = None;
        self.cancel = None;
        match result {
            Ok(report) => {
                self.progress = report.progress.clone();
                self.replace_report(report);
                self.cached_at = None;
                self.now = SystemTime::now();
                self.refresh_candidates();
                self.cache_status = "현재 폴더 재스캔 결과를 반영했습니다.".into();
                self.save_cache();
            }
            Err(message) => {
                if let Some(report) = &self.report {
                    self.progress = report.progress.clone();
                }
                self.error = Some(message);
                self.cache_status = "기존 지도와 DB를 유지했습니다.".into();
            }
        }
    }

    fn poll(&mut self) {
        loop {
            let event = match self.receiver.as_ref().map(|r| r.try_recv()) {
                Some(Ok(event)) => event,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.error = Some("스캔 작업이 예기치 않게 종료되었습니다.".into());
                    self.receiver = None;
                    self.cancel = None;
                    break;
                }
                _ => break,
            };
            match event {
                Event::Restart(p) => {
                    self.progress = p;
                    self.report = None;
                    self.focus = 0;
                    self.selected_path = None;
                    self.children.clear();
                    self.candidates.clear();
                    self.candidate_count = 0;
                }
                Event::Progress(p) => self.progress = p,
                Event::Preview(r) => {
                    self.progress = r.progress.clone();
                    self.replace_report(r);
                }
                Event::Finished(r) => {
                    self.progress = r.progress.clone();
                    self.replace_report(r);
                    self.receiver = None;
                    self.cancel = None;
                    self.now = SystemTime::now();
                    self.navigate(self.focus);
                    self.refresh_candidates();
                    self.save_cache();
                    break;
                }
                Event::Failed(error) => {
                    self.error = Some(error);
                    self.receiver = None;
                    self.cancel = None;
                    break;
                }
            }
        }
    }

    fn navigate(&mut self, id: usize) {
        self.focus = id;
        self.selected_path = None;
        if let Some(r) = &self.report {
            self.children = r.nodes[id].children.clone();
            self.children
                .sort_unstable_by_key(|&i| std::cmp::Reverse(r.nodes[i].bytes));
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let path = self.selected_path.as_ref()?;
        self.report
            .as_ref()?
            .nodes
            .iter()
            .position(|n| &n.path == path)
    }

    fn replace_report(&mut self, report: Report) {
        let selection = self
            .selected_path
            .clone()
            .filter(|p| report.nodes.iter().any(|n| &n.path == p));
        let previous = self.report.as_ref().map(|r| &r.nodes[self.focus].path);
        let focus = previous
            .and_then(|path| {
                path.ancestors()
                    .find_map(|path| report.nodes.iter().position(|n| n.path == path))
            })
            .unwrap_or(0);
        self.report = Some(Arc::new(report));
        self.navigate(focus);
        self.selected_path = selection;
    }

    fn load_cache(&mut self, root: Option<PathBuf>) {
        if root.as_ref().is_some_and(|p| p.as_os_str().is_empty()) {
            self.cache_status = "저장 결과를 찾을 경로를 입력하세요.".into();
            return;
        }
        let Some(worker) = &self.cache else {
            return;
        };
        self.cache_generation += 1;
        if worker
            .tx
            .send(cache::Request::Load {
                generation: self.cache_generation,
                root,
            })
            .is_ok()
        {
            self.cache_loading = true;
            self.cache_status = "저장된 지도 불러오는 중…".into();
        } else {
            self.cache_status = "DB 작업이 종료되었습니다.".into();
        }
    }

    fn save_cache(&mut self) {
        let (Some(worker), Some(report)) = (&self.cache, &self.report) else {
            return;
        };
        if report.cancelled || report.scanning {
            self.cache_status =
                "중지된 결과는 저장하지 않습니다. 이전 저장 결과가 유지됩니다.".into();
            return;
        }
        if worker
            .tx
            .send(cache::Request::Save {
                generation: self.cache_generation,
                report: Arc::clone(report),
                saved_at: SystemTime::now(),
            })
            .is_ok()
        {
            self.cache_saves += 1;
            self.cache_status = "스캔 완료 · DB 저장 중…".into();
        } else {
            self.cache_status = "DB 작업이 종료되어 결과를 저장하지 못했습니다.".into();
        }
    }

    fn poll_cache(&mut self) {
        loop {
            let event = match self.cache.as_ref().map(|worker| worker.rx.try_recv()) {
                Some(Ok(event)) => event,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.cache = None;
                    self.cache_loading = false;
                    self.cache_saves = 0;
                    self.closing = false;
                    self.cache_status = "DB 작업이 종료되었습니다. 저장 여부를 확인하세요.".into();
                    break;
                }
                _ => break,
            };
            match event {
                cache::Event::Loaded { generation, result } => {
                    // A late startup load must never replace a new scan or another load.
                    if generation != self.cache_generation {
                        continue;
                    }
                    self.cache_loading = false;
                    match result {
                        Ok(Some(cached)) => {
                            self.path = cached.report.nodes[0].path.to_string_lossy().into_owned();
                            self.progress = cached.report.progress.clone();
                            self.replace_report(cached.report);
                            self.cached_at = Some(cached.saved_at);
                            self.now = SystemTime::now();
                            self.refresh_candidates();
                            self.cache_status = "저장된 지도를 복원했습니다. ‘현재 폴더 재스캔’ 또는 ‘전체 다시 스캔’으로 갱신하세요.".into();
                        }
                        Ok(None) => {
                            self.cache_status =
                                "저장된 결과가 없습니다. 스캔 완료 후 자동 저장됩니다.".into()
                        }
                        Err(e) => self.cache_status = format!("저장된 지도를 읽지 못했습니다: {e}"),
                    }
                }
                cache::Event::Saved { generation, result } => {
                    self.cache_saves = self.cache_saves.saturating_sub(1);
                    if let Err(error) = &result {
                        self.closing = false;
                        self.error = Some(format!("DB 저장 실패: {error}"));
                    }
                    if generation == self.cache_generation {
                        self.cache_status = match result {
                            Ok(at) => {
                                format!("DB 저장 완료 · {}", date_label(Language::Korean, at))
                            }
                            Err(e) => format!("DB 저장 실패 (스캔 결과는 화면에서 확인 가능): {e}"),
                        };
                    }
                }
            }
        }
    }

    fn refresh_candidates(&mut self) {
        self.candidates.clear();
        self.candidate_count = 0;
        let Some(r) = &self.report else {
            return;
        };
        if r.scanning {
            return;
        }
        let q = self.query.to_lowercase();
        self.candidates = r
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                !n.incomplete
                    && n.bytes >= self.min_mb.saturating_mul(1024 * 1024)
                    && n.age_days(self.access, self.now)
                        .is_some_and(|d| d >= self.days)
                    && (q.is_empty() || n.path.to_string_lossy().to_lowercase().contains(&q))
            })
            .map(|(id, _)| id)
            .collect();
        self.candidates
            .sort_unstable_by_key(|&id| std::cmp::Reverse(r.nodes[id].bytes));
        self.candidate_count = self.candidates.len();
        self.candidates.truncate(200);
    }

    fn top(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Language / 언어");
                let previous = self.language;
                egui::ComboBox::from_id_salt("language")
                    .selected_text(match self.language {
                        Language::Korean => "한국어",
                        Language::English => "English",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.language, Language::Korean, "한국어");
                        ui.selectable_value(&mut self.language, Language::English, "English");
                    });
                if previous != self.language {
                    if let Err(e) = self.language.save() {
                        self.error = Some(format!("언어 설정 저장 실패: {e}"));
                    }
                    ctx.request_repaint();
                }
            });
            let lang = self.language;
            ui.horizontal(|ui| {
                ui.heading("DISK / MAP");
                ui.label(egui::RichText::new(tr!(lang, "공간을 차지하는 오래된 폴더 찾기", "Find old folders taking up disk space")).color(Color32::from_rgb(150, 170, 190)));
            });
            ui.horizontal(|ui| {
                let busy = self.busy();
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("drive").selected_text(tr!(lang, "드라이브", "Drive")).show_ui(ui, |ui| {
                        for drive in &self.drives { ui.selectable_value(&mut self.path, drive.clone(), drive); }
                    });
                    ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(420.0).hint_text(tr!(lang, "C:\\ 또는 폴더 경로", "C:\\ or folder path")));
                    if ui.button(if self.report.is_some() { tr!(lang, "전체 다시 스캔", "Rescan all") } else { tr!(lang, "스캔 시작", "Start scan") }).clicked() { self.begin_scan(); }
                    if ui.add_enabled(self.cache.is_some() && !self.cache_loading, egui::Button::new(tr!(lang, "저장 결과 열기", "Open saved map"))).clicked() { self.load_cache(Some(PathBuf::from(self.path.trim()))); }
                    if ui.small_button(tr!(lang, "드라이브 새로고침", "Refresh drives")).clicked() { self.drives = scan::drives(); }
                });
                if busy {
                    let requested = self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
                    if ui.add_enabled(!requested, egui::Button::new(if requested { tr!(lang, "중지 중…", "Stopping…") } else { tr!(lang, "중지", "Stop") })).clicked()
                        && let Some(cancel) = &self.cancel { cancel.store(true, Ordering::Relaxed); }
                    ui.spinner();
                }
            });
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label(tr!(lang, "색상 / 오래됨 기준", "Color / age based on"));
                changed |= ui.selectable_value(&mut self.access, false, tr!(lang, "마지막 수정", "Last modified")).changed();
                changed |= ui.selectable_value(&mut self.access, true, tr!(lang, "마지막 접근 (참고)", "Last accessed (estimate)")).changed();
                ui.separator();
                ui.label(tr!(lang, "계층 깊이", "Depth"));
                ui.add(egui::Slider::new(&mut self.depth, 1..=4));
                ui.separator();
                for (label, color) in [(tr!(lang, "최근", "Recent"), age_color(Some(0))), (tr!(lang, "6개월", "6 months"), age_color(Some(180))), (tr!(lang, "1년+", "1 year+"), age_color(Some(365))), (tr!(lang, "미확인", "Unknown"), age_color(None))] {
                    ui.colored_label(color, format!("■ {label}"));
                }
            });
            if changed { self.refresh_candidates(); }
            ui.label(egui::RichText::new(tr!(lang, "면적 = 파일 크기 · 색상 = 가장 최근 날짜 · 한 번 클릭: 선택 / 두 번 클릭: 폴더 확대", "Area = file size · Color = latest date · Click to select / double-click to open")).small().weak());
            if self.access {
                ui.colored_label(Color32::from_rgb(235, 190, 105), tr!(lang, "접근 시간은 Windows 설정에 따라 갱신되지 않거나 지연될 수 있습니다. 실제 사용 빈도를 뜻하지 않습니다.", "Windows may disable or delay access-time updates. These dates do not measure actual usage frequency."));
            }
        });
    }

    fn bottom(&self, ctx: &egui::Context) {
        let lang = self.language;
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            if self.cache_loading || self.cache_saves > 0 {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(if self.closing {
                        tr!(
                            lang,
                            "DB 저장을 마친 뒤 종료합니다…",
                            "Closing after the database save completes…"
                        )
                        .to_owned()
                    } else {
                        lang.message(&self.cache_status)
                    });
                });
            } else {
                ui.small(lang.message(&self.cache_status));
            }
            if let Some(worker) = &self.cache {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("DB: {}", worker.path.display()))
                            .small()
                            .weak(),
                    )
                    .truncate(),
                )
                .on_hover_text(worker.path.to_string_lossy());
            }
            ui.horizontal_wrapped(|ui| {
                ui.strong(lang.message(self.progress.engine));
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(lang.message(&self.progress.note)).small(),
                    )
                    .truncate(),
                )
                .on_hover_text(lang.message(&self.progress.note));
            });
            if self.receiver.is_some() && self.progress.total_records > 0 {
                ui.add(
                    egui::ProgressBar::new(
                        self.progress.records as f32 / self.progress.total_records as f32,
                    )
                    .text(tr_format!(
                        lang,
                        "MFT 레코드 {} / {}",
                        "MFT records {} / {}",
                        self.progress.records,
                        self.progress.total_records
                    )),
                );
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(tr_format!(
                    lang,
                    "{} 폴더  /  {} 파일  /  {}",
                    "{} folders  /  {} files  /  {}",
                    self.progress.folders,
                    self.progress.files,
                    size(self.progress.bytes)
                ));
                ui.separator();
                ui.label(tr_format!(
                    lang,
                    "읽기 오류 {} · 링크/재분석 지점 제외 {}",
                    "Read errors {} · Links/reparse points skipped {}",
                    self.progress.errors,
                    self.progress.skipped
                ));
                if let Some(r) = &self.report {
                    ui.label(tr_format!(
                        lang,
                        "{:.1}초 · {}",
                        "{:.1}s · {}",
                        r.elapsed.as_secs_f64(),
                        if self.refresh_receiver.is_some() {
                            tr!(
                                lang,
                                "현재 폴더 재스캔 중 · 지도는 이전 결과",
                                "Refreshing folder · Showing previous map"
                            )
                        } else if self.cached_at.is_some() {
                            tr!(lang, "저장된 결과", "Saved map")
                        } else if r.scanning {
                            tr!(lang, "스캔 중 · 잠정 지도", "Scanning · Preview map")
                        } else if r.cancelled {
                            tr!(lang, "중지된 부분 결과", "Stopped · Partial results")
                        } else {
                            tr!(lang, "스캔 완료", "Scan complete")
                        }
                    ));
                }
            });
            if self.busy() {
                ui.add(egui::Label::new(self.progress.current.to_string_lossy()).truncate());
            }
            if let Some(error) = &self.error {
                ui.colored_label(Color32::LIGHT_RED, lang.message(error));
            }
        });
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        let lang = self.language;
        egui::SidePanel::right("candidates").default_width(330.0).min_width(270.0).max_width(500.0).show(ctx, |ui| {
            ui.heading(tr!(lang, "오래된 대용량 폴더", "Large old folders"));
            if self.busy() { ui.weak(tr!(lang, "후보 목록은 스캔 완료 후 갱신됩니다.", "Candidates update after the scan completes.")); }
            ui.label(tr!(lang, "전체 스캔 범위에서 크기순으로 표시", "Sorted by size across the entire scanned tree"));
            let mut changed = false;
            ui.horizontal(|ui| {
                changed |= ui.add(egui::DragValue::new(&mut self.days).range(0..=36500).suffix(tr!(lang, "일 이상", " days or older"))).changed();
                changed |= ui.add(egui::DragValue::new(&mut self.min_mb).range(0..=100_000_000).suffix(tr!(lang, " MiB 이상", " MiB or larger"))).changed();
            });
            changed |= ui.add(egui::TextEdit::singleline(&mut self.query).hint_text(tr!(lang, "경로 검색", "Filter paths")).desired_width(f32::INFINITY)).changed();
            if changed { self.refresh_candidates(); }
            ui.label(egui::RichText::new(tr!(lang, "읽기 누락·날짜 미확인 폴더는 제외합니다. 상위/하위 폴더의 크기는 서로 중복됩니다.", "Incomplete folders and unknown dates are excluded. Parent and child sizes overlap.")).small().weak());
            ui.separator();
            ui.label(tr_format!(lang, "{}개 일치 · 최대 200개 표시", "{} matches · Showing up to 200", self.candidate_count));
            let mut selected = None;
            egui::ScrollArea::vertical().id_salt("candidate_scroll").show(ui, |ui| {
                if let Some(r) = &self.report {
                    for &id in &self.candidates {
                        let n = &r.nodes[id];
                        let age = n.age_days(self.access, self.now);
                        ui.push_id(id, |ui| {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.colored_label(age_color(age), size(n.bytes));
                                    ui.weak(age_text(lang, age));
                                });
                                if ui.selectable_label(self.focus == id, n.name()).clicked() { selected = Some(id); }
                                ui.add(egui::Label::new(egui::RichText::new(n.path.to_string_lossy()).small().weak()).truncate())
                                    .on_hover_text(n.path.to_string_lossy());
                            });
                        });
                    }
                    if self.candidates.is_empty() { ui.label(tr!(lang, "조건에 맞는 폴더가 없습니다. 기간이나 최소 크기를 조정해 보세요.", "No matching folders. Adjust the age or minimum size.")); }
                }
            });
            if let Some(id) = selected { self.navigate(id); }
        });
    }

    fn main_panel(&mut self, ctx: &egui::Context) {
        let lang = self.language;
        egui::CentralPanel::default().show(ctx, |ui| {
            let busy = self.busy();
            let selected_index = self.selected_index();
            let Some(report) = &self.report else {
                ui.add_space(90.0);
                ui.vertical_centered(|ui| {
                    ui.heading(if self.receiver.is_some() { tr!(lang, "드라이브를 살펴보고 있습니다", "Scanning your drive") } else { tr!(lang, "어디에 공간을 쓰고 있나요?", "Where is your disk space going?") });
                    ui.add_space(15.0);
                    ui.label(tr!(lang, "드라이브 또는 폴더 경로를 선택하고 ‘스캔 시작’을 누르세요.", "Choose a drive or folder path and click Start scan."));
                    ui.label(tr!(lang, "폴더 면적은 크기, 색상은 오래된 정도를 나타냅니다.", "Folder area represents size; color represents age."));
                    ui.label(tr!(lang, "스캔 중에도 중지할 수 있으며, 중지 시 읽은 범위의 결과를 보여줍니다.", "Stop a scan at any time to view the results collected so far."));
                    ui.add_space(20.0);
                    ui.weak(tr!(lang, "파일 내용은 열지 않으며 폴더별 메타데이터만 수집합니다.", "Only size and metadata are analyzed, not file contents."));
                });
                return;
            };
            let nodes = &report.nodes;
            let current = &nodes[self.focus];
            if let Some(at) = self.cached_at {
                ui.colored_label(Color32::LIGHT_YELLOW, tr_format!(lang, "저장된 결과 · 저장 {} · 현재 상태는 재스캔으로 확인하세요", "Saved map · Saved {} · Rescan to check the current state", date_label(lang, at)));
            }
            let mut navigate = None;
            let mut refresh = None;
            let mut selection = None;
            let can_refresh = !busy && !report.scanning && report.elapsed != Duration::ZERO;
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(current.parent.is_some(), egui::Button::new(tr!(lang, "← 상위", "← Up"))).clicked() { navigate = current.parent; }
                if ui.button(tr!(lang, "루트", "Root")).clicked() { navigate = Some(0); }
                if ui.button(tr!(lang, "경로 복사", "Copy path")).clicked() { ctx.copy_text(current.path.to_string_lossy().into_owned()); }
                if ui.button(tr!(lang, "탐색기로 열기", "Open in Explorer")).clicked() {
                    if report.elapsed == Duration::ZERO { self.error = Some("데모 경로는 탐색기로 열 수 없습니다.".into()); }
                    else if let Err(e) = open_folder(&current.path) { self.error = Some(format!("탐색기를 열 수 없습니다: {e}")); }
                }
                if ui.add_enabled(can_refresh, egui::Button::new(tr!(lang, "현재 폴더 재스캔", "Rescan current folder")))
                    .on_hover_text(tr!(lang, "이 폴더와 모든 하위 폴더를 다시 읽어 삭제·추가·크기 변경을 반영합니다. 읽기 오류는 건너뛰고 해당 부분의 이전 집계를 유지합니다. 중지 시 기존 지도를 유지합니다.", "Rescan this folder and its descendants for additions, deletions and size changes. Read failures retain previous data; other folders continue. Stopping preserves the existing map.")).clicked() { refresh = Some(self.focus); }
                if ui.add_enabled(can_refresh && selected_index.is_some(), egui::Button::new(tr!(lang, "선택 항목 새로 고침", "Refresh selected"))).clicked() { refresh = selected_index; }
            });
            if let Some(id) = selected_index { ui.label(tr_format!(lang, "선택: {}", "Selected: {}", nodes[id].path.display())); }
            if self.refresh_receiver.is_some() { ui.colored_label(Color32::LIGHT_YELLOW, tr!(lang, "현재 폴더 재스캔 중입니다. 완료되면 기존 지도에 반영합니다.", "Refreshing the folder. The map will update when complete.")); }
            ui.add(egui::Label::new(egui::RichText::new(current.path.to_string_lossy()).size(19.0)).truncate()).on_hover_text(current.path.to_string_lossy());
            ui.horizontal_wrapped(|ui| {
                ui.heading(size(current.bytes));
                ui.label(tr_format!(lang, "{} 파일 · {} 하위 폴더 · {}", "{} files · {} subfolders · {}", current.files, current.children.len(), age_text(lang, current.age_days(self.access, self.now))));
                if report.scanning { ui.colored_label(Color32::LIGHT_YELLOW, tr!(lang, "스캔 중 · 연결·집계된 용량만 표시 (크기·색상 변경 가능)", "Scanning · Showing linked and counted data only (sizes and colors may change)")); }
                else if current.incomplete { ui.colored_label(Color32::LIGHT_YELLOW, tr!(lang, "일부 항목 미집계", "Incomplete totals")); }
                if report.elapsed == Duration::ZERO { ui.colored_label(Color32::LIGHT_BLUE, tr!(lang, "데모 데이터", "Demo data")); }
            });
            if !report.issues.is_empty() {
                egui::CollapsingHeader::new(tr!(lang, "읽기 오류 상세 (최대 100건)", "Read error details (up to 100)")).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(110.0).show(ui, |ui| { for issue in &report.issues { ui.label(lang.message(issue)); } });
                });
            }
            let height = (ui.available_height() * 0.62).max(120.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
            ui.painter().rect_filled(rect, 6.0, Color32::from_rgb(16, 22, 32));
            if current.bytes == 0 {
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, tr!(lang, "표시할 파일 크기가 없습니다", "No file sizes to display"), FontId::proportional(18.0), Color32::GRAY);
            } else {
                let mut budget = 1500;
                let mut renderer = MapRenderer { language: lang, nodes, access: self.access, now: self.now, navigate: &mut navigate, refresh: &mut refresh, selection: &mut selection, active_selection: selected_index, can_refresh, budget: &mut budget };
                renderer.draw(ui, self.focus, rect.shrink(4.0), self.depth);
            }
            ui.label(egui::RichText::new(tr!(lang, "[직속 파일]은 현재 폴더의 파일 합계입니다. 작은 항목은 [기타]로 묶이며 아래 목록에서 열 수 있습니다.", "[Direct files] combines files in this folder. Small folders are grouped under [Other folders]; open them from the list below.")).small().weak());
            ui.separator();
            ui.label(tr!(lang, "하위 폴더 · 한 번 클릭: 선택 / 두 번 클릭: 열기 / 우클릭: 새로 고침", "Subfolders · Click: select / Double-click: open / Right-click: refresh"));
            egui::ScrollArea::vertical().id_salt("children").auto_shrink([false, false]).show_rows(ui, 28.0, self.children.len(), |ui, range| {
                for row in range {
                    let id = self.children[row];
                    let n = &nodes[id];
                    ui.horizontal(|ui| {
                        ui.colored_label(age_color(n.age_days(self.access, self.now)), "■");
                        ui.label(format!("{:>11}", size(n.bytes)));
                        let response = ui.selectable_label(selected_index == Some(id), n.name());
                        if response.clicked() || response.secondary_clicked() { selection = Some(id); }
                        if response.double_clicked() { navigate = Some(id); }
                        response.context_menu(|ui| {
                            if ui.add_enabled(can_refresh, egui::Button::new(tr!(lang, "선택 항목 새로 고침", "Refresh selected"))).clicked() { selection = Some(id); refresh = Some(id); ui.close(); }
                            if ui.button(tr!(lang, "열기", "Open")).clicked() { navigate = Some(id); ui.close(); }
                        });
                        if ui.small_button(tr!(lang, "열기", "Open")).clicked() { navigate = Some(id); }
                        ui.weak(age_text(lang, n.age_days(self.access, self.now)));
                        if n.incomplete { ui.weak(tr!(lang, "일부 누락", "Incomplete")); }
                    });
                }
            });
            let selected_path = selection.map(|id| nodes[id].path.clone());
            if let Some(id) = navigate { self.navigate(id); }
            else if let Some(path) = selected_path { self.selected_path = Some(path); }
            if let Some(id) = refresh { self.begin_subtree_scan(id); }
        });
    }
}

impl eframe::App for DiskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_cache();
        self.poll();
        self.poll_subtree();
        self.top(ctx);
        self.bottom(ctx);
        self.sidebar(ctx);
        self.main_panel(ctx);
        if self.busy() || self.cache_loading || self.cache_saves > 0 {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.cache_saves > 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
        }
        if self.closing && self.cache_saves == 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl Drop for DiskApp {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

struct MapRenderer<'a> {
    language: Language,
    nodes: &'a [Node],
    access: bool,
    now: SystemTime,
    navigate: &'a mut Option<usize>,
    refresh: &'a mut Option<usize>,
    selection: &'a mut Option<usize>,
    active_selection: Option<usize>,
    can_refresh: bool,
    budget: &'a mut usize,
}

impl MapRenderer<'_> {
    fn draw(&mut self, ui: &mut egui::Ui, parent: usize, bounds: Rect, depth: usize) {
        let lang = self.language;
        let node = &self.nodes[parent];
        let mut items: Vec<(Option<usize>, u64, &str)> = node
            .children
            .iter()
            .filter(|&&id| self.nodes[id].bytes > 0)
            .map(|&id| (Some(id), self.nodes[id].bytes, ""))
            .collect();
        items.sort_unstable_by_key(|item| std::cmp::Reverse(item.1));
        if items.len() > 100 {
            let others = items
                .drain(100..)
                .fold(0u64, |sum, item| sum.saturating_add(item.1));
            items.push((None, others, tr!(lang, "[기타 폴더]", "[Other folders]")));
        }
        if node.own_bytes > 0 {
            items.push((
                None,
                node.own_bytes,
                tr!(lang, "[직속 파일]", "[Direct files]"),
            ));
        }
        items.sort_unstable_by_key(|item| std::cmp::Reverse(item.1));
        let weights: Vec<_> = items.iter().map(|x| x.1).collect();
        let rects = treemap::layout(&weights, bounds);
        for ((id, bytes, label), rect) in items.into_iter().zip(rects) {
            if rect.width() < 2.0 || rect.height() < 2.0 {
                continue;
            }
            let inner = rect.shrink(1.5);
            let age = id.and_then(|id| self.nodes[id].age_days(self.access, self.now));
            let color = if id.is_none() {
                Color32::from_rgb(62, 73, 91)
            } else {
                age_color(age)
            };
            ui.painter()
                .rect_filled(inner, 3.0, color.gamma_multiply(0.65));
            let name = id
                .map(|i| self.nodes[i].name())
                .unwrap_or_else(|| label.into());
            let response = ui.interact(
                inner,
                ui.id().with((parent, id, label)),
                if id.is_some() {
                    Sense::click()
                } else {
                    Sense::hover()
                },
            );
            if response.clicked() || response.secondary_clicked() {
                *self.selection = id;
            }
            if response.double_clicked() {
                *self.navigate = id;
            }
            if let Some(id) = id {
                response.context_menu(|ui| {
                    if ui.button(tr!(lang, "열기", "Open")).clicked() {
                        *self.navigate = Some(id);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.can_refresh,
                            egui::Button::new(tr!(lang, "선택 항목 새로 고침", "Refresh selected")),
                        )
                        .clicked()
                    {
                        *self.selection = Some(id);
                        *self.refresh = Some(id);
                        ui.close();
                    }
                });
            }
            if response.hovered() || (id.is_some() && id == self.active_selection) {
                ui.painter().rect_stroke(
                    inner,
                    3.0,
                    Stroke::new(
                        2.0_f32,
                        if id == self.active_selection {
                            Color32::LIGHT_BLUE
                        } else {
                            Color32::WHITE
                        },
                    ),
                    StrokeKind::Inside,
                );
            }
            response.on_hover_ui(|ui| {
                ui.strong(&name);
                ui.label(size(bytes));
                if let Some(id) = id {
                    let n = &self.nodes[id];
                    ui.label(n.path.to_string_lossy());
                    ui.label(age_text(lang, age));
                    if n.incomplete {
                        ui.label(tr!(lang, "읽지 못했거나 제외된 항목이 있어 부분 집계입니다.", "Totals are incomplete because some items could not be read or were excluded."));
                    }
                    ui.label(tr!(lang, "한 번 클릭: 선택 · 두 번 클릭: 폴더 확대 · 우클릭: 새로 고침", "Click: select · Double-click: open folder · Right-click: refresh"));
                } else {
                    ui.label(tr!(lang, "집계 블록 · 개별 폴더는 아래 목록에서 탐색하세요.", "Grouped tile · Browse individual folders in the list below."));
                }
            });
            if inner.width() > 48.0 && inner.height() > 22.0 {
                let painter = ui.painter().with_clip_rect(inner.shrink(3.0));
                painter.text(
                    inner.min + Vec2::new(6.0, 4.0),
                    Align2::LEFT_TOP,
                    &name,
                    FontId::proportional(13.0),
                    Color32::WHITE,
                );
                if let Some(id) = id
                    && depth > 1
                    && inner.height() > 72.0
                    && inner.width() > 90.0
                    && !self.nodes[id].children.is_empty()
                    && *self.budget > 0
                {
                    *self.budget = self.budget.saturating_sub(1);
                    let child_bounds = Rect::from_min_max(
                        inner.min + Vec2::new(4.0, 25.0),
                        inner.max - Vec2::splat(4.0),
                    );
                    self.draw(ui, id, child_bounds, depth - 1);
                } else if inner.height() > 44.0 {
                    painter.text(
                        inner.min + Vec2::new(6.0, 25.0),
                        Align2::LEFT_TOP,
                        size(bytes),
                        FontId::proportional(12.0),
                        Color32::from_gray(225),
                    );
                }
            }
        }
    }
}

fn age_color(days: Option<u64>) -> Color32 {
    let Some(days) = days else {
        return Color32::from_rgb(115, 129, 150);
    };
    let t = (days as f32 / 365.0).min(1.0);
    let (a, b, f) = if t < 0.5 {
        ([43.0, 170.0, 171.0], [223.0, 177.0, 83.0], t * 2.0)
    } else {
        ([223.0, 177.0, 83.0], [220.0, 91.0, 86.0], (t - 0.5) * 2.0)
    };
    Color32::from_rgb(
        (a[0] + (b[0] - a[0]) * f) as u8,
        (a[1] + (b[1] - a[1]) * f) as u8,
        (a[2] + (b[2] - a[2]) * f) as u8,
    )
}

fn size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut n = bytes as f64;
    let mut i = 0;
    while n >= 1024.0 && i < units.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    format!("{n:.1} {}", units[i])
}
fn age_text(lang: Language, age: Option<u64>) -> String {
    age.map(|d| tr_format!(lang, "{d}일 전", "{d} days ago"))
        .unwrap_or_else(|| tr!(lang, "날짜 미확인", "Date unknown").into())
}

fn date_label(lang: Language, at: SystemTime) -> String {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|d| {
            i64::try_from(d.as_secs())
                .ok()
                .and_then(|secs| chrono::DateTime::from_timestamp(secs, d.subsec_nanos()))
        })
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| tr!(lang, "시각 미확인", "Time unknown").into())
}

fn open_folder(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(path)
            .spawn()?;
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
    Ok(())
}

fn demo_report() -> Report {
    let now = SystemTime::now();
    let mut nodes: Vec<Node> = Vec::new();
    let specs = [
        ("Demo", None, 0, 0),
        ("Archive", Some(0), 6, 540),
        ("Projects", Some(0), 4, 5),
        ("Downloads", Some(0), 18, 240),
        ("Media", Some(0), 12, 90),
        ("Backups", Some(1), 38, 600),
        ("Old builds", Some(1), 16, 540),
        ("Active", Some(2), 8, 5),
        ("Experiments", Some(2), 7, 320),
        ("Video", Some(4), 24, 120),
    ];
    for (name, parent, gib, days) in specs {
        let path = parent
            .map(|p: usize| nodes[p].path.join(name))
            .unwrap_or_else(|| PathBuf::from(name));
        nodes.push(Node {
            path,
            parent,
            children: vec![],
            bytes: gib * 1024u64.pow(3),
            own_bytes: gib * 1024u64.pow(3),
            files: gib * 80,
            modified: Some(now - Duration::from_secs(days * 86400)),
            accessed: Some(now - Duration::from_secs(days * 86400)),
            missing_modified: false,
            missing_accessed: false,
            incomplete: false,
        });
        if let Some(p) = parent {
            let id = nodes.len() - 1;
            nodes[p].children.push(id);
        }
    }
    for i in (1..nodes.len()).rev() {
        let p = nodes[i].parent.unwrap();
        nodes[p].bytes += nodes[i].bytes;
        nodes[p].files += nodes[i].files;
    }
    Report {
        progress: Progress {
            folders: nodes.len(),
            files: nodes[0].files,
            bytes: nodes[0].bytes,
            ..Default::default()
        },
        nodes,
        issues: vec![],
        cancelled: false,
        scanning: false,
        elapsed: Duration::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_single_click_selects_and_double_click_navigates() {
        let ctx = egui::Context::default();
        let mut report = demo_report();
        report.nodes[0].children = vec![1];
        let mut navigate = None;
        let mut selection = None;
        let mut refresh = None;
        let mut frame = |time, pressed: Option<bool>| {
            let events = pressed
                .map(|pressed| {
                    vec![
                        egui::Event::PointerMoved(egui::pos2(100.0, 100.0)),
                        egui::Event::PointerButton {
                            pos: egui::pos2(100.0, 100.0),
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ]
                })
                .unwrap_or_default();
            let _ = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(500.0, 400.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let mut budget = 10;
                        let active_selection = selection;
                        let mut renderer = MapRenderer {
                            language: Language::Korean,
                            nodes: &report.nodes,
                            access: false,
                            now: SystemTime::now(),
                            navigate: &mut navigate,
                            refresh: &mut refresh,
                            selection: &mut selection,
                            active_selection,
                            can_refresh: true,
                            budget: &mut budget,
                        };
                        renderer.draw(
                            ui,
                            0,
                            Rect::from_min_size(egui::pos2(10.0, 10.0), Vec2::new(300.0, 200.0)),
                            1,
                        );
                    });
                },
            );
            (selection, navigate)
        };
        frame(0.0, None);
        frame(0.1, Some(true));
        assert_eq!(frame(0.12, Some(false)), (Some(1), None));
        frame(0.2, Some(true));
        assert_eq!(frame(0.22, Some(false)), (Some(1), Some(1)));
    }

    fn app() -> DiskApp {
        DiskApp {
            language: Language::Korean,
            path: "Demo".into(),
            drives: vec![],
            receiver: None,
            refresh_receiver: None,
            cancel: None,
            progress: Progress::default(),
            report: Some(Arc::new(demo_report())),
            cache: None,
            cache_generation: 0,
            cache_loading: false,
            cache_saves: 0,
            cache_status: String::new(),
            cached_at: None,
            closing: false,
            error: None,
            focus: 0,
            selected_path: None,
            access: false,
            days: 180,
            min_mb: 1024,
            depth: 2,
            query: String::new(),
            candidates: vec![],
            candidate_count: 0,
            children: vec![],
            now: SystemTime::now(),
        }
    }

    fn cache_channels() -> (
        cache::Worker,
        std::sync::mpsc::Sender<cache::Event>,
        Receiver<cache::Request>,
    ) {
        let (tx, requests) = std::sync::mpsc::channel();
        let (events, rx) = std::sync::mpsc::channel();
        (
            cache::Worker {
                tx,
                rx,
                path: PathBuf::from("test.sqlite3"),
            },
            events,
            requests,
        )
    }

    #[test]
    fn late_cache_load_cannot_overwrite_newer_work() {
        let mut app = app();
        let (worker, events, _requests) = cache_channels();
        app.cache = Some(worker);
        app.cache_generation = 2;
        app.navigate(1);
        events
            .send(cache::Event::Loaded {
                generation: 1,
                result: Ok(Some(cache::Cached {
                    report: demo_report(),
                    saved_at: SystemTime::now(),
                })),
            })
            .unwrap();
        app.poll_cache();
        assert_eq!(app.focus, 1);
        assert!(app.cached_at.is_none());
        let at = SystemTime::now();
        events
            .send(cache::Event::Loaded {
                generation: 2,
                result: Ok(Some(cache::Cached {
                    report: demo_report(),
                    saved_at: at,
                })),
            })
            .unwrap();
        app.poll_cache();
        assert_eq!(app.cached_at, Some(at));
        assert!(!app.cache_loading);
        assert!(!app.candidates.is_empty());
    }

    #[test]
    fn finished_scan_queues_shared_snapshot_and_cancelled_scan_does_not_save() {
        let mut app = app();
        let (worker, events, requests) = cache_channels();
        app.cache = Some(worker);
        let (tx, rx) = std::sync::mpsc::channel();
        app.receiver = Some(rx);
        tx.send(Event::Finished(demo_report())).unwrap();
        app.poll();
        match requests.try_recv().unwrap() {
            cache::Request::Save { report, .. } => {
                assert!(Arc::ptr_eq(&report, app.report.as_ref().unwrap()))
            }
            _ => panic!("Expected snapshot save"),
        }
        assert_eq!(app.cache_saves, 1);
        events
            .send(cache::Event::Saved {
                generation: 0,
                result: Ok(SystemTime::now()),
            })
            .unwrap();
        app.poll_cache();
        assert_eq!(app.cache_saves, 0);
        assert!(app.cache_status.contains("DB 저장 완료"));
        let mut cancelled = demo_report();
        cancelled.cancelled = true;
        app.replace_report(cancelled);
        app.save_cache();
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn deleted_focus_moves_to_parent_and_merged_root_is_saved() {
        let mut app = app();
        let (worker, _events, requests) = cache_channels();
        app.cache = Some(worker);
        app.navigate(5);
        app.cached_at = Some(SystemTime::now());
        let mut report = demo_report();
        let removed = report.nodes[5].clone();
        let mut parent = removed.parent;
        while let Some(id) = parent {
            report.nodes[id].bytes -= removed.bytes;
            report.nodes[id].files -= removed.files;
            parent = report.nodes[id].parent;
        }
        report.nodes.remove(5);
        for n in &mut report.nodes {
            n.parent = n.parent.map(|id| if id > 5 { id - 1 } else { id });
            n.children.retain(|&id| id != 5);
            for id in &mut n.children {
                if *id > 5 {
                    *id -= 1;
                }
            }
        }
        report.progress.bytes = report.nodes[0].bytes;
        report.progress.files = report.nodes[0].files;
        app.finish_subtree(Ok(report));
        assert_eq!(app.focus, 1);
        assert!(app.cached_at.is_none());
        match requests.try_recv().unwrap() {
            cache::Request::Save { report, .. } => {
                assert_eq!(report.nodes[0].path, PathBuf::from("Demo"));
                assert!(!report.nodes.iter().any(|n| n.path.ends_with("Backups")));
            }
            _ => panic!("Expected save of merged map"),
        }
    }

    #[test]
    fn failed_subtree_refresh_keeps_map_and_does_not_save() {
        let mut app = app();
        let (worker, _events, requests) = cache_channels();
        app.cache = Some(worker);
        let previous = Arc::clone(app.report.as_ref().unwrap());
        let saved_at = SystemTime::now();
        app.cached_at = Some(saved_at);
        app.navigate(1);
        app.finish_subtree(Err("읽기 실패".into()));
        assert!(Arc::ptr_eq(&previous, app.report.as_ref().unwrap()));
        assert_eq!(app.focus, 1);
        assert_eq!(app.cached_at, Some(saved_at));
        assert!(requests.try_recv().is_err());
        assert!(!app.busy());
    }

    #[test]
    fn refresh_selected_top_level_leaf_keeps_root_view_and_updates_db_snapshot() {
        use std::{fs, time::Instant};
        let root = std::env::temp_dir().join(format!(
            "diskmap-selection-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("$Recycle.Bin")).unwrap();
        fs::create_dir_all(root.join("keep")).unwrap();
        fs::write(root.join("$Recycle.Bin/deleted.bin"), [0; 37]).unwrap();
        fs::write(root.join("keep/keep.bin"), [0; 11]).unwrap();
        let report = scan::scan(&root, &AtomicBool::new(false), |_| {}).unwrap();
        let mut app = app();
        app.replace_report(report);
        app.navigate(0);
        let selected = root.join("$Recycle.Bin");
        app.selected_path = Some(selected.clone());
        let target = app.selected_index().unwrap();
        assert_ne!(target, app.focus);
        let (worker, _events, requests) = cache_channels();
        app.cache = Some(worker);
        fs::remove_file(root.join("$Recycle.Bin/deleted.bin")).unwrap();
        app.begin_subtree_scan(target);
        let start = Instant::now();
        while app.busy() {
            app.poll_subtree();
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.error.is_none(), "{:?}", app.error);
        assert_eq!(app.focus, 0);
        assert_eq!(app.selected_path.as_ref(), Some(&selected));
        let report = app.report.as_ref().unwrap();
        assert_eq!(report.nodes[0].bytes, 11);
        assert_eq!(report.nodes[app.selected_index().unwrap()].bytes, 0);
        match requests.try_recv().unwrap() {
            cache::Request::Save { report, .. } => {
                assert_eq!(report.nodes[0].path, root);
                assert_eq!(report.nodes[0].bytes, 11);
            }
            _ => panic!("Expected merged snapshot"),
        }
        drop(app);
        let target = root.canonicalize().unwrap();
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(target.parent(), Some(temp.as_path()));
        assert!(
            target
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("diskmap-selection-")
        );
        fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn candidates_exclude_recent_unknown_and_partial_folders() {
        let mut app = app();
        let nodes = &mut Arc::get_mut(app.report.as_mut().unwrap()).unwrap().nodes;
        nodes[1].incomplete = true;
        nodes[3].missing_modified = true;
        app.refresh_candidates();
        assert_eq!(app.candidates, vec![5, 6, 8]);
        app.query = "experiments".into();
        app.refresh_candidates();
        assert_eq!(app.candidates, vec![8]);
        app.min_mb = 100_000;
        app.refresh_candidates();
        assert!(app.candidates.is_empty());
    }

    #[test]
    fn previews_do_not_offer_candidates_or_reset_navigation() {
        let mut app = app();
        app.navigate(1);
        let (tx, rx) = std::sync::mpsc::channel();
        app.receiver = Some(rx);
        let mut preview = demo_report();
        preview.scanning = true;
        tx.send(Event::Preview(preview)).unwrap();
        app.poll();
        app.refresh_candidates();
        assert_eq!(app.focus, 1);
        assert!(app.candidates.is_empty());
        tx.send(Event::Finished(demo_report())).unwrap();
        app.poll();
        assert_eq!(app.focus, 1);
        assert!(!app.candidates.is_empty());
        assert!(app.receiver.is_none());
    }

    #[test]
    fn mft_reordered_nodes_preserve_focus_by_path_and_fallback_clears_map() {
        let mut app = app();
        app.navigate(1);
        let path = app.report.as_ref().unwrap().nodes[1].path.clone();
        let mut next = demo_report();
        next.nodes.swap(1, 2);
        let remap = |id| match id {
            1 => 2,
            2 => 1,
            other => other,
        };
        for n in &mut next.nodes {
            n.parent = n.parent.map(remap);
            for child in &mut n.children {
                *child = remap(*child);
            }
        }
        app.replace_report(next);
        assert_eq!(app.focus, 2);
        assert_eq!(app.report.as_ref().unwrap().nodes[app.focus].path, path);
        let (tx, rx) = std::sync::mpsc::channel();
        app.receiver = Some(rx);
        tx.send(Event::Restart(Progress {
            engine: "폴더 열거",
            note: "MFT fallback".into(),
            ..Default::default()
        }))
        .unwrap();
        app.poll();
        assert!(app.report.is_none());
        assert!(app.children.is_empty());
        assert_eq!(app.focus, 0);
        assert_eq!(app.progress.note, "MFT fallback");
    }

    #[test]
    fn demo_panels_render_and_navigation_changes_children() {
        let mut app = app();
        app.navigate(0);
        app.refresh_candidates();
        let ctx = egui::Context::default();
        let original = Arc::clone(app.report.as_ref().unwrap());
        for language in [Language::Korean, Language::English, Language::Korean] {
            app.language = language;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(1440.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    app.top(ctx);
                    app.bottom(ctx);
                    app.sidebar(ctx);
                    app.main_panel(ctx);
                },
            );
            assert!(!output.shapes.is_empty());
            fn collect(shape: &egui::epaint::Shape, text: &mut String) {
                match shape {
                    egui::epaint::Shape::Text(s) => {
                        text.push_str(s.galley.text());
                        text.push('\n');
                    }
                    egui::epaint::Shape::Vec(shapes) => {
                        for shape in shapes {
                            collect(shape, text);
                        }
                    }
                    _ => {}
                }
            }
            let mut text = String::new();
            for shape in &output.shapes {
                collect(&shape.shape, &mut text);
            }
            let expected = tr!(language, "선택 항목 새로 고침", "Refresh selected");
            assert!(text.contains(expected), "Missing {expected}: {text}");
            let other = tr!(language, "Refresh selected", "선택 항목 새로 고침");
            assert!(!text.contains(other));
            assert!(Arc::ptr_eq(app.report.as_ref().unwrap(), &original));
            assert_eq!(app.focus, 0);
        }
        app.navigate(1);
        assert_eq!(app.children, vec![5, 6]);
        app.navigate(5);
        assert!(app.children.is_empty());
    }
}
