//! Source-stage RAW processing. The setting is non-destructive; a separate command writes a
//! cleaned DNG, never the original. Prepared jobs are Send and run on a host worker thread.

use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};

use lightcraft_catalog::{MediaKind, Op, PhotoId, Source};
use lightcraft_raw::{
    RawImage,
    color::CameraTransform,
    denoise::{Options, Report},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CommandSpec, bad, cmd, has_selection};
use crate::{Result, Session};

const MAX_SOURCE_BYTES: usize = 1 << 30;
const MAX_INTERPRETATION_BYTES: usize = 256 << 10;

/// LightCraft's file-local colour estimate travels separately from sensor samples. These
/// private XMP fields preserve our rendering on reopening; they are not a camera calibration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SourceInterpretation {
    version: u32,
    pub transform: CameraTransform,
    pub look: Option<crate::camera_preview::CameraLook>,
    pub relative_wb: bool,
}

impl SourceInterpretation {
    fn valid(&self) -> bool {
        // CameraTone's custom Deserialize already validates finite, monotone knots and
        // finite chroma in 0..4; its private fields cannot bypass those constructors.
        let t = &self.transform;
        self.version == 1
            && t.matrix.0.iter().flatten().all(|v| v.is_finite() && v.abs() < 64.0)
            && t.wb.iter().all(|v| v.is_finite() && *v > 0.0 && *v <= 64.0)
            && t.white_xy.x.is_finite()
            && t.white_xy.y.is_finite()
            && t.white_xy.x > 0.0
            && t.white_xy.y > 0.0
            && t.white_xy.x + t.white_xy.y < 1.0
            && t.baseline_exposure.is_finite()
            && t.baseline_exposure.abs() < 32.0
            && self.look.as_ref().is_none_or(|look| {
                look.matrix.0.iter().flatten().all(|v| v.is_finite() && v.abs() < 64.0)
                    && look.hue_sat.as_ref().is_none_or(|table| {
                        let count = table.hue_divisions.checked_mul(table.sat_divisions).and_then(|n| n.checked_mul(table.val_divisions));
                        table.hue_divisions > 0
                            && table.sat_divisions >= 2
                            && table.val_divisions > 0
                            && count == Some(table.data.len())
                            && table.data.len() <= 4096
                            && table.data.iter().flatten().all(|v| v.is_finite() && v.abs() <= 360.0)
                    })
            })
    }
}

/// Obtain exactly the same interpretation for live development and cleaned-DNG readback.
pub(crate) fn source_interpretation(raw: &RawImage, bytes: &[u8]) -> SourceInterpretation {
    if raw.format == lightcraft_raw::RawFormat::Dng
        && let Some(packet) = lightcraft_raw::embedded_xmp(bytes)
        && packet.len() <= 16 << 20
        && let Ok(xmp) = lightcraft_meta::parse_xmp(&packet)
        && let Some(record) = xmp.properties.get("lc:rawSourceInterpretation").and_then(|v| v.first())
        && record.len() <= MAX_INTERPRETATION_BYTES
        && let Ok(interpretation) = serde_json::from_str::<SourceInterpretation>(record)
        && interpretation.valid()
    {
        return interpretation;
    }
    let xy = lightcraft_raw::color::as_shot_white_xy(raw);
    let transform = lightcraft_raw::color::camera_transform(raw, xy);
    SourceInterpretation {
        version: 1,
        look: crate::camera_preview::fit_preview(raw, bytes, &transform),
        relative_wb: crate::camera_preview::file_local_look(raw.format) && transform.matrix_is_fallback,
        transform,
    }
}

/// A processed DNG re-export retains its original source interpretation and provenance.
pub(crate) fn preserve_processing_packet(bytes: &[u8], packet: String) -> std::result::Result<String, String> {
    let Some(original) = lightcraft_raw::embedded_xmp(bytes) else { return Ok(packet) };
    let xmp = lightcraft_meta::parse_xmp(&original).map_err(|e| e.to_string())?;
    let fields: Vec<(&str, &str)> = ["rawSourceInterpretation", "rawProcessing"]
        .iter()
        .filter_map(|name| xmp.properties.get(&format!("lc:{name}")).and_then(|v| v.first()).map(|v| (*name, v.as_str())))
        .collect();
    if fields.is_empty() {
        return Ok(packet);
    }
    let fresh = lightcraft_meta::write_xmp_lc(&Default::default(), &fields);
    lightcraft_meta::merge_xmp(
        &packet,
        &fresh,
        lightcraft_meta::MergeRules { owned: &["lc:rawSourceInterpretation", "lc:rawProcessing"], owned_if_present: &[] },
    )
    .map_err(|e| e.to_string())
}

/// Frontends may default absent parameters to an object, but must preserve explicitly
/// malformed RAW parameters so they cannot act on the current selection by accident.
pub fn requires_object_params(id: &str) -> bool {
    matches!(id, "photo.rawDenoise" | "photo.exportProcessedDng" | "folder.exportProcessedDng")
}

/// Strict local targets: explicit malformed input must never fall back to the selection.
pub fn checked_targets(s: &Session, p: &Value, command: &str) -> Result<Vec<PhotoId>> {
    const MAX_TARGETS: usize = 10_000;
    let object = p.as_object().ok_or_else(|| bad(command, "params must be an object"))?;
    if object.contains_key("id") && object.contains_key("ids") {
        return Err(bad(command, "provide id or ids, not both"));
    }
    let numeric_id =
        |value: &Value| value.as_u64().filter(|id| *id > 0).map(PhotoId).ok_or_else(|| bad(command, "photo IDs must be positive unsigned integers"));
    let ids = if let Some(value) = object.get("ids") {
        let array = value.as_array().ok_or_else(|| bad(command, "ids must be a numeric array"))?;
        if array.is_empty() || array.len() > MAX_TARGETS {
            return Err(bad(command, "ids must contain 1..10000 photos"));
        }
        array.iter().map(numeric_id).collect::<Result<Vec<_>>>()?
    } else if let Some(value) = object.get("id") {
        vec![numeric_id(value)?]
    } else {
        s.targets(p)
    };
    if ids.is_empty() || ids.len() > MAX_TARGETS {
        return Err(bad(command, "select 1..10000 photos"));
    }
    let mut seen = std::collections::HashSet::new();
    for id in &ids {
        if !seen.insert(*id) {
            return Err(bad(command, "duplicate photo IDs"));
        }
        if s.catalog.photo(*id).is_none() {
            return Err(bad(command, "unknown photo ID"));
        }
    }
    Ok(ids)
}

fn raw_denoise(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "photo.rawDenoise";
    let strength = match p.get("strength") {
        Some(v) => v.as_f64().ok_or_else(|| bad(C, "strength must be a number"))?,
        None => 50.0,
    };
    let options = Options { strength: strength as f32 };
    options.validate().map_err(|e| bad(C, e.to_string()))?;
    if !strength.is_finite() || !(0.0..=100.0).contains(&strength) {
        return Err(bad(C, "strength must be within 0..100"));
    }
    let ids = checked_targets(s, p, C)?;
    // Preflight the complete selection before changing any settings.
    for id in &ids {
        let photo = s.catalog.photo(*id).ok_or_else(|| bad(C, "unknown photo"))?;
        if photo.kind != MediaKind::Raw || photo.preview_only.is_some() || !matches!(&photo.source, Source::File { .. }) {
            return Err(bad(C, format!("{}: RAW denoise requires a supported RAW original", photo.file_name)));
        }
    }
    let mut ops = Vec::new();
    let mut changed = Vec::new();
    for id in ids {
        let mut d = (*s.develop_of(id).ok_or_else(|| bad(C, "no photo settings"))?).clone();
        d.detail.raw_denoise = strength;
        d.set_section_enabled("detail", true);
        if let Some(op) = s.develop_op(id, d, "RAW Denoise (Native)") {
            ops.push(op);
            changed.push(id.0);
        }
    }
    if !ops.is_empty() {
        s.commit("RAW Denoise (Native)", Op::Batch { ops })?;
    }
    Ok(
        json!({"changed": changed, "strength": strength, "algorithm": "native-phase-bilateral", "version": lightcraft_raw::denoise::VERSION, "ai": false}),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceStamp {
    bytes: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
}

fn source_stamp(path: &str) -> Option<SourceStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(SourceStamp {
        bytes: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        },
    })
}

/// Detached, cancellation-aware preparation for UI/CLI background processing. No catalog
/// mutation is required on completion, and original bytes are never written.
pub struct ProcessedDngJob {
    pub photo: PhotoId,
    pub source: String,
    pub output: PathBuf,
    pub cancel: Arc<AtomicBool>,
    read: Option<crate::merge::ByteReader>,
    expected_source: Option<SourceStamp>,
    options: Options,
    packet: String,
    settings: lightcraft_develop::DevelopSettings,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessedDngResult {
    pub photo: PhotoId,
    pub path: PathBuf,
    pub original: String,
    pub report: Report,
    /// Our fitted colour/tone is embedded separately; other editors use the DNG camera tags.
    pub has_lightcraft_colour: bool,
}

/// Snapshot settings and output name on the UI thread; call [`ProcessedDngJob::run`] elsewhere.
/// An explicit `path` must be a new `.dng` file; defaults to `Processed/<stem>-Clean.dng`.
pub fn prepare_processed_dng(s: &Session, id: PhotoId, path: Option<&Path>) -> std::result::Result<ProcessedDngJob, String> {
    let photo = s.catalog.photo(id).ok_or("unknown photo")?;
    let Source::File { path: source } = &photo.source else {
        return Err("processed DNG requires a RAW file".into());
    };
    if photo.kind != MediaKind::Raw || photo.preview_only.is_some() {
        return Err("processed DNG requires a supported RAW original".into());
    }
    let variant_settings = s.develop_of(id).ok_or("no photo settings")?;
    let options = crate::files::raw_denoise_options(&variant_settings);
    options.validate().map_err(|e| e.to_string())?;
    if options.strength == 0.0 {
        return Err("enable RAW Denoise before exporting a cleaned DNG".into());
    }
    let source_path = Path::new(source);
    let output = match path {
        Some(p) => p.to_path_buf(),
        None => {
            let stem = source_path.file_stem().ok_or("RAW source has no file name")?.to_string_lossy();
            source_path.parent().unwrap_or(Path::new(".")).join("Processed").join(format!("{stem}-Clean.dng"))
        }
    };
    if !output.extension().is_some_and(|e| e.eq_ignore_ascii_case("dng")) {
        return Err("processed output must have a .dng extension".into());
    }
    if source_path == output || lightcraft_catalog::safe_file::same_file(source_path, &output) {
        return Err("processed output cannot overwrite its RAW original".into());
    }
    check_output_ancestors(&output)?;
    if std::fs::symlink_metadata(&output).is_ok() {
        return Err(format!("{} already exists; choose a new name", output.display()));
    }
    let mut settings = (*variant_settings).clone();
    settings.detail.raw_denoise = 0.0; // The file contains the cleaned values; never denoise it twice.
    Ok(ProcessedDngJob {
        photo: id,
        source: source.clone(),
        output,
        cancel: Arc::new(AtomicBool::new(false)),
        read: s.media.file_bytes.clone(),
        expected_source: source_stamp(source),
        options,
        packet: crate::sidecar::sidecar_packet(photo, &s.catalog),
        settings,
    })
}

// Refuse existing symlink/junction ancestors; a Processed alias must not redirect writes.
fn check_output_ancestors(output: &Path) -> std::result::Result<(), String> {
    for parent in output.parent().into_iter().flat_map(Path::ancestors).filter(|p| !p.as_os_str().is_empty()) {
        match std::fs::symlink_metadata(parent) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(format!("{} is a symlink; choose a physical output folder", parent.display())),
            Ok(meta) if !meta.is_dir() => return Err(format!("{} is not an output folder", parent.display())),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", parent.display())),
        }
    }
    Ok(())
}

// TIFF rational tags may round; reject material black/white changes before publishing.
fn levels_match(raw: &RawImage, back: &RawImage) -> bool {
    let close = |a: f32, b: f32| a.is_finite() && b.is_finite() && (a - b).abs() <= 1e-5 * a.abs().max(1.0);
    let a = &raw.black;
    let b = &back.black;
    if a.repeat_rows != b.repeat_rows
        || a.repeat_cols != b.repeat_cols
        || a.delta_h.len() != b.delta_h.len()
        || a.delta_v.len() != b.delta_v.len()
        || a.delta_h.iter().zip(&b.delta_h).any(|(&x, &y)| !close(x, y))
        || a.delta_v.iter().zip(&b.delta_v).any(|(&x, &y)| !close(x, y))
    {
        return false;
    }
    for y in 0..a.repeat_rows {
        for x in 0..a.repeat_cols {
            for s in 0..raw.cpp {
                if !close(a.at(x, y, s, raw.cpp), b.at(x, y, s, back.cpp)) || !close(raw.white_at(s), back.white_at(s)) {
                    return false;
                }
            }
        }
    }
    true
}

impl ProcessedDngJob {
    pub fn run(self) -> std::result::Result<ProcessedDngResult, String> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("RAW processing cancelled".into());
        }
        check_output_ancestors(&self.output)?;
        if std::fs::symlink_metadata(&self.output).is_ok() {
            return Err(format!("{} already exists; choose a new name", self.output.display()));
        }
        let weight = std::fs::metadata(&self.source).map(|m| m.len().saturating_mul(6).min(usize::MAX as u64) as usize).unwrap_or(0);
        let _permit = crate::memory::work_gate().acquire(weight);
        if self.expected_source.is_some() && source_stamp(&self.source) != self.expected_source {
            return Err("RAW original changed since the job was prepared; refresh the folder before processing".into());
        }
        let bytes = match &self.read {
            Some(read) => read(&self.source)?,
            None => {
                let mut f = std::fs::File::open(&self.source).map_err(|e| format!("{}: {e}", self.source))?;
                if f.metadata().map_err(|e| e.to_string())?.len() > MAX_SOURCE_BYTES as u64 {
                    return Err("RAW source exceeds 1 GiB processing limit".into());
                }
                let mut bytes = Vec::new();
                use std::io::Read;
                (&mut f).take(MAX_SOURCE_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                bytes
            }
        };
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err("RAW source exceeds 1 GiB processing limit".into());
        }
        if self.expected_source.is_some() && source_stamp(&self.source) != self.expected_source {
            return Err("RAW original changed since the job was prepared; refresh the folder before processing".into());
        }
        crate::files::preflight_raw_processing(&bytes)?;
        let embedded_packet = lightcraft_raw::embedded_xmp(&bytes);
        let mut raw = lightcraft_raw::decode(&bytes).map_err(|e| e.to_string())?;
        let interpretation = source_interpretation(&raw, &bytes);
        drop(bytes);
        let report = lightcraft_raw::denoise::process(&mut raw, self.options, &self.cancel).map_err(|e| e.to_string())?;
        let record = serde_json::to_string(&interpretation).map_err(|e| e.to_string())?;
        let processing = serde_json::to_string(&report).map_err(|e| e.to_string())?;
        let settings = serde_json::to_string(&self.settings).map_err(|e| e.to_string())?;
        let extra = lightcraft_meta::write_xmp_lc(
            &raw.metadata,
            &[("settings", &settings), ("rawProcessing", &processing), ("rawSourceInterpretation", &record)],
        );
        let original_packet = match embedded_packet {
            Some(original) => lightcraft_meta::merge_xmp(
                &original,
                &self.packet,
                lightcraft_meta::MergeRules { owned: crate::sidecar::OWNED, owned_if_present: crate::sidecar::OWNED_IF_STATED },
            )
            .map_err(|e| e.to_string())?,
            None => self.packet,
        };
        let packet = lightcraft_meta::merge_xmp(
            &original_packet,
            &extra,
            lightcraft_meta::MergeRules { owned: &["lc:settings", "lc:rawProcessing", "lc:rawSourceInterpretation"], owned_if_present: &[] },
        )
        .map_err(|e| e.to_string())?;
        let opts = lightcraft_raw::DngWriteOptions {
            xmp: Some(packet),
            compression: lightcraft_raw::DngCompression::Deflate { tile: 256, half: false },
            ..Default::default()
        };
        let encoded = lightcraft_raw::write_dng(&raw, &opts).map_err(|e| e.to_string())?;
        let back = lightcraft_raw::decode(&encoded).map_err(|e| format!("processed DNG readback failed: {e}"))?;
        if back.data != raw.data
            || back.cfa != raw.cfa
            || back.active_area != raw.active_area
            || back.crop != raw.crop
            || back.orientation != raw.orientation
            || !levels_match(&raw, &back)
        {
            return Err("processed DNG failed sensor-data readback; no output written".into());
        }
        drop((raw, back));
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("RAW processing cancelled".into());
        }
        if self.expected_source.is_some() && source_stamp(&self.source) != self.expected_source {
            return Err("RAW original changed while processing; no output written".into());
        }
        check_output_ancestors(&self.output)?;
        if let Some(parent) = self.output.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        check_output_ancestors(&self.output)?;
        lightcraft_catalog::safe_file::write_new(&self.output, &encoded).map_err(|e| format!("{}: {e}; original kept", self.output.display()))?;
        Ok(ProcessedDngResult {
            photo: self.photo,
            path: self.output,
            original: self.source,
            report,
            has_lightcraft_colour: interpretation.look.is_some(),
        })
    }
}

fn export_processed(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "photo.exportProcessedDng";
    let ids = checked_targets(s, p, C)?;
    let path = match p.get("path") {
        Some(v) => Some(Path::new(v.as_str().ok_or_else(|| bad(C, "path must be a file name string"))?)),
        None => None,
    };
    if path.is_some() && ids.len() != 1 {
        return Err(bad(C, "an explicit path requires exactly one photo"));
    }
    let jobs = ids.into_iter().map(|id| prepare_processed_dng(s, id, path)).collect::<std::result::Result<Vec<_>, _>>().map_err(|e| bad(C, e))?;
    let mut names = std::collections::HashSet::new();
    if jobs.iter().any(|job| !names.insert(job.output.clone())) {
        return Err(bad(C, "selected RAW files have colliding output names; export individually with unique paths"));
    }
    let mut outputs = Vec::new();
    for job in jobs {
        outputs.push(job.run().map_err(|e| bad(C, e))?);
    }
    Ok(json!({"outputs": outputs}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "photo.rawDenoise",
            "RAW Denoise (Native)",
            ["Photo"],
            None,
            "{ids?, strength?: 0..100} — persistent native, non-AI RAW preprocessing before demosaic; default 50, 0 disables → {changed, strength, algorithm, version, ai:false}",
            has_selection,
            raw_denoise
        ),
        cmd!(
            "photo.exportProcessedDng",
            "Export Processed DNG",
            ["Photo"],
            None,
            "{ids?, path?:new.dng} — cleaned sensor/linear samples with edits, default Processed/<stem>-Clean.dng; requires RAW Denoise >0; never overwrites → {outputs}",
            has_selection,
            export_processed
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{SourceLevel, SourceRef};

    fn setup(tag: &str) -> (Session, PhotoId, PathBuf, Vec<u8>) {
        let dir = crate::tests_xmp::temp_dir(&format!("raw-processing-{tag}"));
        let source = dir.join("Shot.dng");
        let bytes = crate::tests_xmp::synthetic_dng_with(None, Default::default());
        let mut raw = lightcraft_raw::decode(&bytes).unwrap();
        let mut seed = 23421u32;
        if let lightcraft_raw::RawData::U16(data) = &mut raw.data {
            for v in data.iter_mut() {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = 6000 + ((seed >> 16) % 900) as u16;
            }
        }
        let bytes = lightcraft_raw::write_dng(&raw, &Default::default()).unwrap();
        std::fs::write(&source, &bytes).unwrap();
        let mut s = Session::new().with_fs();
        s.execute("library.import", &json!({"paths": [source.to_string_lossy()]})).unwrap();
        let id = s.catalog.photos().next().unwrap().id;
        s.execute("library.select", &json!({"ids": [id.0]})).unwrap();
        (s, id, dir, bytes)
    }

    #[test]
    fn command_is_non_destructive_persistent_and_undoable() {
        let (mut s, id, dir, bytes) = setup("settings");
        let result = s.execute("photo.rawDenoise", &json!({"strength": 70.0})).unwrap();
        assert_eq!(result["ai"], false);
        assert_eq!(s.catalog.photo(id).unwrap().develop.detail.raw_denoise, 70.0);
        assert_eq!(std::fs::read(dir.join("Shot.dng")).unwrap(), bytes);
        assert!(!dir.join("Processed").exists());
        let packet = crate::sidecar::sidecar_packet(s.catalog.photo(id).unwrap(), &s.catalog);
        let sc = crate::sidecar::parse_sidecar(&packet, true).unwrap();
        let Some(crate::sidecar::DevelopPatch::Full(settings)) = sc.develop else { panic!("settings missing") };
        assert_eq!(settings.detail.raw_denoise, 70.0);
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(s.catalog.photo(id).unwrap().develop.detail.raw_denoise, 0.0);
        for v in [-1.0, 100.1] {
            assert!(s.execute("photo.rawDenoise", &json!({"strength": v})).is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_cache_before_view_exports_and_stale_results_use_correct_processing() {
        let (mut s, id, dir, _) = setup("cache");
        let old = s.render_job(id, 64, 64, false, true).unwrap().run();
        s.accept(&old);
        let original = s.source_now(id, SourceLevel::Full).unwrap();
        s.execute("photo.rawDenoise", &json!({"strength": 100.0})).unwrap();
        let processed_job = s.render_job(id, 64, 64, false, true).unwrap();
        assert_ne!(old.source_key, processed_job.source_key);
        let processed = processed_job.run();
        assert!(processed.rendered.is_ok());
        s.accept(&processed);
        let clean = s.source_now(id, SourceLevel::Full).unwrap();
        assert_ne!(original.data, clean.data, "source values changed before pipeline NR");
        s.accept(&old);
        let photo = s.catalog.photo(id).unwrap().clone();
        let SourceRef::Loaded(src) = s.media.source_ref(&photo, SourceLevel::Full) else { panic!("clean cache lost") };
        assert_eq!(src.raw_processing_key, Options { strength: 100.0 }.cache_key());
        let before = s.render_job(id, 64, 64, true, true).unwrap();
        let before_source = before.source.load_source().unwrap();
        assert_eq!(before_source.raw_processing_key, 0);
        let export = s.export_job(id, 64, 64, Default::default(), Default::default()).unwrap();
        assert_eq!(export.source.load_source().unwrap().raw_processing_key, Options { strength: 100.0 }.cache_key());
        s.execute("develop.sectionEnabled", &json!({"section":"detail", "enabled":false})).unwrap();
        let disabled = s.render_job(id, 64, 64, false, true).unwrap();
        assert_eq!(disabled.source.load_source().unwrap().raw_processing_key, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn processed_export_readback_preserves_original_and_rejects_collisions() {
        let (mut s, id, dir, bytes) = setup("export");
        s.execute("photo.rawDenoise", &json!({"strength": 80.0})).unwrap();
        let prepared = prepare_processed_dng(&s, id, None).unwrap();
        // The UI prepares the job without processing or creating any output directory.
        assert!(!dir.join("Processed").exists());
        let result = std::thread::spawn(move || prepared.run()).join().unwrap().unwrap();
        assert_eq!(result.path, dir.join("Processed/Shot-Clean.dng"));
        let output = std::fs::read(&result.path).unwrap();
        let cleaned = lightcraft_raw::decode(&output).unwrap();
        let original = lightcraft_raw::decode(&bytes).unwrap();
        assert_ne!(cleaned.data, original.data);
        assert_eq!(cleaned.crop, original.crop);
        assert_eq!(cleaned.cfa, original.cfa);
        assert_eq!(cleaned.black.values, original.black.values);
        assert_eq!(cleaned.white, original.white);
        assert_eq!(std::fs::read(dir.join("Shot.dng")).unwrap(), bytes);
        let packet = lightcraft_raw::embedded_xmp(&output).unwrap();
        let sc = crate::sidecar::parse_sidecar(&packet, true).unwrap();
        let Some(crate::sidecar::DevelopPatch::Full(settings)) = sc.develop else { panic!("settings missing") };
        assert_eq!(settings.detail.raw_denoise, 0.0, "cannot double process on reimport");
        assert!(prepare_processed_dng(&s, id, None).err().unwrap().contains("already exists"));
        assert!(prepare_processed_dng(&s, id, Some(&dir.join("Shot.dng"))).is_err());
        let expected = crate::files::load_bytes_with_raw_denoise(&bytes, usize::MAX, Options { strength: 80.0 }).unwrap();
        let reopened = crate::files::load_bytes(&output, usize::MAX).unwrap();
        assert_eq!(reopened.0, expected.0, "editing reopened output and original+processing agree exactly");
        assert_eq!(reopened.1.relative_wb, expected.1.relative_wb);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prepared_cancellation_and_disk_failure_leave_original_untouched() {
        let (mut s, id, dir, bytes) = setup("cancel");
        s.execute("photo.rawDenoise", &json!({"strength": 50.0})).unwrap();
        let job = prepare_processed_dng(&s, id, None).unwrap();
        job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(job.run().unwrap_err().contains("cancelled"));
        let fault = lightcraft_catalog::safe_file::fail_writes_after(16);
        assert!(prepare_processed_dng(&s, id, None).unwrap().run().is_err());
        drop(fault);
        assert!(!dir.join("Processed/Shot-Clean.dng").exists());
        assert_eq!(std::fs::read(dir.join("Shot.dng")).unwrap(), bytes);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_private_interpretation_is_ignored() {
        let (s, id, dir, bytes) = setup("malformed-look");
        let raw = lightcraft_raw::decode(&bytes).unwrap();
        let mut interpretation = source_interpretation(&raw, &bytes);
        interpretation.transform.wb[0] = -1.0;
        assert!(!interpretation.valid());
        let record = serde_json::to_string(&interpretation).unwrap();
        let packet = lightcraft_meta::write_xmp_lc(&raw.metadata, &[("rawSourceInterpretation", &record)]);
        let encoded = lightcraft_raw::write_dng(&raw, &lightcraft_raw::DngWriteOptions { xmp: Some(packet), ..Default::default() }).unwrap();
        let back = source_interpretation(&raw, &encoded);
        assert!(back.valid());
        assert_eq!(s.catalog.photo(id).unwrap().develop.detail.raw_denoise, 0.0);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[ignore = "requires LIGHTCRAFT_SONY_CORPUS; real samples remain read-only"]
    fn sony_processed_dng_reopens_with_the_same_colour_and_denoise() {
        let dir = crate::tests_xmp::temp_dir("raw-processing-sony");
        let corpus = std::env::var("LIGHTCRAFT_SONY_CORPUS").expect("set LIGHTCRAFT_SONY_CORPUS");
        let mut paths: Vec<_> = std::fs::read_dir(corpus)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw")))
            .collect();
        paths.sort();
        assert!(!paths.is_empty());
        for path in paths {
            let bytes = std::fs::read(&path).unwrap();
            let mut session = Session::new().with_fs();
            session.execute("library.import", &json!({"paths":[path.to_string_lossy()]})).unwrap();
            let id = session.catalog.photos().next().unwrap().id;
            session.execute("library.select", &json!({"ids":[id.0]})).unwrap();
            session.execute("photo.rawDenoise", &json!({"strength":60.0})).unwrap();
            let out = dir.join(path.file_name().unwrap()).with_extension("dng");
            let result = prepare_processed_dng(&session, id, Some(&out)).unwrap().run().unwrap();
            let encoded = std::fs::read(&out).unwrap();
            let expected = crate::files::load_bytes_with_raw_denoise(&bytes, 256, Options { strength: 60.0 }).unwrap();
            let reopened = crate::files::load_bytes(&encoded, 256).unwrap();
            assert_eq!((expected.0.width, expected.0.height), (reopened.0.width, reopened.0.height));
            let error =
                expected.0.data.iter().zip(&reopened.0.data).flat_map(|(a, b)| a.iter().zip(b).map(|(x, y)| (x - y).abs())).fold(0.0f32, f32::max);
            assert!(error < 0.0002, "{} max colour difference={error}", path.display());
            assert_eq!(expected.1.relative_wb, reopened.1.relative_wb);
            assert_eq!(expected.1.camera_tone, reopened.1.camera_tone);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            eprintln!("{}: {}, fitted look={}, reopened preview max delta={error}", path.display(), result.report.mode, result.has_lightcraft_colour);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn virtual_copy_processing_and_export_use_its_independent_settings() {
        let (mut s, original_id, dir, _) = setup("variant");
        let copy = s.execute("photo.virtualCopy", &json!({})).unwrap();
        let copy_id = PhotoId(copy["ids"][0].as_u64().unwrap());
        s.execute("photo.rawDenoise", &json!({"strength":85.0})).unwrap();
        s.execute("develop.set", &json!({"control":"light.exposure","value":0.7})).unwrap();
        assert_eq!(s.catalog.photo(original_id).unwrap().develop.detail.raw_denoise, 0.0);
        let prepared = prepare_processed_dng(&s, copy_id, Some(&dir.join("Copy-Clean.dng"))).unwrap();
        assert_eq!(prepared.options.strength, 85.0);
        let result = prepared.run().unwrap();
        let packet = lightcraft_raw::embedded_xmp(&std::fs::read(result.path).unwrap()).unwrap();
        let sc = crate::sidecar::parse_sidecar(&packet, true).unwrap();
        let Some(crate::sidecar::DevelopPatch::Full(d)) = sc.develop else { panic!("settings missing") };
        assert_eq!(d.detail.raw_denoise, 0.0);
        assert_eq!(d.light.exposure, 0.7);
        let job = s.render_job(copy_id, 64, 64, false, true).unwrap();
        assert_eq!(job.source.load_source().unwrap().raw_processing_key, Options { strength: 85.0 }.cache_key());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_private_tone_is_rejected_during_deserialization() {
        let (s, id, dir, bytes) = setup("invalid-tone");
        let raw = lightcraft_raw::decode(&bytes).unwrap();
        let mut record = source_interpretation(&raw, &bytes);
        record.look = Some(crate::camera_preview::CameraLook {
            matrix: lightcraft_color::Mat3::IDENTITY,
            tone: lightcraft_pipeline::tone::CameraTone::new(std::array::from_fn(|i| [0.01 * (i + 1) as f32, 0.01 * (i + 1) as f32])).unwrap(),
            hue_sat: None,
        });
        let mut value = serde_json::to_value(&record).unwrap();
        value["look"]["tone"]["knots"][0][0] = json!(0.0);
        assert!(serde_json::from_value::<SourceInterpretation>(value).is_err());
        let mut value = serde_json::to_value(&record).unwrap();
        value["look"]["tone"]["chroma"][0] = json!(-0.1);
        assert!(serde_json::from_value::<SourceInterpretation>(value).is_err());
        assert_eq!(s.develop_of(id).unwrap().detail.raw_denoise, 0.0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn processed_output_refuses_symlink_ancestors_even_if_added_after_preparation() {
        let (mut s, id, dir, _) = setup("ancestor");
        s.execute("photo.rawDenoise", &json!({"strength":50.0})).unwrap();
        let destination = dir.join("elsewhere");
        std::fs::create_dir(&destination).unwrap();
        std::os::unix::fs::symlink(&destination, dir.join("Processed")).unwrap();
        assert!(prepare_processed_dng(&s, id, None).err().unwrap().contains("symlink"));
        std::fs::remove_file(dir.join("Processed")).unwrap();
        let job = prepare_processed_dng(&s, id, None).unwrap();
        std::os::unix::fs::symlink(&destination, dir.join("Processed")).unwrap();
        assert!(job.run().unwrap_err().contains("symlink"));
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_changed_original_is_not_processed_with_stale_catalog_settings() {
        let (mut s, id, dir, _) = setup("changed-source");
        s.execute("photo.rawDenoise", &json!({"strength":50.0})).unwrap();
        let job = prepare_processed_dng(&s, id, None).unwrap();
        let replacement = crate::tests_xmp::synthetic_dng_with(None, Default::default());
        std::fs::write(dir.join("Shot.dng"), &replacement).unwrap();
        assert!(job.run().unwrap_err().contains("changed since"));
        assert!(!dir.join("Processed").exists());
        assert_eq!(std::fs::read(dir.join("Shot.dng")).unwrap(), replacement);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn invalid_explicit_targets_never_use_selection_or_mutate_sources() {
        let (mut s, id, dir, bytes) = setup("strict-targets");
        s.execute("photo.rawDenoise", &json!({"strength":60.0})).unwrap();
        let invalid = vec![
            Value::Null,
            json!([]),
            json!("invalid"),
            json!(7),
            json!({"ids":"invalid"}),
            json!({"ids":[]}),
            json!({"ids":["invalid"]}),
            json!({"ids":[-1]}),
            json!({"ids":[1.5]}),
            json!({"ids":[0]}),
            json!({"ids":[id.0,id.0]}),
            json!({"ids":[u64::MAX]}),
            json!({"id":"invalid"}),
            json!({"id":null}),
            json!({"id":-1}),
            json!({"id":id.0,"ids":[id.0]}),
            json!({"ids":vec![id.0;10_001]}),
        ];
        for mut params in invalid {
            if let Some(object) = params.as_object_mut() {
                object.insert("strength".into(), json!(90.0));
            }
            assert!(s.execute("photo.rawDenoise", &params).is_err(), "{params}");
            assert!(s.execute("photo.exportProcessedDng", &params).is_err(), "{params}");
            assert_eq!(s.develop_of(id).unwrap().detail.raw_denoise, 60.0);
            assert_eq!(std::fs::read(dir.join("Shot.dng")).unwrap(), bytes);
            assert!(!dir.join("Processed").exists());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
