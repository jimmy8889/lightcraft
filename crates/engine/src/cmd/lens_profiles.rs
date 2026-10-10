//! Exact measured lens corrections. No original files are written; coefficients are stored in
//! ordinary undoable develop settings. The source decoder independently verifies capture identity.

use super::{CommandSpec, always, bad, cmd, has_active};
use crate::{Result, Session, media::SourceLevel};
use lightcraft_develop::lens_profiles::{self, CalibratedLens, LensProfile, same_measurement};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Params {
    crop_factor: Option<f64>,
    focus_m: Option<f64>,
    profile_data: Option<LensProfile>,
    distortion: Option<bool>,
    tca: Option<bool>,
    vignetting: Option<bool>,
}

fn params(p: &Value, c: &str) -> Result<Params> {
    let p: Params = serde_json::from_value(p.clone()).map_err(|e| bad(c, e.to_string()))?;
    if p.crop_factor.is_some_and(|v| !v.is_finite() || !(0.5..=10.0).contains(&v))
        || p.focus_m.is_some_and(|v| !v.is_finite() || !(0.01..=1e6).contains(&v))
    {
        return Err(bad(c, "invalid crop_factor or focus_m"));
    }
    if let Some(profile) = &p.profile_data {
        profile.validate().map_err(|e| bad(c, e))?;
    }
    Ok(p)
}

fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    resolve(s, p, false)
}
fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    resolve(s, p, true)
}

fn resolve(s: &mut Session, p: &Value, apply: bool) -> Result<Value> {
    let c = if apply { "optics.applyProfile" } else { "optics.inspectProfile" };
    let params = params(p, c)?;
    let id = s.active().ok_or_else(|| bad(c, "no active photo"))?;
    let photo = s.catalog.photo(id).ok_or_else(|| bad(c, "no photo"))?.clone();
    let profiles = if let Some(p) = params.profile_data { vec![p] } else { lens_profiles::bundled().map_err(|e| bad(c, e))? };
    let matches: Vec<_> = profiles.iter().filter(|p| p.matches(&photo.meta.lens)).collect();
    let profile = if matches.len() == 1 { matches.first().copied() } else { None };
    let key = lens_profiles::capture_identity(
        &photo.meta.camera,
        &photo.meta.lens,
        photo.meta.focal_mm,
        photo.meta.aperture,
        photo.content_hash.as_deref(),
    );
    let mut reason = if matches.is_empty() {
        Some("No exact lens alias in the selected database".to_string())
    } else if matches.len() > 1 {
        Some("Ambiguous lens alias; no profile selected".into())
    } else {
        None
    };
    if let Some(profile) = profile {
        let maker = photo.meta.camera.split_whitespace().next().unwrap_or_default();
        reason = reason.or_else(|| {
            if !maker.eq_ignore_ascii_case(&profile.maker) {
                Some("Camera manufacturer does not match profile".into())
            } else if !photo.develops_raw() {
                Some("Source is a rendered image or camera preview".into())
            } else if photo.embedded_lens.is_some() {
                Some("Embedded lens correction takes precedence".into())
            } else if params.crop_factor.is_none() {
                Some("Capture crop factor is unknown; specify crop_factor explicitly".into())
            } else if !params.crop_factor.is_some_and(|v| same_measurement(v, profile.crop_factor)) {
                Some("Capture crop factor does not match the measured profile".into())
            } else if photo.width == 0
                || photo.height == 0
                || !same_measurement(photo.width.max(photo.height) as f64 / photo.width.min(photo.height) as f64, profile.aspect_ratio)
            {
                Some("Capture aspect ratio does not match the measured profile".into())
            } else if !photo.meta.focal_mm.is_some_and(|f| f.is_finite() && f > 0.0) {
                Some("Focal length is unknown".into())
            } else {
                None
            }
        });
    }
    reason = reason.or_else(|| key.is_none().then(|| "Capture metadata/content identity is missing; reload source metadata".to_string()));
    // Loading verifies capture metadata against the same bytes used to decode pixels. Never
    // assume catalog metadata describes an externally replaced file or an offline smart preview.
    if reason.is_none() {
        if let Err(e) = s.source_now(id, SourceLevel::Thumb) {
            reason = Some(format!("Source verification failed: {e}"));
        } else {
            let info = s.source_info(id);
            if !info.raw || info.calibration_key != key || key.is_none() {
                reason = Some("Actual decoded source identity is unknown or differs from the catalog; reload source metadata".into());
            }
        }
    }
    let mut calibration = CalibratedLens {
        capture_key: key.unwrap_or(lens_profiles::CaptureIdentity { metadata: 0, content: [0, 0] }),
        crop_factor: params.crop_factor.unwrap_or(1.0),
        aspect_ratio: profile.map(|p| p.aspect_ratio).unwrap_or(1.5),
        distortion: None,
        tca: None,
        vignetting: None,
    };
    let focal = photo.meta.focal_mm.map(f64::from);
    let aperture = photo.meta.aperture.map(f64::from);
    let mut available = json!({});
    if let Some(profile) = profile {
        if reason.is_none() {
            calibration.distortion = profile.distortion.iter().find(|r| focal.is_some_and(|f| same_measurement(f, r.focal_mm))).map(|r| r.model);
            calibration.tca = profile.tca.iter().find(|r| focal.is_some_and(|f| same_measurement(f, r.focal_mm))).map(|r| r.model);
            calibration.vignetting = profile
                .vignetting
                .iter()
                .find(|r| {
                    focal.is_some_and(|f| same_measurement(f, r.focal_mm))
                        && aperture.is_some_and(|a| same_measurement(a, r.aperture))
                        && params.focus_m.is_some_and(|d| same_measurement(d, r.focus_m))
                })
                .map(|r| r.k);
        }
        available = json!({
            "distortion":{"supported":calibration.distortion.is_some(),"reason":reason.clone().or_else(||calibration.distortion.is_none().then(||"No exact focal calibration row".to_string()))},
            "tca":{"supported":calibration.tca.is_some(),"reason":reason.clone().or_else(||calibration.tca.is_none().then(||"No exact focal TCA calibration row".to_string()))},
            "vignetting":{"supported":calibration.vignetting.is_some(),"reason":reason.clone().or_else(||calibration.vignetting.is_none().then(|| if params.focus_m.is_none() {"Focus distance is unknown; specify focus_m".to_string()} else {"No exact focal/aperture/focus calibration row".to_string()}))}
        });
    }
    if params.distortion == Some(false) {
        calibration.distortion = None;
    }
    if params.tca == Some(false) {
        calibration.tca = None;
    }
    if params.vignetting == Some(false) {
        calibration.vignetting = None;
    }
    let effects =
        json!({"distortion":calibration.distortion.is_some(),"tca":calibration.tca.is_some(),"vignetting":calibration.vignetting.is_some()});
    if apply {
        if let Some(reason) = &reason {
            return Err(bad(c, reason));
        }
        if !calibration.valid() || (calibration.distortion.is_none() && calibration.tca.is_none() && calibration.vignetting.is_none()) {
            return Err(bad(c, "No supported requested correction at these exact capture settings"));
        }
        let mut settings = (*photo.develop).clone();
        settings.optics.calibration = Some(calibration);
        settings.optics.lens_profile = true;
        settings.set_section_enabled("optics", true);
        s.set_develop(id, settings, "Measured Lens Corrections")?;
    }
    Ok(
        json!({"photo":id.0,"profile":profile.map(|p|&p.id),"lens":photo.meta.lens,"focal_mm":focal,"aperture":aperture,"crop_factor":params.crop_factor,"focus_m":params.focus_m,"reason":reason,"available":available,"tca_quality":"experimental working-RGB approximation; sensor-channel correction remains unsupported","effects":effects,"applied":apply}),
    )
}

fn profiles(_: &mut Session, p: &Value) -> Result<Value> {
    if !p.as_object().is_some_and(|o| o.is_empty()) {
        return Err(bad("optics.profiles", "expected {}"));
    }
    Ok(
        json!({"profiles":lens_profiles::bundled().map_err(|e|bad("optics.profiles",e))?,"matching":"exact alias, focal, capture crop/aspect; exact aperture/focus for vignetting; no interpolation"}),
    )
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "optics.profiles","Measured Lens Profiles",[],None,"{} — bundled calibrated Sony E lenses and measured rows",always,profiles),
        cmd!(query "optics.inspectProfile","Inspect Measured Lens Corrections",[],None,"{crop_factor?, focus_m?, profile_data?: LensProfile, distortion?, tca?, vignetting?} — exact capture matching and per-effect availability",has_active,inspect),
        cmd!(
            "optics.applyProfile",
            "Apply Measured Lens Corrections",
            [],
            None,
            "{crop_factor, focus_m?, profile_data?: LensProfile, distortion?, tca?, vignetting?} — apply available requested corrections; report unsupported effects",
            has_active,
            apply
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Selection, media::DecodedSource};
    use lightcraft_catalog::{Op, Photo, PhotoId, Source};
    use lightcraft_pipeline::SourceInfo;
    use lightcraft_raster::Rgb32f;
    use std::sync::Arc;

    fn fixture() -> (Session, PhotoId) {
        let mut s = Session::new();
        let id = PhotoId(1);
        let mut photo = Photo::new(id, Source::File { path: "synthetic.arw".into() }, "synthetic.arw", "ARW", 6000, 4000, "");
        photo.kind = lightcraft_catalog::MediaKind::Raw;
        photo.meta.camera = "Sony ILCE-7M4".into();
        photo.meta.lens = "FE 50mm f/1.8".into();
        photo.meta.focal_mm = Some(50.0);
        photo.meta.aperture = Some(4.0);
        photo.content_hash = Some(lightcraft_preview::hash_bytes(b"original fixture").to_string());
        let info = crate::media::source_info(&photo);
        s.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        s.selection = Selection::single(id);
        s.media.file_loader = Some(Arc::new(move |_, _| Ok((Rgb32f::filled(60, 40, [0.2; 3]), info.clone()))));
        (s, id)
    }
    #[test]
    fn lens_profiles_apply_available_effects_without_guessing_focus_and_undo() {
        let (mut s, id) = fixture();
        let r = s.execute("optics.applyProfile", &json!({"crop_factor":1})).unwrap();
        assert_eq!(r["effects"], json!({"distortion":true,"tca":true,"vignetting":false}));
        assert!(r["available"]["vignetting"]["reason"].as_str().unwrap().contains("Focus"));
        assert!(s.develop_of(id).unwrap().optics.calibration.is_some());
        s.execute("edit.undo", &json!({})).unwrap();
        assert!(s.develop_of(id).unwrap().optics.calibration.is_none());
        s.execute("edit.redo", &json!({})).unwrap();
        assert!(s.develop_of(id).unwrap().optics.calibration.is_some());
        let r = s.execute("optics.applyProfile", &json!({"crop_factor":1,"focus_m":10})).unwrap();
        assert_eq!(r["effects"]["vignetting"], true);
    }
    #[test]
    fn lens_profiles_refuse_unknown_crop_wrong_aspect_and_nonexact_rows() {
        let (mut s, _) = fixture();
        for p in [json!({}), json!({"crop_factor":1.534}), json!({"crop_factor":1,"unexpected":true}), json!({"crop_factor":"1"})] {
            assert!(s.execute("optics.applyProfile", &p).is_err());
        }
        let r = s.execute("optics.inspectProfile", &json!({"crop_factor":1,"focus_m":9})).unwrap();
        assert_eq!(r["effects"]["distortion"], true);
        assert_eq!(r["effects"]["vignetting"], false);
        let mut photo = s.catalog.photo(PhotoId(1)).unwrap().as_ref().clone();
        photo.width = 6000;
        photo.height = 3375;
        s.catalog.apply(Op::RemovePhoto { id: photo.id }).unwrap();
        s.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        assert!(s.execute("optics.applyProfile", &json!({"crop_factor":1})).is_err());
    }
    #[test]
    fn lens_profiles_stale_catalog_and_unverified_source_do_not_apply() {
        let (mut s, id) = fixture();
        let actual = SourceInfo { raw: true, calibration_key: None, ..Default::default() };
        let loaded = actual.clone();
        s.media.file_loader = Some(Arc::new(move |_, _| Ok((Rgb32f::filled(60, 40, [0.2; 3]), loaded.clone()))));
        assert!(s.execute("optics.applyProfile", &json!({"crop_factor":1})).is_err());
        assert!(s.develop_of(id).unwrap().optics.calibration.is_none());
        let (mut s, id) = fixture();
        let mut actual = crate::media::source_info(s.catalog.photo(id).unwrap());
        actual.calibration_key.as_mut().unwrap().content[0] ^= 1; // external byte-stream replacement, identical EXIF
        let loaded = actual.clone();
        s.media.file_loader = Some(Arc::new(move |_, _| Ok((Rgb32f::filled(60, 40, [0.2; 3]), loaded.clone()))));
        assert!(s.execute("optics.applyProfile", &json!({"crop_factor":1})).is_err());
        let header = crate::media::source_info(s.catalog.photo(id).unwrap());
        assert!(DecodedSource::new(Arc::new(Rgb32f::new(3, 2)), None).info_or(header.clone()).calibration_key.is_none());
        assert!(DecodedSource::new(Arc::new(Rgb32f::new(3, 2)), Some(actual)).info_or(header).calibration_key.is_none());
    }
    #[test]
    fn lens_profiles_decoder_uses_actual_bytes_and_metadata() {
        let meta = lightcraft_meta::Metadata {
            make: Some("Sony".into()),
            model: Some("ILCE-7M4".into()),
            lens_model: Some("FE 50mm f/1.8".into()),
            focal_length: Some(50.0),
            f_number: Some(4.0),
            ..Default::default()
        };
        let bytes = crate::tests_xmp::synthetic_dng_with(None, meta.clone());
        let (_, actual) = crate::files::load_bytes(&bytes, 64).unwrap();
        let expected = lens_profiles::capture_identity(
            "Sony ILCE-7M4",
            "FE 50mm f/1.8",
            Some(50.0),
            Some(4.0),
            Some(&lightcraft_preview::hash_bytes(&bytes).to_string()),
        );
        assert_eq!(actual.calibration_key, expected);
        assert!(expected.is_some());
        let mut changed = bytes.clone();
        changed.push(42); // valid TIFF padding; unchanged EXIF, different file
        let (_, other) = crate::files::load_bytes(&changed, 64).unwrap();
        assert_ne!(other.calibration_key, expected);
        let mut meta = meta;
        meta.lens_model = Some("FE 35mm f/1.8".into());
        meta.focal_length = Some(35.0);
        let replacement = crate::tests_xmp::synthetic_dng_with(None, meta);
        let (_, other) = crate::files::load_bytes(&replacement, 64).unwrap();
        assert_ne!(other.calibration_key, expected);
        let header = SourceInfo { calibration_key: expected, ..actual };
        assert!(DecodedSource::new(Arc::new(Rgb32f::new(3, 2)), Some(other)).info_or(header).calibration_key.is_none());
    }

    #[test]
    fn lens_profiles_user_data_is_exact_and_invalid_data_refused() {
        let (mut s, _) = fixture();
        let mut profile = lens_profiles::bundled().unwrap().pop().unwrap();
        profile.id = "own-calibration".into();
        let r = s.execute("optics.applyProfile", &json!({"crop_factor":1,"profile_data":profile})).unwrap();
        assert_eq!(r["profile"], "own-calibration");
        profile.distortion.push(profile.distortion[0].clone());
        assert!(s.execute("optics.applyProfile", &json!({"crop_factor":1,"profile_data":profile})).is_err());
    }
    #[test]
    fn lens_profiles_copy_sync_preset_and_metadata_changes_stay_bound_to_source() {
        let (mut s, id) = fixture();
        s.execute("optics.applyProfile", &json!({"crop_factor":1,"focus_m":10})).unwrap();
        let mut photo = s.catalog.photo(id).unwrap().as_ref().clone();
        photo.id = PhotoId(2);
        photo.content_hash = Some(lightcraft_preview::hash_bytes(b"different photo").to_string());
        photo.meta.lens = "FE 35mm f/1.8".into();
        photo.meta.focal_mm = Some(35.0);
        photo.develop = Arc::default();
        s.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        s.execute("develop.copy", &json!({})).unwrap();
        s.execute("library.select", &json!({"ids":[2],"active":2})).unwrap();
        s.execute("develop.paste", &json!({})).unwrap();
        let src = Rgb32f::filled(60, 40, [0.2; 3]);
        let unapplied = |s: &Session, id: PhotoId| {
            let info = crate::media::source_info(s.catalog.photo(id).unwrap());
            assert!(lightcraft_pipeline::frame_for(&src, &info, &s.develop_of(id).unwrap(), true).warp.is_none());
        };
        unapplied(&s, PhotoId(2));
        s.execute("library.select", &json!({"ids":[1],"active":1})).unwrap();
        let preset = s.execute("preset.create", &json!({"name":"Measured","groups":["optics"]})).unwrap()["id"].clone();
        s.execute("library.select", &json!({"ids":[2],"active":2})).unwrap();
        s.execute("preset.apply", &json!({"id":preset,"amount":100})).unwrap();
        unapplied(&s, PhotoId(2));
        // Auto-sync distributes settings too, but every other source's identity guard still applies.
        for id in [id, PhotoId(2)] {
            s.catalog.apply(Op::SetDevelop { id, settings: Arc::default(), label: "Reset fixture".into(), edited: None }).unwrap();
        }
        s.execute("library.select", &json!({"ids":[1,2],"active":1})).unwrap();
        s.auto_sync = true;
        s.execute("optics.applyProfile", &json!({"crop_factor":1,"focus_m":1000})).unwrap();
        assert!(s.develop_of(PhotoId(2)).unwrap().optics.calibration.is_some());
        unapplied(&s, PhotoId(2));
        let mut original = s.catalog.photo(id).unwrap().as_ref().clone();
        original.meta.camera = "Sony ILCE-7M3".into();
        s.catalog.apply(Op::RemovePhoto { id: original.id }).unwrap();
        s.catalog.apply(Op::AddPhoto { photo: Box::new(original) }).unwrap();
        unapplied(&s, id);
    }
}
