//! Direct-folder workspace. Kept separate so upstream UI improvements stay easy to merge.

use egui::{Key, Modifiers};
use lightcraft_catalog::PhotoId;
use lightcraft_engine::LibrarySource;
use serde_json::{Value, json};

use crate::LightcraftApp;
use crate::state::{BeforeAfter, InfoOverlay, RightPanel, ViewMode, Zoom};

pub fn active(app: &LightcraftApp) -> bool {
    app.ui.folder_cull
        && app.session.source == LibrarySource::Folder
        && !matches!(
            app.ui.right,
            RightPanel::Edit | RightPanel::Crop | RightPanel::Masking | RightPanel::Remove | RightPanel::RedEye | RightPanel::Profiles
        )
        && matches!(app.ui.view, ViewMode::Detail | ViewMode::PhotoGrid | ViewMode::SquareGrid)
}

/// Called after a valid browse starts. Ratings/develop changes use the XMP
/// writer; choosing a folder never moves its originals into the catalog.
pub fn entered(app: &mut LightcraftApp, path: &str) {
    app.ui.folder_path = path.to_string();
    app.ui.folder_cull = true;
    app.ui.folder_undo_actions.clear();
    app.ui.folder_undo_floor = app.session.undo.len();
    app.ui.right = RightPanel::None;
    app.ui.dialog = None;
    app.ui.tool.clear();
    app.gesture = None;
    app.ui.fullscreen = false;
    app.ui.presets = false;
    app.ui.filmstrip = true;
    app.ui.folder_pending_file = None;
    app.ui.pan = (0.5, 0.5);
    app.ui.view = ViewMode::Detail;
    app.ui.before_after = BeforeAfter::Off;
    app.ui.zoom = Zoom::Fit;
    app.ui.click_zoom = 100;
    app.ui.info_overlay = InfoOverlay::Basic;
    app.session.xmp.auto_write = !app.library_problem.as_ref().is_some_and(|problem| problem.dismissed);
    app.session.filter = Default::default();
    app.ui.search.clear();
    let _ = app.session.execute("library.sort", &json!({"key": "fileName", "ascending": true, "group": "none"}));
}

/// CLI files select their folder, then the requested image; extra folders become sidebar roots.
fn open_paths(app: &mut LightcraftApp, p: &Value) -> Result<Value, String> {
    let paths = p.get("paths").and_then(Value::as_array).ok_or("file.openPaths: paths must be an array")?;
    let first = paths.first().and_then(Value::as_str).ok_or("file.openPaths: supply a file or folder")?;
    let path = std::path::absolute(std::path::Path::new(first)).map_err(|e| e.to_string())?;
    let (folder, file) = if path.is_dir() {
        (path.clone(), None)
    } else if path.is_file() {
        (path.parent().ok_or("file has no parent folder")?.to_path_buf(), Some(path.to_string_lossy().to_string()))
    } else {
        return Err(format!("{first}: file or folder does not exist"));
    };
    let result = app.run("library.browse", json!({"path": folder.to_string_lossy(), "subfolders": false}))?;
    entered(app, &folder.to_string_lossy());
    app.ui.folder_pending_file = file;
    for path in paths.iter().filter_map(Value::as_str) {
        let path = std::path::Path::new(path);
        let root = if path.is_dir() {
            Some(path)
        } else if path.is_file() {
            path.parent()
        } else {
            None
        };
        if let Some(root) = root {
            let _ = app.run("local.addRoot", json!({"path": root.to_string_lossy()}));
        }
    }
    Ok(result)
}

pub fn run(app: &mut LightcraftApp, id: &str, p: &Value) -> Option<Result<Value, String>> {
    match id {
        "file.openPaths" => Some(open_paths(app, p)),
        "file.openFolder" => {
            let path =
                p.get("path").and_then(Value::as_str).map(str::to_string).or_else(|| app.services.pick_folder.as_mut().and_then(|pick| pick()));
            Some(match path {
                Some(path) => app.run("file.openPaths", json!({"paths": [path]})),
                None => Ok(Value::Null),
            })
        }
        "folder.refresh" => Some(match app.session.browse.clone() {
            Some(b) if app.session.source == LibrarySource::Folder => {
                let state = app.ui.clone();
                let filter = app.session.filter.clone();
                let sort = app.session.sort;
                let result = app.run("library.browse", json!({"path": b.path, "subfolders": b.subfolders}));
                app.ui = state;
                app.session.filter = filter;
                app.session.sort = sort;
                result
            }
            _ => Err("Open a folder first".into()),
        }),
        "folder.cullMode" => Some(if app.session.source != LibrarySource::Folder {
            Err("Open a folder first".into())
        } else {
            let on = p.get("enabled").and_then(Value::as_bool).unwrap_or(!active(app));
            app.ui.folder_cull = on;
            app.ui.right = if on { RightPanel::None } else { RightPanel::Edit };
            app.ui.view = ViewMode::Detail;
            app.ui.before_after = BeforeAfter::Off;
            app.ui.tool.clear();
            Ok(json!({"culling": on}))
        }),
        "folder.reject" => Some(if app.scan.is_some() || app.import.is_some() {
            Err("Wait for this folder to finish reading before moving files".into())
        } else if !active(app) {
            Err("Enable the folder culling workspace first".into())
        } else {
            match app.session.execute("cull.reject", &json!({})) {
                Ok(r) => {
                    app.ui.folder_undo_actions.push(app.session.undo.len());
                    Ok(r)
                }
                Err(e) => Err(e.to_string()),
            }
        }),
        "folder.undo" => Some(undo(app)),
        // The native menu consumes Cmd+Z on macOS before egui sees it.
        "edit.undo" if active(app) => Some(undo(app)),
        _ => None,
    }
}

fn undo(app: &mut LightcraftApp) -> Result<Value, String> {
    let last_move_depth = app.ui.folder_undo_actions.last().copied().unwrap_or(app.ui.folder_undo_floor);
    if app.session.undo.len() > last_move_depth {
        // Every real edit input (keyboard, stars, Develop) takes priority over an earlier file move.
        // Failed XMP writes still leave their catalog undo applied; depth reflects that correctly.
        return app.session.execute("edit.undo", &json!({})).map_err(|e| e.to_string());
    }
    if app.scan.is_some() || app.import.is_some() {
        return Err("Wait for this folder to finish reading before undoing file moves".into());
    }
    let folder = app.session.browse.as_ref().filter(|_| app.session.source == LibrarySource::Folder).ok_or("Open a folder first")?.path.clone();
    // A fresh session can restore the durable rejection history. An empty history does not
    // undo unrelated edits from before this folder was opened.
    let history = app.session.execute("cull.history", &json!({"folder": folder})).map_err(|e| e.to_string())?;
    if history.get("transactions").and_then(Value::as_array).is_some_and(|v| v.iter().any(|t| t["state"] == "committed")) {
        let result = app.session.execute("cull.undo", &json!({"folder": folder})).map_err(|e| e.to_string());
        if result.is_ok() {
            app.ui.folder_undo_actions.pop();
        }
        result
    } else {
        Err("No folder edit or completed rejection to undo".into())
    }
}

/// Select the first visible photo while scan batches arrive. Never steal selection while browsing.
pub fn tick(app: &mut LightcraftApp) {
    if !app.ui.folder_cull || app.session.source != LibrarySource::Folder {
        return;
    }
    let visible = app.session.visible_cloned();
    let requested = app.ui.folder_pending_file.as_ref().and_then(|path| {
        let requested = lightcraft_catalog::query::folder_key(path);
        visible.iter().copied().find(|id| {
            app.session.catalog.photo(*id).is_some_and(|p| {
                matches!(&p.source, lightcraft_catalog::Source::File { path: source } if lightcraft_catalog::query::folder_key(source) == requested)
            })
        })
    });
    if let Some(id) = requested {
        let _ = app.session.execute("library.select", &json!({"ids": [id.0]}));
        app.ui.folder_pending_file = None;
    } else if app.session.active().is_none_or(|id| !visible.contains(&id)) {
        let ids: Vec<u64> = visible.first().map(|id| vec![id.0]).unwrap_or_default();
        let _ = app.session.execute("library.select", &json!({"ids": ids}));
    }
    if app.scan.is_none() && app.import.is_none() {
        app.ui.folder_pending_file = None;
    }
}

/// Cull bindings are context-specific and stop before the ordinary editor keymap.
pub fn handle_keys(app: &mut LightcraftApp, ctx: &egui::Context) -> bool {
    if !active(app) || app.ui.dialog.is_some() {
        return false;
    }
    let action = ctx.input(|i| {
        i.events.iter().find_map(|event| {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = event else { return None };
            if *key == Key::Z && (modifiers.command || modifiers.ctrl) && !modifiers.shift && !modifiers.alt {
                return Some(("folder.undo", json!({})));
            }
            if *modifiers != Modifiers::NONE {
                return None;
            }
            match key {
                Key::ArrowLeft | Key::ArrowUp => Some(("library.previous", json!({}))),
                Key::ArrowRight | Key::ArrowDown => Some(("library.next", json!({}))),
                Key::Z => Some(("view.zoomToggle", json!({}))),
                Key::X => Some(("folder.reject", json!({}))),
                _ => [Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5]
                    .iter()
                    .position(|k| k == key)
                    .map(|n| ("photo.rate", json!({"rating": n}))),
            }
        })
    });
    let Some((id, mut params)) = action else { return false };
    if id == "photo.rate"
        && let Some(PhotoId(id)) = app.session.active()
    {
        params["ids"] = json!([id]);
    }
    let result = app.run(id, params);
    if let Err(error) = result {
        app.toast_error(ctx, error);
    }
    true
}

/// Recursive browsing excludes workflow outputs, refuses symlink directory traversal and hidden
/// directories, and caps depth. Explicitly opening an output folder still allows inspecting it.
pub fn scan_paths(root: &str, subfolders: bool) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    let mut pending = vec![(std::path::PathBuf::from(root), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() && subfolders && depth < 64 && !workspace_output(&name) {
                pending.push((path, depth + 1));
            } else if (kind.is_file() || kind.is_symlink() && path.is_file()) && lightcraft_engine::import::is_supported(&path) {
                files.push(path.to_string_lossy().to_string());
            }
        }
    }
    files.sort_by(|a, b| {
        let a_name = std::path::Path::new(a).file_name().and_then(|name| name.to_str()).unwrap_or(a);
        let b_name = std::path::Path::new(b).file_name().and_then(|name| name.to_str()).unwrap_or(b);
        lightcraft_catalog::query::natural_file_name_cmp(a_name, b_name).then_with(|| a.cmp(b))
    });
    Ok(files)
}

fn workspace_output(name: &str) -> bool {
    ["Rejected", "Processed", "LightCraft Exports"].iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// First launch presents the actual direct-folder action instead of an artificial demo library.
pub fn empty(app: &mut LightcraftApp, ui: &mut egui::Ui) {
    let rect = ui.max_rect();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Center)));
    child.add_space((rect.height() / 2.0 - 70.0).max(0.0));
    child.heading("Open a folder and start working");
    child.label("Browse originals in place, cull, and develop. Changes save beside your photos in XMP.");
    child.add_space(12.0);
    let response = child.button("Open Folder…");
    crate::widgets::register(ui.ctx(), "button:openFolder", response.rect);
    if response.clicked()
        && let Err(e) = app.run("file.openFolder", json!({}))
    {
        app.toast_error(ui.ctx(), e);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use lightcraft_catalog::{Flag, PhotoId};
    use lightcraft_engine::Session;
    use lightcraft_raster::Rgba8;

    use super::*;
    use crate::Services;
    use crate::headless::Headless;

    const TIMEOUT: Duration = Duration::from_secs(40);
    static FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Folder(std::path::PathBuf);

    impl Folder {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("lightcraft-folder-workspace-{}-{}", std::process::id(), FIXTURE.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn photo(&self, name: &str, offset: u8) -> Vec<u8> {
            let image = Rgba8::from_fn(128, 96, |x, y| [x as u8 + offset, y as u8, offset, 255]);
            let bytes =
                lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&image), &lightcraft_codecs::EncodeMeta::default()).unwrap();
            std::fs::write(self.0.join(name), &bytes).unwrap();
            bytes
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn headless() -> Headless {
        lightcraft_engine::gpu::set_enabled(false);
        let mut app = LightcraftApp::new(Session::new().with_fs(), Services::default());
        app.ui.settings.gpu = false;
        app.ui.right = RightPanel::None;
        Headless::new(app, [1200.0, 800.0], 1.0)
    }

    fn invoke(h: &mut Headless, id: &str, params: Value) -> Value {
        let result = h.request("engine.execute", json!({"command": id, "params": params}), TIMEOUT);
        assert_eq!(result["ok"], true, "{id}: {result}");
        result["result"].clone()
    }

    fn screenshot(h: &mut Headless, name: &str) {
        let Some(dir) = std::env::var_os("LIGHTCRAFT_FOLDER_SCREENSHOTS") else { return };
        let path = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&path).unwrap();
        let image = h.snapshot(TIMEOUT);
        let bytes: Vec<u8> = image.pixels.iter().flat_map(|pixel| pixel.to_array()).collect();
        let image = Rgba8::from_bytes(image.size[0], image.size[1], &bytes).unwrap();
        let png = lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&image), &lightcraft_codecs::EncodeMeta::default()).unwrap();
        std::fs::write(path.join(name), png).unwrap();
    }

    #[test]
    fn empty_session_has_direct_folder_action() {
        let mut h = headless();
        assert!(h.settle(TIMEOUT));
        assert!(h.app.widgets.iter().any(|(id, _)| id == "button:openFolder"));
        screenshot(&mut h, "folder-empty.png");
    }

    #[test]
    fn open_folder_culls_in_place_in_natural_order_and_persists_rating() {
        let folder = Folder::new();
        let original = folder.photo("IMG_2.png", 2);
        folder.photo("IMG_10.png", 10);
        folder.photo("IMG_1.png", 1);
        let mut h = headless();
        invoke(&mut h, "file.openFolder", json!({"path": folder.0.to_string_lossy()}));
        assert!(h.settle(TIMEOUT));
        assert!(active(&h.app));
        assert_eq!(h.app.ui.view, ViewMode::Detail);
        assert!(h.app.ui.dialog.is_none(), "no import review");
        assert!(h.app.session.catalog.photos().all(|p| p.local), "folder photos stay outside library collection");
        let visible = h.app.session.visible_cloned();
        let names: Vec<_> = visible.iter().map(|id| h.app.session.catalog.photo(*id).unwrap().file_name.as_str()).collect();
        assert_eq!(names, ["IMG_1.png", "IMG_2.png", "IMG_10.png"]);
        assert_eq!(h.app.session.active(), visible.first().copied());
        h.request("ui.key", json!({"key": "right"}), TIMEOUT);
        assert_eq!(h.app.session.active(), Some(visible[1]));
        h.request("ui.key", json!({"key": "5"}), TIMEOUT);
        assert_eq!(h.app.session.catalog.photo(visible[1]).unwrap().rating, 5);
        let xmp = std::fs::read_to_string(folder.0.join("IMG_2.xmp")).unwrap();
        assert_eq!(lightcraft_engine::sidecar::parse_sidecar(&xmp, false).unwrap().rating, Some(5));
        assert_eq!(std::fs::read(folder.0.join("IMG_2.png")).unwrap(), original);
        h.request("ui.key", json!({"key": "Z"}), TIMEOUT);
        assert_eq!(h.app.ui.zoom, Zoom::Percent(100));
        assert!(h.settle(TIMEOUT));
        screenshot(&mut h, "folder-culling.png");
        h.request("ui.key", json!({"key": "Z", "ctrl": true}), TIMEOUT);
        assert_eq!(h.app.session.catalog.photo(visible[1]).unwrap().rating, 0);
        invoke(&mut h, "panel.crop", json!({}));
        assert!(!active(&h.app));
        h.request("ui.key", json!({"key": "X"}), TIMEOUT);
        assert_eq!(h.app.session.catalog.photo(visible[1]).unwrap().flag, Flag::None, "crop X never rejects files");
        assert!(folder.0.join("IMG_2.png").exists());
    }

    #[test]
    fn first_folder_batch_arrives_before_later_files_finish_probing() {
        let folder = Folder::new();
        for i in 0..12 {
            folder.photo(&format!("IMG_{i}.png"), i);
        }
        let mut h = headless();
        let original_probe = h.app.session.media.file_probe.clone().unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let (release, gate) = std::sync::mpsc::channel();
        let gate = std::sync::Mutex::new(gate);
        h.app.session.media.file_probe = Some(std::sync::Arc::new(move |path| {
            if count.fetch_add(1, Ordering::Relaxed) == crate::import::BATCH {
                let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
            }
            original_probe(path)
        }));
        invoke(&mut h, "file.openFolder", json!({"path": folder.0.to_string_lossy()}));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while h.app.session.catalog.len() < crate::import::BATCH && std::time::Instant::now() < deadline {
            h.step();
            std::thread::sleep(Duration::from_millis(5));
        }
        let first_count = h.app.session.catalog.len();
        let first_names: Vec<_> =
            h.app.session.visible_cloned().iter().map(|id| h.app.session.catalog.photo(*id).unwrap().file_name.clone()).collect();
        let first_active = h.app.session.active();
        let still_importing = h.app.import.is_some();
        let discovery_undo = h.app.session.undo.len();
        if let Some(id) = first_active {
            invoke(&mut h, "photo.rate", json!({"ids": [id.0], "rating": 4}));
            // Real edits can be undone while later files are still being discovered.
            invoke(&mut h, "edit.undo", json!({}));
        }
        let _ = release.send(());
        assert_eq!(first_count, crate::import::BATCH, "first batch must be usable before later probes finish");
        assert_eq!(
            first_names,
            (0..crate::import::BATCH).map(|i| format!("IMG_{i}.png")).collect::<Vec<_>>(),
            "stream batches follow natural filename order"
        );
        assert!(first_active.is_some());
        assert!(still_importing);
        assert!(h.settle(TIMEOUT));
        assert_eq!(h.app.session.catalog.len(), 12);
        assert_eq!(discovery_undo, 0, "folder discoveries are not import undo steps");
        assert_eq!(h.app.session.redo.len(), 1, "later discoveries preserve the user's rating redo");
        invoke(&mut h, "edit.redo", json!({}));
        assert_eq!(h.app.session.catalog.photo(first_active.unwrap()).unwrap().rating, 4);
        h.request("ui.key", json!({"key": "Z", "ctrl": true}), TIMEOUT);
        assert_eq!(h.app.session.catalog.photo(first_active.unwrap()).unwrap().rating, 0);
        assert_eq!(h.app.session.catalog.len(), 12, "rating undo never removes discovered photos");
    }

    #[test]
    fn positional_file_opens_parent_selects_file_and_refresh_keeps_editor_state() {
        let folder = Folder::new();
        folder.photo("IMG_2.png", 2);
        folder.photo("IMG_1.png", 1);
        let mut h = headless();
        invoke(&mut h, "file.openPaths", json!({"paths": [folder.0.join("IMG_2.png").to_string_lossy()]}));
        assert!(h.settle(TIMEOUT));
        let active = h.app.session.active().unwrap();
        assert_eq!(h.app.session.catalog.photo(active).unwrap().file_name, "IMG_2.png");
        assert!(
            h.app
                .ui
                .local_roots
                .iter()
                .any(|root| lightcraft_catalog::query::folder_key(&folder.0.to_string_lossy()) == lightcraft_catalog::query::folder_key(root)),
            "file parent is available in Local"
        );
        invoke(&mut h, "folder.cullMode", json!({"enabled": false}));
        h.app.ui.zoom = Zoom::Percent(100);
        invoke(&mut h, "folder.refresh", json!({}));
        assert!(h.settle(TIMEOUT));
        assert_eq!(h.app.ui.right, RightPanel::Edit);
        assert_eq!(h.app.ui.zoom, Zoom::Percent(100));
        assert_eq!(h.app.session.active(), Some(active));
    }

    #[test]
    fn recursive_scan_excludes_rejected_processed_and_hidden_outputs() {
        let folder = Folder::new();
        folder.photo("original.png", 1);
        for name in ["Rejected", "Processed", "LightCraft Exports", ".lightcraft", "shoot"] {
            let sub = folder.0.join(name);
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("photo.png"), [0u8]).unwrap();
        }
        let found = scan_paths(&folder.0.to_string_lossy(), true).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|path| path.ends_with("shoot/photo.png")));
        assert_eq!(scan_paths(&folder.0.join("Processed").to_string_lossy(), true).unwrap().len(), 1, "explicit output-folder browse works");
    }

    #[test]
    fn culling_fit_and_filmstrip_use_camera_previews_until_pixel_inspection() {
        use lightcraft_catalog::{MediaKind, Op, Photo, Source};
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let count = reads.clone();
        let mut h = headless();
        h.app.session.media.file_loader = Some(std::sync::Arc::new(move |_, _| {
            count.fetch_add(1, Ordering::Relaxed);
            Err("mock full-resolution decoder".into())
        }));
        h.app.session.media.preview_loader = Some(std::sync::Arc::new(|_, _| Some(Rgba8::from_fn(150, 100, |x, y| [x as u8, y as u8, 60, 255]))));
        let mut photo = Photo::new(PhotoId(1), Source::File { path: "/mock-folder/IMG_1.ARW".into() }, "IMG_1.ARW", "ARW", 6000, 4000, "2026-01-01");
        photo.kind = MediaKind::Raw;
        photo.develop = std::sync::Arc::new(photo.camera_defaults());
        photo.local = true;
        h.app.session.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        entered(&mut h.app, "/mock-folder");
        h.app.session.browse = Some(lightcraft_engine::Browse { path: "/mock-folder".into(), subfolders: false });
        h.app.session.source = LibrarySource::Folder;
        assert!(h.settle(TIMEOUT));
        assert_eq!(h.app.loupe_shown, Some((PhotoId(1), "embedded")));
        assert_eq!(reads.load(Ordering::Relaxed), 0, "culling renderer defers RAW development after metadata ingestion");
        screenshot(&mut h, "folder-camera-preview.png");
        let built = h.app.renderer.thumb_jobs_built;
        for _ in 0..10 {
            h.step();
        }
        assert_eq!(h.app.renderer.thumb_jobs_built, built, "embedded filmstrip previews remain cached");
        // Retain an earlier small rendered texture while the full RAW job is pending. Its
        // requested native dimensions must not make this stand-in claim full-resolution detail.
        let pixels = std::sync::Arc::new(egui::ColorImage::filled([150, 100], egui::Color32::DARK_GRAY));
        let texture = h.view.ctx.load_texture("cached-fit-raw", (*pixels).clone(), egui::TextureOptions::LINEAR);
        h.app.renderer.textures.insert(
            crate::render::Slot::Main,
            crate::render::Tex {
                key: 0,
                photo: PhotoId(1),
                tex: texture,
                size: [150, 100],
                histogram: None,
                ms: 0.0,
                quick: None,
                pixels: Some(pixels),
            },
        );
        h.request("ui.key", json!({"key": "Z"}), TIMEOUT);
        assert!(h.settle(TIMEOUT));
        assert!(reads.load(Ordering::Relaxed) > 0, "100% inspection asks the RAW decoder for native pixels");
        assert_eq!(h.app.renderer.textures.get(&crate::render::Slot::Main).unwrap().size, [150, 100]);
        screenshot(&mut h, "folder-raw-inspection-loading.png");
    }

    #[test]
    fn physical_100_percent_is_independent_of_display_scale() {
        let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        for scale in [1.0, 1.5, 2.0] {
            let rect = crate::panels::detail::fit_rect(area, 1.5, Zoom::Percent(100), [6000, 4000], scale, (0.5, 0.5));
            assert!((rect.width() * scale - 6000.0).abs() < 0.1);
            assert!((rect.height() * scale - 4000.0).abs() < 0.1);
        }
    }

    #[test]
    fn file_rejection_shortcut_and_ctrl_z_restore_complete_group() {
        // The folder UI lands before the engine culling worker. This becomes an end-to-end
        // regression in the integrated build; the engine worker separately tests file safety.
        if lightcraft_engine::find_command("cull.reject").is_none() {
            return;
        }
        let folder = Folder::new();
        let original = folder.photo("IMG_1.png", 1);
        folder.photo("IMG_2.png", 2);
        std::fs::write(folder.0.join("IMG_1.png.dop"), "opaque DxO sidecar").unwrap();
        let mut h = headless();
        invoke(&mut h, "file.openFolder", json!({"path": folder.0.to_string_lossy()}));
        assert!(h.settle(TIMEOUT));
        let id = h.app.session.active().unwrap();
        h.request("ui.key", json!({"key": "X"}), TIMEOUT);
        assert!(!folder.0.join("IMG_1.png").exists());
        assert_eq!(std::fs::read(folder.0.join("Rejected/IMG_1.png")).unwrap(), original);
        assert!(folder.0.join("Rejected/IMG_1.png.dop").exists());
        assert_ne!(h.app.session.active(), Some(id));
        let remaining = h.app.session.active().unwrap();
        // Mouse/API star ratings and Develop edits also precede the earlier durable file Undo.
        invoke(&mut h, "photo.rate", json!({"ids": [remaining.0], "rating": 4}));
        invoke(&mut h, "folder.cullMode", json!({"enabled": false}));
        invoke(&mut h, "develop.set", json!({"control": "light.exposure", "value": 1.0}));
        invoke(&mut h, "folder.cullMode", json!({"enabled": true}));
        h.request("ui.key", json!({"key": "Z", "ctrl": true}), TIMEOUT);
        assert_eq!(lightcraft_develop::controls::get(&h.app.session.catalog.photo(remaining).unwrap().develop, "light.exposure"), Some(0.0));
        assert!(folder.0.join("Rejected/IMG_1.png").exists(), "latest Develop edit is undone first");
        h.request("ui.key", json!({"key": "Z", "ctrl": true}), TIMEOUT);
        assert_eq!(h.app.session.catalog.photo(remaining).unwrap().rating, 0);
        assert!(folder.0.join("Rejected/IMG_1.png").exists(), "latest star rating is undone second");
        h.request("ui.key", json!({"key": "Z", "ctrl": true}), TIMEOUT);
        assert_eq!(std::fs::read(folder.0.join("IMG_1.png")).unwrap(), original);
        assert!(folder.0.join("IMG_1.png.dop").exists());
        assert_eq!(h.app.session.active(), Some(PhotoId(id.0)));
    }
}
