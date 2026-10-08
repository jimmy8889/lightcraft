//! Explicit capture assertions for the measured-profile engine commands. Draft inputs belong to
//! one photo only, never enter develop settings, and are never persisted or inferred from a body.

use lightcraft_catalog::PhotoId;
use lightcraft_develop::lens_profiles::CaptureIdentity;
use serde_json::{Value, json};

use crate::{
    LightcraftApp,
    theme::Tokens,
    widgets::{register, text_button},
};

#[derive(Clone, Debug)]
pub struct Draft {
    pub identity: Option<(u64, Option<CaptureIdentity>)>,
    pub crop: String,
    pub focus: String,
    pub distortion: bool,
    pub tca: bool,
    pub vignetting: bool,
    pub report: Option<Value>,
    pub profiles: Option<Value>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            identity: None,
            crop: String::new(),
            focus: String::new(),
            distortion: true,
            tca: false,
            vignetting: true,
            report: None,
            profiles: None,
        }
    }
}

impl Draft {
    fn params(&self) -> Result<Value, &'static str> {
        let mut params = json!({"distortion":self.distortion,"tca":self.tca,"vignetting":self.vignetting});
        if !self.crop.trim().is_empty() {
            let crop = self.crop.trim().parse::<f64>().map_err(|_| "Enter a numeric capture crop factor")?;
            if !crop.is_finite() || !(0.5..=10.0).contains(&crop) {
                return Err("Capture crop must be between 0.5 and 10");
            }
            params["crop_factor"] = json!(crop);
        }
        if !self.focus.trim().is_empty() {
            let focus = self.focus.trim().parse::<f64>().map_err(|_| "Enter focus distance in metres, or leave it unknown")?;
            if !focus.is_finite() || !(0.01..=1e6).contains(&focus) {
                return Err("Focus distance must be between 0.01 and 1000000 metres");
            }
            params["focus_m"] = json!(focus);
        }
        Ok(params)
    }

    fn can_apply(&self) -> bool {
        let Some(report) = &self.report else { return false };
        !self.crop.trim().is_empty()
            && self.params().is_ok()
            && report["reason"].is_null()
            && [("distortion", self.distortion), ("tca", self.tca), ("vignetting", self.vignetting)]
                .iter()
                .any(|(effect, requested)| *requested && report["available"][*effect]["supported"] == true)
    }
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(crate::i18n::tr(text)).size(11.0).color(Tokens::get(ui.ctx()).text_dim));
}

fn reason_note(ui: &mut egui::Ui, reason: &str) {
    note(
        ui,
        match reason {
            "Focus distance is unknown; specify focus_m" => "Enter the actual focus distance for measured vignetting.",
            "Capture crop factor is unknown; specify crop_factor explicitly" => "Enter the actual capture crop factor above.",
            "No exact focal/aperture/focus calibration row" => "No measured vignetting row matches this focal length, aperture and focus distance.",
            "No exact focal calibration row" => "No measured distortion row matches this focal length.",
            "No exact focal TCA calibration row" => "No measured colour-fringe row matches this focal length.",
            other => other,
        },
    );
}

pub fn show(app: &mut LightcraftApp, ui: &mut egui::Ui, id: PhotoId) {
    let identity = Some((id.0, app.session.catalog.photo(id).and_then(|p| lightcraft_engine::media::source_info(p).calibration_key)));
    let mut draft = std::mem::take(&mut app.ui.measured_lens);
    if draft.identity != identity {
        draft = Draft { identity, ..Default::default() };
    }
    if draft.profiles.is_none() {
        draft.profiles = app.run("optics.profiles", json!({})).ok();
        // No crop assertion yet: inspection can identify the lens, but cannot apply corrections.
        draft.report = app.run("optics.inspectProfile", json!({})).ok();
    }
    egui::Frame::NONE.inner_margin(egui::Margin { left: 24, right: 22, top: 4, bottom: 8 }).show(ui, |ui| {
        ui.label(egui::RichText::new(crate::i18n::tr("Measured profiles")).strong());
        if app.session.develop_of(id).is_some_and(|d| d.optics.calibration.is_some()) {
            note(ui, "Measured settings are saved for this photo. Enter capture assertions to replace them.");
        }
        let lens = app.session.catalog.photo(id).map(|p| p.meta.lens.as_str()).unwrap_or("");
        note(ui, if lens.is_empty() { "Lens identity is unknown" } else { lens });
        egui::CollapsingHeader::new(crate::i18n::tr("Bundled lens coverage")).id_salt("measured-coverage").show(ui, |ui| {
            if let Some(profiles) = draft.profiles.as_ref().and_then(|v| v["profiles"].as_array()) {
                for profile in profiles {
                    if let Some(alias) = profile["aliases"].as_array().and_then(|v| v.first()).and_then(Value::as_str) {
                        note(ui, alias);
                    }
                }
            }
            note(ui, "Exact measured rows only; no interpolation.");
        });
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(crate::i18n::tr("Crop factor"));
            let r = ui.add(egui::TextEdit::singleline(&mut draft.crop).desired_width(68.0).hint_text(crate::i18n::tr("Required")));
            register(ui.ctx(), "input:measured-crop", r.rect);
            changed |= r.changed();
        });
        ui.horizontal(|ui| {
            ui.label(crate::i18n::tr("Focus (m)"));
            let r = ui.add(egui::TextEdit::singleline(&mut draft.focus).desired_width(82.0).hint_text(crate::i18n::tr("Unknown")));
            register(ui.ctx(), "input:measured-focus", r.rect);
            changed |= r.changed();
        });
        note(ui, "Enter the actual capture crop factor. Focus is optional; vignetting needs a measured distance.");
        for (name, requested, label) in [
            ("distortion", &mut draft.distortion, "Distortion"),
            ("tca", &mut draft.tca, "Colour fringes (experimental)"),
            ("vignetting", &mut draft.vignetting, "Vignetting"),
        ] {
            let r = ui.checkbox(requested, crate::i18n::tr(label));
            register(ui.ctx(), format!("checkbox:measured-{name}"), r.rect);
        }
        note(
            ui,
            "Colour fringes use experimental working-RGB correction; sensor-channel correction is unsupported. Measured corrections use the CPU.",
        );
        if changed {
            draft.report = None;
        }
        let params = draft.params();
        if let Err(reason) = &params {
            note(ui, reason);
        }
        ui.horizontal(|ui| {
            let check = ui.add_enabled_ui(params.is_ok(), |ui| text_button(ui, "measured-check", crate::i18n::tr("Check"), false)).inner;
            if check.clicked()
                && let Ok(params) = &params
            {
                draft.report = app.run("optics.inspectProfile", params.clone()).ok();
            }
            let apply = ui.add_enabled_ui(draft.can_apply(), |ui| text_button(ui, "measured-apply", crate::i18n::tr("Apply supported"), false)).inner;
            if apply.clicked()
                && let Ok(params) = &params
            {
                // The engine rechecks the actual source and owns the Undo transaction.
                draft.report = app.run("optics.applyProfile", params.clone()).ok();
            }
        });
        if let Some(report) = &draft.report {
            if let Some(profile) = report["profile"].as_str() {
                let data =
                    draft.profiles.as_ref().and_then(|v| v["profiles"].as_array()).and_then(|profiles| profiles.iter().find(|p| p["id"] == profile));
                let name = data.and_then(|p| p["aliases"].as_array()).and_then(|a| a.first()).and_then(Value::as_str).unwrap_or(profile);
                note(ui, &format!("Matched profile: {name}"));
                // Expose measured distances so an unavailable vignette correction is actionable;
                // these are coverage facts, never suggestions to guess the capture distance.
                if let Some(profile) = data {
                    let mut distances: Vec<f64> =
                        profile["vignetting"].as_array().into_iter().flatten().filter_map(|row| row["focus_m"].as_f64()).collect();
                    distances.sort_by(f64::total_cmp);
                    distances.dedup();
                    if !distances.is_empty() {
                        note(
                            ui,
                            &format!(
                                "Measured focus rows: {} m. Use only the actual capture distance.",
                                distances.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
                            ),
                        );
                    }
                }
            } else {
                note(ui, "No exact bundled profile matches this lens.");
            }
            if let Some(reason) = report["reason"].as_str() {
                reason_note(ui, reason);
            } else {
                for (effect, label) in [("distortion", "Distortion"), ("tca", "Colour fringes"), ("vignetting", "Vignetting")] {
                    if report["available"][effect]["supported"] == true {
                        note(ui, &format!("{label}: supported"));
                    } else if let Some(reason) = report["available"][effect]["reason"].as_str() {
                        note(ui, &format!("{label}: unavailable"));
                        reason_note(ui, reason);
                    }
                }
                if report["applied"] == true && app.session.develop_of(id).is_some_and(|d| d.optics.calibration.is_some()) {
                    note(ui, "Measured settings stored. Undo restores the previous settings.");
                }
            }
        } else {
            note(ui, "Check these capture assertions to see available corrections.");
        }
    });
    app.ui.measured_lens = draft;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Services, headless::Headless};
    use lightcraft_catalog::{MediaKind, Op, Photo, Source};
    use lightcraft_engine::{Selection, Session};
    use lightcraft_raster::Rgb32f;
    use std::{sync::Arc, time::Duration};

    const T: Duration = Duration::from_secs(20);

    fn headless() -> Headless {
        let mut session = Session::new();
        let mut photo = Photo::new(PhotoId(1), Source::File { path: "measured.arw".into() }, "measured.arw", "ARW", 6000, 4000, "");
        photo.kind = MediaKind::Raw;
        photo.meta.camera = "Sony ILCE-7M4".into();
        photo.meta.lens = "FE 50mm f/1.8".into();
        photo.meta.focal_mm = Some(50.0);
        photo.meta.aperture = Some(4.0);
        photo.content_hash = Some(lightcraft_preview::hash_bytes(b"measured UI fixture").to_string());
        let info = lightcraft_engine::media::source_info(&photo);
        session.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        session.selection = Selection::single(PhotoId(1));
        session.media.file_loader = Some(Arc::new(move |_, _| Ok((Rgb32f::filled(90, 60, [0.2; 3]), info))));
        let services = Services {
            png: Some(Box::new(|img| lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(img), &Default::default()).unwrap())),
            write: Some(Box::new(|p, b| std::fs::write(p, b).map_err(|e| e.to_string()))),
            ..Default::default()
        };
        let mut app = LightcraftApp::new(session, services);
        app.ui.open_sections = vec!["optics".into()];
        app.ui.histogram = false;
        app.ui.filmstrip = false;
        app.ui.right_width = 330.0;
        let mut h = Headless::new(app, [1300.0, 1100.0], 1.0);
        h.step();
        h.step();
        h
    }

    fn click(h: &mut Headless, id: &str) {
        let r = h.request("ui.clickWidget", json!({"id":id}), T);
        assert_eq!(r["ok"], true, "{r}");
    }

    fn shot(h: &mut Headless, name: &str) {
        if let Some(dir) = std::env::var_os("LIGHTCRAFT_OPTICS_UI_SHOTS") {
            let path = std::path::Path::new(&dir).join(name);
            let r = h.request("ui.screenshot", json!({"path":path.to_string_lossy(),"headless":true}), T);
            assert_eq!(r["ok"], true, "{r}");
        }
    }

    #[test]
    fn measured_optics_ui_requires_assertions_applies_supported_and_undoes() {
        let mut h = headless();
        assert!(h.app.ui.measured_lens.crop.is_empty());
        assert!(h.app.ui.measured_lens.focus.is_empty());
        assert!(!h.app.ui.measured_lens.tca);
        assert!(!h.app.ui.measured_lens.can_apply());
        assert!(h.app.ui.measured_lens.report.as_ref().unwrap()["reason"].as_str().unwrap().contains("crop"));
        click(&mut h, "input:measured-crop");
        assert_eq!(h.request("ui.text", json!({"text":"1"}), T)["ok"], true);
        click(&mut h, "button:measured-check");
        let report = h.app.ui.measured_lens.report.as_ref().unwrap();
        assert_eq!(report["available"]["distortion"]["supported"], true);
        assert_eq!(report["available"]["vignetting"]["supported"], false);
        assert!(h.app.ui.measured_lens.can_apply());
        shot(&mut h, "measured-no-focus.png");
        click(&mut h, "input:measured-focus");
        assert_eq!(h.request("ui.text", json!({"text":"10"}), T)["ok"], true);
        click(&mut h, "button:measured-check");
        click(&mut h, "button:measured-apply");
        let calibration = h.app.session.develop_of(PhotoId(1)).unwrap().optics.calibration.unwrap();
        assert!(calibration.distortion.is_some());
        assert!(calibration.vignetting.is_some());
        assert!(calibration.tca.is_none());
        shot(&mut h, "measured-applied.png");
        h.app.run("edit.undo", json!({})).unwrap();
        assert!(h.app.session.develop_of(PhotoId(1)).unwrap().optics.calibration.is_none());
        // Draft assertions are not persisted in ui.json, and are reset for another capture.
        let restored: crate::UiState = serde_json::from_str(&serde_json::to_string(&h.app.ui).unwrap()).unwrap();
        assert!(restored.measured_lens.crop.is_empty());
        let mut photo = h.app.session.catalog.photo(PhotoId(1)).unwrap().as_ref().clone();
        photo.id = PhotoId(2);
        photo.meta.lens = "Unknown lens".into();
        h.app.session.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        h.app.run("library.select", json!({"ids":[2],"active":2})).unwrap();
        h.step();
        assert!(h.app.ui.measured_lens.crop.is_empty());
        assert!(h.app.ui.measured_lens.focus.is_empty());
        assert!(!h.app.ui.measured_lens.can_apply());
        assert!(h.app.ui.measured_lens.report.as_ref().unwrap()["profile"].is_null());
        shot(&mut h, "measured-unsupported.png");
    }

    #[test]
    fn measured_optics_ui_refuses_invalid_fields_and_fits_narrow_panel() {
        let mut h = headless();
        for crop in ["NaN", "-1", "1,0", "Sony"] {
            h.app.ui.measured_lens.crop = crop.into();
            assert!(h.app.ui.measured_lens.params().is_err());
        }
        h.app.ui.measured_lens.crop = "1".into();
        h.app.ui.measured_lens.focus = "infinity".into();
        assert!(h.app.ui.measured_lens.params().is_err());
        h.app.ui.measured_lens.focus.clear();
        click(&mut h, "button:measured-check");
        click(&mut h, "checkbox:measured-tca");
        click(&mut h, "button:measured-apply");
        assert!(h.app.session.develop_of(PhotoId(1)).unwrap().optics.calibration.unwrap().tca.is_some());
        for width in [crate::state::RIGHT_WIDTH.min, crate::state::RIGHT_WIDTH.default, crate::state::RIGHT_WIDTH.max] {
            h.app.ui.right_width = width;
            h.step();
            h.step();
            let panel = h.app.widgets.iter().find(|(id, _)| id == "panel:right_panel").unwrap().1;
            for (id, r) in &h.app.widgets {
                if id.contains("measured-") {
                    assert!(r.left() >= panel.left() && r.right() <= panel.right(), "{id} {r:?} outside {panel:?}");
                }
            }
        }
        shot(&mut h, "measured-wide.png");
    }
}
