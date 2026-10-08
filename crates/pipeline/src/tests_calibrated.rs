//! Procedural measured-curve tests in linear RGB. Real sensor-channel TCA fidelity is a separate gap.
use crate::{SourceInfo, frame_for};
use lightcraft_develop::lens_profiles::{CalibratedLens, CaptureIdentity, LateralCa, RadialDistortion};
use lightcraft_develop::{DevelopSettings, EmbeddedLens};
use lightcraft_geom::{Affine, Orientation, Point};
use lightcraft_raster::Rgb32f;

fn calibration() -> CalibratedLens {
    CalibratedLens {
        capture_key: CaptureIdentity { metadata: 7, content: [1, 2] },
        crop_factor: 1.0,
        aspect_ratio: 1.5,
        distortion: Some(RadialDistortion::Poly3 { k1: -0.03 }),
        tca: Some(LateralCa { red: [0.0001, 0.0, 1.006], blue: [-0.0001, 0.0, 0.995] }),
        vignetting: Some([-0.4, 0.1, -0.02]),
    }
}
fn settings() -> DevelopSettings {
    let mut s = DevelopSettings::default();
    s.optics.calibration = Some(calibration());
    s.optics.lens_profile = true;
    s
}
fn info() -> SourceInfo {
    SourceInfo { raw: true, calibration_key: Some(calibration().capture_key), ..Default::default() }
}

#[test]
fn calibrated_maps_match_published_models_and_scale_with_resolution_orientation() {
    let mut s = settings();
    s.optics.calibration.as_mut().unwrap().tca = None;
    for model in [
        RadialDistortion::Poly3 { k1: 0.02 },
        RadialDistortion::Poly5 { k1: 0.01, k2: 0.001 },
        RadialDistortion::Ptlens { a: 0.005, b: -0.01, c: 0.003 },
    ] {
        s.optics.calibration.as_mut().unwrap().distortion = Some(model);
        for (w, h) in [(600.0, 400.0), (300.0, 200.0), (400.0, 600.0)] {
            let wp = crate::optics::Warp::from_settings(w, h, &s, None);
            let p = Point::new(w / 2.0 + w.min(h) * 0.35, h / 2.0);
            let mapped = wp.corrected_to_source(p, 1);
            let radius = 0.7;
            let scale = match model {
                RadialDistortion::Poly3 { k1 } => 1.0 - k1 + k1 * radius * radius,
                RadialDistortion::Poly5 { k1, k2 } => 1.0 + k1 * radius.powi(2) + k2 * radius.powi(4),
                RadialDistortion::Ptlens { a, b, c } => a * radius.powi(3) + b * radius.powi(2) + c * radius + 1.0 - a - b - c,
            };
            assert!((mapped.x - (w / 2.0 + (p.x - w / 2.0) * scale)).abs() < 1e-10);
            assert_eq!(mapped.y, p.y);
            assert_eq!(wp.corrected_to_source(Point::new(w / 2.0, h / 2.0), 0), Point::new(w / 2.0, h / 2.0));
        }
    }
    let wp = crate::optics::Warp::from_settings(600.0, 400.0, &settings(), None);
    let q = Point::new(440.0, 200.0);
    let green = wp.corrected_to_source(q, 1);
    let r = (green.x - 300.0) / 200.0;
    assert!((wp.corrected_to_source(q, 0).x - (300.0 + (green.x - 300.0) * (1.006 + 0.0001 * r * r))).abs() < 1e-10);
    assert!((wp.calibrated_gain(Point::new(600.0, 400.0)) as f64 - 1.0 / 0.68).abs() < 1e-6);
}

#[test]
fn calibrated_source_identity_unknown_preview_and_embedded_precedence() {
    let src = Rgb32f::filled(60, 40, [0.2; 3]);
    let s = settings();
    assert!(frame_for(&src, &info(), &s, true).warp.is_some());
    for altered in [
        SourceInfo::default(),
        SourceInfo { raw: false, ..info() },
        SourceInfo { calibration_key: None, ..info() },
        SourceInfo { calibration_key: Some(CaptureIdentity { metadata: 8, ..calibration().capture_key }), ..info() },
        SourceInfo { calibration_key: Some(CaptureIdentity { content: [2, 1], ..calibration().capture_key }), ..info() },
    ] {
        assert!(frame_for(&src, &altered, &s, true).warp.is_none());
    }
    let wp = crate::optics::Warp::from_settings(60.0, 40.0, &s, Some(&EmbeddedLens::default()));
    assert!(wp.calibration.is_none());
    assert!(crate::optics::Warp::from_settings(60.0, 34.0, &s, None).calibration.is_none());
    let mut bad = s.clone();
    bad.optics.calibration.as_mut().unwrap().vignetting = Some([-8.0, 8.0, 0.0]);
    assert!(frame_for(&src, &info(), &bad, true).warp.is_none());
}

fn inverse_radial(p: Point, w: f64, h: f64, scale: impl Fn(f64) -> f64) -> Point {
    let centre = Point::new(w / 2.0, h / 2.0);
    let norm = w.min(h) / 2.0;
    let rd = p.dist(centre) / norm;
    if rd < 1e-15 {
        return centre;
    }
    let (mut lo, mut hi) = (0.0, 4.0);
    for _ in 0..50 {
        let mid = (lo + hi) / 2.0;
        if mid * scale(mid) < rd {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let ru = (lo + hi) / 2.0;
    Point::new(centre.x + (p.x - centre.x) * ru / rd, centre.y + (p.y - centre.y) * ru / rd)
}
fn pattern(p: Point) -> f32 {
    (0.3 + 0.08 * (p.x / 25.0).sin() + 0.06 * (p.y / 19.0).cos()) as f32
}
fn shot(w: usize, h: usize) -> Rgb32f {
    let cal = calibration();
    let wp = crate::optics::Warp::from_settings(w as f64, h as f64, &settings(), None);
    Rgb32f::from_fn(w, h, |x, y| {
        let p = Point::new(x as f64 + 0.5, y as f64 + 0.5);
        std::array::from_fn(|ch| {
            let plane = match ch {
                0 => cal.tca.unwrap().red,
                2 => cal.tca.unwrap().blue,
                _ => [0.0, 0.0, 1.0],
            };
            let p0 = inverse_radial(p, w as f64, h as f64, |r| plane[0] * r * r + plane[1] * r + plane[2]);
            let ideal = inverse_radial(p0, w as f64, h as f64, |r| cal.distortion.unwrap().scale(r));
            pattern(Point::new(ideal.x * 300.0 / w as f64, ideal.y * 200.0 / h as f64)) / wp.calibrated_gain(p)
        })
    })
}

#[test]
fn calibrated_distortion_tca_and_vignette_correct_procedural_image() {
    let (w, h) = (300, 200);
    let src = shot(w, h);
    let frame = frame_for(&src, &info(), &settings(), true);
    let corrected = frame.sample(&src, w, h);
    let mut err = 0.0;
    let mut before = 0.0;
    let mut count = 0;
    for y in 12..h - 12 {
        for x in 12..w - 12 {
            let expected = pattern(Point::new(x as f64 + 0.5, y as f64 + 0.5));
            for ch in 0..3 {
                err += (corrected.get(x, y)[ch] - expected).abs() as f64;
                before += (src.get(x, y)[ch] - expected).abs() as f64;
                count += 1;
            }
        }
    }
    assert!(err / (count as f64) < 0.0002, "{err} / {count}");
    assert!(err < before * 0.01, "{err} vs {before}");
    let down = frame.sample(&src, 150, 100);
    let frame_small = frame_for(&shot(150, 100), &info(), &settings(), true);
    let corrected_small = frame_small.sample(&shot(150, 100), 150, 100);
    let mut preview_error = 0.0;
    let mut samples = 0;
    for y in 8..92 {
        for x in 8..142 {
            for ch in 0..3 {
                preview_error += (down.get(x, y)[ch] - corrected_small.get(x, y)[ch]).abs() as f64;
                samples += 1;
            }
        }
    }
    assert!(preview_error / (samples as f64) < 0.001, "preview/export normalized mapping differs: {}", preview_error / samples as f64);
}

#[test]
fn calibrated_vignetting_is_corrected_before_prefilter_and_rotation() {
    let (w, h) = (300, 200);
    let mut s = settings();
    let cal = s.optics.calibration.as_mut().unwrap();
    cal.distortion = None;
    cal.tca = None;
    let wp = crate::optics::Warp::from_settings(w as f64, h as f64, &s, None);
    let src = Rgb32f::from_fn(w, h, |x, y| [0.3 / wp.calibrated_gain(Point::new(x as f64 + 0.5, y as f64 + 0.5)); 3]);
    for orient in [Orientation::Normal, Orientation::Rotate90] {
        s.orientation = orient;
        for (ow, oh) in [(300, 200), (150, 100), (75, 50)] {
            let frame = frame_for(&src, &info(), &s, true);
            let out = frame.sample(&src, ow, oh);
            assert!(out.data.iter().all(|p| p.iter().all(|c| (*c - 0.3).abs() < 2e-6)));
        }
    }
}

#[test]
fn calibrated_interval_coverage_never_disagrees_with_pixel_framing() {
    let mut s = settings();
    s.optics.calibration.as_mut().unwrap().distortion = Some(RadialDistortion::Ptlens { a: 0.005, b: -0.01, c: 0.003 });
    let wp = crate::optics::Warp::from_settings(90.0, 60.0, &s, None);
    let affine = Affine([1.0, 0.0, 0.0, 1.0, -10.0, -7.0]);
    for y in (0..80).step_by(7) {
        for x in (0..110).step_by(7) {
            let (x1, y1) = ((x + 7).min(110), (y + 7).min(80));
            if let Some(covered) = wp.block_coverage(&affine, x, x1, y, y1) {
                for py in y..y1 {
                    for px in x..x1 {
                        assert_eq!(wp.covers(&affine, px, py), covered);
                    }
                }
            }
        }
    }
}
