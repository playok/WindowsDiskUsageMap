#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cache;
mod i18n;
mod mft;
mod scan;
mod subtree;
mod treemap;

fn main() -> eframe::Result {
    if std::env::args().nth(1).as_deref() == Some("--scan-check") {
        let path = std::env::args().nth(2).unwrap_or_else(|| "C:\\".into());
        let started = std::time::Instant::now();
        let mut first = false;
        let result = scan::scan_auto(
            std::path::Path::new(&path),
            &std::sync::atomic::AtomicBool::new(false),
            |event| {
                if matches!(event, scan::Event::Preview(_)) && !first {
                    println!("First map: {:.3}s", started.elapsed().as_secs_f64());
                    first = true;
                }
            },
        );
        match result {
            Ok(r) => println!(
                "Engine: {}\nNote: {}\nFolders: {} Files: {} Bytes: {} Errors: {} Skipped: {}\nElapsed: {:.3}s",
                r.progress.engine,
                r.progress.note,
                r.progress.folders,
                r.progress.files,
                r.progress.bytes,
                r.progress.errors,
                r.progress.skipped,
                started.elapsed().as_secs_f64()
            ),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1000.0, 680.0]),
        ..Default::default()
    };
    eframe::run_native(
        concat!("Disk Usage Map ", env!("CARGO_PKG_VERSION")),
        options,
        Box::new(|cc| Ok(Box::new(app::DiskApp::new(cc)))),
    )
}
