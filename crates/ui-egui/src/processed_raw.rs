//! Cancellable source-stage DNG jobs run off the UI thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lightcraft_engine::cmd::raw_processing::{ProcessedDngResult, Processor, prepare_processed_dng_with_processor, processor_from_params};
use serde_json::{Value, json};

use crate::LightcraftApp;

const LABEL: &str = "RAW Processing";

#[derive(Default)]
pub struct State {
    running: Option<Running>,
    pub last: Option<Value>,
    pmrid_compiled: Option<bool>,
    pub experimental: ExperimentalDraft,
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(task) = &self.running {
            task.cancel.store(true, Ordering::Relaxed);
        }
    }
}

struct Running {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<(usize, usize)>>,
    mode: &'static str,
}

pub fn run(app: &mut LightcraftApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    match id {
        "folder.exportProcessedDng" | "photo.exportProcessedDng" => Some(start(app, params)),
        "folder.cancelProcessedDng" => Some(match &app.processed_raw.running {
            Some(task) => {
                task.cancel.store(true, Ordering::Relaxed);
                Ok(json!({"stopping": true}))
            }
            None => Err("No RAW processing job is running".into()),
        }),
        _ => None,
    }
}

fn start(app: &mut LightcraftApp, params: &Value) -> Result<Value, String> {
    if !params.is_object() {
        return Err("RAW processing parameters must be an object".into());
    }
    let processor = processor_from_params(params).map_err(|error| error.to_string())?;
    let mode = match &processor {
        Processor::Native => "native",
        Processor::ExperimentalPmrid { .. } => "experimental-pmrid",
    };
    if cfg!(target_arch = "wasm32") {
        return Err("Cleaned DNG processing requires the Windows or Linux desktop app".into());
    }
    if app.processed_raw.running.is_some() || app.tasks.is_running(LABEL) {
        return Err("RAW processing is already running".into());
    }
    let ids = lightcraft_engine::cmd::raw_processing::checked_targets(&app.session, params, "photo.exportProcessedDng")
        .map_err(|error| error.to_string())?;
    let path = match params.get("path") {
        Some(path) => Some(std::path::Path::new(path.as_str().ok_or("path must be a string")?)),
        None => None,
    };
    if ids.is_empty() {
        return Err("Select a supported RAW photo first".into());
    }
    if path.is_some() && ids.len() != 1 {
        return Err("An explicit output path requires one photo".into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let mut jobs = Vec::new();
    // All selected files must support source processing before any work starts.
    for id in ids {
        let mut job = prepare_processed_dng_with_processor(&app.session, id, path, processor.clone())?;
        if jobs.iter().any(|previous: &lightcraft_engine::cmd::raw_processing::ProcessedDngJob| previous.output == job.output) {
            return Err("Selected photos would share an output filename; process them separately with explicit output paths".into());
        }
        job.cancel = cancel.clone();
        jobs.push(job);
    }
    let total = jobs.len();
    let progress = Arc::new(Mutex::new((0, total)));
    let worker_progress = progress.clone();
    let worker_cancel = cancel.clone();
    crate::tasks::spawn(
        app,
        LABEL,
        move || {
            let mut results = Vec::<Result<ProcessedDngResult, String>>::new();
            for job in jobs {
                if worker_cancel.load(Ordering::Relaxed) {
                    break;
                }
                results.push(job.run());
                if let Ok(mut counts) = worker_progress.lock() {
                    counts.0 += 1;
                }
            }
            (results, worker_cancel.load(Ordering::Relaxed))
        },
        move |app, ctx, (results, cancelled)| {
            app.processed_raw.running = None;
            let mut saved = Vec::new();
            let mut failed = Vec::new();
            for result in results {
                match result {
                    Ok(result) => saved.push(serde_json::to_value(result).unwrap_or_default()),
                    Err(error) => failed.push(error),
                }
            }
            let message =
                format!("{} cleaned DNG(s) saved · {} failure(s){}", saved.len(), failed.len(), if cancelled { " · cancelled" } else { "" });
            app.processed_raw.last = Some(json!({"mode":mode,"saved": saved, "failed": failed, "cancelled": cancelled}));
            if let Some(error) = failed.first() {
                app.toast_error(ctx, format!("{message}: {error}"));
            } else {
                app.toast(ctx, message);
            }
        },
    )?;
    app.processed_raw.running = Some(Running { cancel, progress, mode });
    Ok(json!({"running": true, "total": total,"mode":mode}))
}

pub fn status(app: &LightcraftApp) -> Value {
    let running = app.processed_raw.running.as_ref().map(|task| {
        let (done, total) = task.progress.lock().map(|counts| *counts).unwrap_or((0, 0));
        json!({"mode":task.mode,"done": done, "total": total, "stopping": task.cancel.load(Ordering::Relaxed)})
    });
    json!({"running": running, "last": app.processed_raw.last})
}

pub fn show(app: &mut LightcraftApp, ctx: &egui::Context) {
    let Some(task) = &app.processed_raw.running else { return };
    if !app.tasks.is_running(LABEL) {
        // The worker disconnected: tasks::poll already displayed its error.
        app.processed_raw.running = None;
        return;
    }
    let (done, total) = task.progress.lock().map(|counts| *counts).unwrap_or((0, 1));
    let stopping = task.cancel.load(Ordering::Relaxed);
    let cancel = task.cancel.clone();
    let experimental = task.mode == "experimental-pmrid";
    let title = if experimental { "Experimental AI RAW Denoise" } else { "RAW Denoise (Native)" };
    egui::Window::new(title).resizable(false).anchor(egui::Align2::CENTER_BOTTOM, [0.0, -80.0]).show(ctx, |ui| {
        ui.label(format!("Processing cleaned DNG · {done} of {total}"));
        ui.label(if experimental { "Research CPU model; cancellation waits for the current operation." } else { "Classical CPU processing." });
        ui.add(egui::ProgressBar::new(done as f32 / total.max(1) as f32).desired_width(320.0));
        let response = ui.add_enabled(!stopping, egui::Button::new(if stopping { "Stopping…" } else { "Cancel" }));
        crate::widgets::register(ui.ctx(), "button:cancelRawProcessing", response.rect);
        if response.clicked() {
            cancel.store(true, Ordering::Relaxed);
        }
    });
}

/// Unpersisted explicit inputs: a measured camera profile is never selected or invented.
pub struct ExperimentalDraft {
    pub checkpoint: String,
    pub noise_profile: String,
    pub strength: f64,
}

impl Default for ExperimentalDraft {
    fn default() -> Self {
        Self { checkpoint: String::new(), noise_profile: String::new(), strength: 70.0 }
    }
}

pub fn show_experimental(app: &mut LightcraftApp, ui: &mut egui::Ui, photo: lightcraft_catalog::PhotoId) {
    let compiled = match app.processed_raw.pmrid_compiled {
        Some(value) => value,
        None => {
            let value = app
                .session
                .execute("raw.processingCapabilities", &json!({}))
                .ok()
                .is_some_and(|report| report["experimentalPmrid"]["compiled"] == true);
            app.processed_raw.pmrid_compiled = Some(value);
            value
        }
    };
    if !compiled {
        return;
    }
    let mut draft = std::mem::take(&mut app.processed_raw.experimental);
    let header = ui.collapsing("Experimental AI RAW denoise", |ui| {
        ui.label("Requires a measured camera noise profile. CPU processing can be slow.");
        ui.label("Official PMRID checkpoint");
        let input = ui.add(egui::TextEdit::singleline(&mut draft.checkpoint).desired_width(ui.available_width()));
        crate::widgets::register(ui.ctx(), "input:rawAiCheckpoint", input.rect);
        ui.label("Measured noise profile JSON");
        let input = ui.add(egui::TextEdit::singleline(&mut draft.noise_profile).desired_width(ui.available_width()));
        crate::widgets::register(ui.ctx(), "input:rawAiNoiseProfile", input.rect);
        ui.horizontal(|ui| {
            ui.label("Strength");
            let input = ui.add(egui::DragValue::new(&mut draft.strength).range(1.0..=100.0).speed(1.0));
            crate::widgets::register(ui.ctx(), "input:rawAiStrength", input.rect);
        });
        let ready = !draft.checkpoint.trim().is_empty()
            && !draft.noise_profile.trim().is_empty()
            && draft.strength.is_finite()
            && (1.0..=100.0).contains(&draft.strength);
        let button = ui.add_enabled(ready, egui::Button::new("Save Experimental AI DNG"));
        crate::widgets::register(ui.ctx(), "button:processedAiDng", button.rect);
        if button.clicked() {
            let params = json!({"ids":[photo.0],"mode":"experimental-pmrid",
                "checkpoint":draft.checkpoint,"noiseProfile":draft.noise_profile,"strength":draft.strength});
            if let Err(error) = app.run("photo.exportProcessedDng", params) {
                app.toast_error(ui.ctx(), error);
            }
        }
    });
    crate::widgets::register(ui.ctx(), "disclosure:rawAi", header.header_response.rect);
    app.processed_raw.experimental = draft;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "pmrid", not(target_arch = "wasm32")))]
    #[test]
    fn experimental_ui_requires_explicit_inputs_and_reports_cancellable_provider() {
        use crate::headless::Headless;
        use lightcraft_catalog::{MediaKind, Op, Photo, PhotoId, Source};
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("lc-raw-ai-ui-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.ARW");
        std::fs::write(&source, b"mock source").unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let gate = Mutex::new(gate);
        let mut session = lightcraft_engine::Session::new();
        session.media.file_bytes = Some(Arc::new(move |_| {
            let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
            Err("mock source read".into())
        }));
        session.media.file_loader = Some(Arc::new(|_, _| {
            Ok((lightcraft_raster::Rgb32f::filled(90, 60, [0.2; 3]), lightcraft_pipeline::SourceInfo { raw: true, ..Default::default() }))
        }));
        let mut photo = Photo::new(PhotoId(1), Source::File { path: source.to_string_lossy().into() }, "source.ARW", "ARW", 90, 60, "");
        photo.kind = MediaKind::Raw;
        session.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        session.execute("library.select", &json!({"ids":[1]})).unwrap();
        let services = crate::Services {
            png: Some(Box::new(|img| lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(img), &Default::default()).unwrap())),
            write: Some(Box::new(|p, b| std::fs::write(p, b).map_err(|e| e.to_string()))),
            ..Default::default()
        };
        let mut app = LightcraftApp::new(session, services);
        app.ui.open_sections = vec!["detail".into()];
        app.ui.histogram = false;
        app.ui.filmstrip = false;
        let mut h = Headless::new(app, [1300.0, 1100.0], 1.0);
        h.step();
        h.step();
        let timeout = Duration::from_secs(20);
        let response = h.request("ui.clickWidget", json!({"id":"disclosure:rawAi"}), timeout);
        assert_eq!(response["ok"], true, "{response}");
        h.request("ui.clickWidget", json!({"id":"button:processedAiDng"}), timeout);
        assert!(h.app.processed_raw.running.is_none(), "blank profile inputs must not dispatch");
        for (widget, text) in
            [("input:rawAiCheckpoint", "/explicit/torch_pretrained.ckp"), ("input:rawAiNoiseProfile", "/explicit/measured-camera.json")]
        {
            assert_eq!(h.request("ui.clickWidget", json!({"id":widget}), timeout)["ok"], true);
            assert_eq!(h.request("ui.text", json!({"text":text}), timeout)["ok"], true);
        }
        assert_eq!(h.request("ui.clickWidget", json!({"id":"button:processedAiDng"}), timeout)["ok"], true);
        assert_eq!(status(&h.app)["running"]["mode"], "experimental-pmrid");
        assert_eq!(h.app.session.develop_of(PhotoId(1)).unwrap().detail.raw_denoise, 0.0);
        if let Some(dir) = std::env::var_os("LIGHTCRAFT_RAW_AI_UI_SHOTS") {
            let path = std::path::Path::new(&dir).join("experimental-ai-progress.png");
            assert_eq!(h.request("ui.screenshot", json!({"path":path.to_string_lossy(),"headless":true}), timeout)["ok"], true);
        }
        h.app.run("folder.cancelProcessedDng", json!({})).unwrap();
        let _ = release.send(());
        h.request("ui.settle", json!({"timeoutMs":10_000}), timeout);
        assert!(h.app.processed_raw.running.is_none());
        assert_eq!(status(&h.app)["last"]["mode"], "experimental-pmrid");
        assert_eq!(status(&h.app)["last"]["cancelled"], true);
        assert_eq!(std::fs::read(&source).unwrap(), b"mock source");
        assert!(!root.join("Processed/source-AI-Clean.dng").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_ids_or_path_never_fall_back_to_current_selection_or_default_destination() {
        let mut app = LightcraftApp::new(lightcraft_engine::Session::new(), crate::Services::default());
        assert!(start(&mut app, &json!({})).unwrap_err().contains("select 1..10000"));
        let root = std::env::temp_dir().join(format!("lc-raw-ui-invalid-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.ARW");
        std::fs::write(&source, b"mock source").unwrap();
        let mut photo = lightcraft_catalog::Photo::new(
            lightcraft_catalog::PhotoId(1),
            lightcraft_catalog::Source::File { path: source.to_string_lossy().to_string() },
            "source.ARW",
            "ARW",
            10,
            10,
            "2026-01-01",
        );
        photo.kind = lightcraft_catalog::MediaKind::Raw;
        let mut settings = (*photo.develop).clone();
        settings.detail.raw_denoise = 50.0;
        photo.develop = Arc::new(settings);
        app.session.catalog.apply(lightcraft_catalog::Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        app.session.execute("library.select", &json!({"ids": [1]})).unwrap();
        for params in [
            json!(null),
            json!([]),
            json!(1),
            json!({"ids": "bad"}),
            json!({"ids": []}),
            json!({"id": 0}),
            json!({"id": 1, "ids": [1]}),
            json!({"ids": [1, 1]}),
            json!({"ids": vec![1; 10_001]}),
            json!({"ids": [-1]}),
            json!({"ids": [1.5]}),
            json!({"ids": ["bad"]}),
            json!({"ids": [999]}),
            json!({"id": "bad"}),
            json!({"id": -1}),
            json!({"id": 999}),
            json!({"path": 1}),
            json!({"mode": "experimental-pmrid"}),
            json!({"mode": "unknown"}),
            json!({"mode": 1}),
            json!({"mode": null}),
        ] {
            for id in ["folder.exportProcessedDng", "photo.exportProcessedDng"] {
                let error = run(&mut app, id, &params).unwrap().unwrap_err();
                if params.get("mode").is_some() {
                    assert!(error.contains("mode must") || error.contains("checkpoint"), "{error}");
                }
                assert!(app.processed_raw.running.is_none());
                assert!(!app.tasks.is_running(LABEL));
                assert!(!root.join("Processed/source-Clean.dng").exists());
            }
        }
        assert_eq!(std::fs::read(&source).unwrap(), b"mock source");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn canonical_and_alias_exports_schedule_cancellable_background_jobs() {
        use lightcraft_catalog::{MediaKind, Op, Photo, PhotoId, Source};
        for (i, command) in ["photo.exportProcessedDng", "folder.exportProcessedDng"].into_iter().enumerate() {
            let root = std::env::temp_dir().join(format!("lc-raw-ui-{}-{i}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let source = root.join("source.ARW");
            std::fs::write(&source, b"mock source").unwrap();
            let mut app = LightcraftApp::new(lightcraft_engine::Session::new(), crate::Services::default());
            let (release, gate) = std::sync::mpsc::channel();
            let gate = Mutex::new(gate);
            app.session.media.file_bytes = Some(Arc::new(move |_| {
                let _ = gate.lock().unwrap().recv_timeout(std::time::Duration::from_secs(10));
                Err("mock source read".into())
            }));
            let mut photo =
                Photo::new(PhotoId(1), Source::File { path: source.to_string_lossy().to_string() }, "source.ARW", "ARW", 10, 10, "2026-01-01");
            photo.kind = MediaKind::Raw;
            let mut settings = (*photo.develop).clone();
            settings.detail.raw_denoise = 50.0;
            photo.develop = Arc::new(settings);
            app.session.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
            app.session.execute("library.select", &json!({"ids": [1]})).unwrap();
            let response = app.run(command, json!({})).unwrap();
            assert_eq!(response["running"], true, "{command} must return background status");
            assert!(app.tasks.is_running(LABEL));
            app.run("folder.cancelProcessedDng", json!({})).unwrap();
            let _ = release.send(());
            let ctx = egui::Context::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while app.tasks.is_running(LABEL) && std::time::Instant::now() < deadline {
                crate::tasks::poll(&mut app, &ctx);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(!app.tasks.is_running(LABEL));
            assert_eq!(app.processed_raw.last.as_ref().unwrap()["cancelled"], true);
            assert_eq!(std::fs::read(&source).unwrap(), b"mock source");
            assert!(!root.join("Processed/source-Clean.dng").exists());
            std::fs::remove_dir_all(&root).unwrap();
        }
    }
}
