#![cfg(feature = "pmrid")]
//! Opt-in model acceptance. Model/reference files live outside the repository; no weights bundled.
use lightcraft_raw::{
    BlackLevel, Cfa, ColorData, OpcodeLists, Orientation, RawData, RawFormat, RawImage, Rect,
    pmrid::{CalibrationPoint, Model, NoiseProfile, Options, process},
};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

fn profile() -> NoiseProfile {
    NoiseProfile {
        schema: 1,
        camera_model: "Procedural camera".into(),
        points: vec![CalibrationPoint { iso: 1600, shot: 0.0010093132, read_variance: 0.000003239 }],
    }
}
fn fixture(pattern: &str, float: bool) -> RawImage {
    let (width, height) = (72, 70);
    let cfa = Cfa::bayer(pattern).unwrap();
    let mut seed = 42917u32;
    let mut data = Vec::new();
    for y in 0..height {
        for x in 0..width {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let c = cfa.color_at(x, y) as usize;
            let noise = ((seed >> 16) as f32 / 65535.0 - 0.5) * 0.06;
            let v = if x >= 60 { 1.1 } else { [0.2, 0.4, 0.6][c] + noise };
            data.push(256.0 + v * 15000.0);
        }
    }
    RawImage {
        format: RawFormat::Dng,
        width,
        height,
        cpp: 1,
        data: if float { RawData::F32(data) } else { RawData::U16(data.into_iter().map(|v| v.round() as u16).collect()) },
        cfa: Some(cfa),
        bits: 16,
        black: BlackLevel::uniform(256.0),
        white: vec![15256.0],
        active_area: Rect::new(3, 2, 63, 61),
        crop: Rect::new(1, 1, 61, 59),
        orientation: Orientation::Rotate90,
        color: ColorData::default(),
        wb_multipliers: None,
        linearized: false,
        opcodes: OpcodeLists::default(),
        metadata: lightcraft_meta::Metadata { model: Some("Procedural camera".into()), iso: Some(1600), ..Default::default() },
    }
}
fn model() -> Model {
    Model::load(Path::new(&std::env::var("LIGHTCRAFT_PMRID_CHECKPOINT").expect("set LIGHTCRAFT_PMRID_CHECKPOINT"))).unwrap()
}
fn floats(path: impl AsRef<Path>) -> Vec<f32> {
    std::fs::read(path).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

#[test]
fn explicit_noise_profile_matches_camera_and_iso_domain_without_extrapolation() {
    let mut p = profile();
    p.validate().unwrap();
    let key = p.cache_key(1600, Options::default()).unwrap();
    assert_eq!(key, p.cache_key(1600, Options::default()).unwrap());
    assert_ne!(key, p.cache_key(1600, Options { strength: 50.0, ..Default::default() }).unwrap());
    assert!(p.at("Sony ILCE-7M3", 1600).is_err());
    assert!(p.at("Procedural camera", 1599).is_err());
    assert!(p.at("Procedural camera", 1601).is_err());
    let a = p.at("Procedural camera", 1600).unwrap();
    p.points.push(CalibrationPoint { iso: 3200, shot: a.0 * 2.0, read_variance: a.1 * 4.0 });
    let mid = p.at("Procedural camera", 2400).unwrap();
    assert!((mid.0 - a.0 * 1.5).abs() < 1e-12);
    assert!((mid.1 - a.1 * 2.5).abs() < 1e-12);
    p.points[1].iso = 1600;
    assert!(p.validate().is_err());
    p = profile();
    p.points[0].shot = f64::NAN;
    assert!(p.validate().is_err());
    p = profile();
    p.points[0].read_variance = -0.1;
    assert!(p.validate().is_err());
    p = profile();
    p.schema = 2;
    assert!(p.validate().is_err());
}
#[test]
fn model_hash_and_tile_allocations_are_bounded() {
    assert!(Model::from_bytes(vec![0; 32]).is_err());
    let mut o = Options::default();
    o.validate().unwrap();
    o.halo = usize::MAX;
    assert!(o.validate().is_err());
    o = Options::default();
    o.tile_core = 129;
    assert!(o.validate().is_err());
    o = Options::default();
    o.strength = f32::NAN;
    assert!(o.validate().is_err());
}

#[test]
#[ignore = "requires external pinned checkpoint and PyTorch reference tensors"]
fn official_checkpoint_matches_pytorch_reference_and_rejects_hostile_tensors() {
    let model = model();
    let dir = std::env::var("LIGHTCRAFT_PMRID_REFERENCE").expect("set LIGHTCRAFT_PMRID_REFERENCE");
    let input = floats(Path::new(&dir).join("pmrid-reference-input.f32"));
    let expected = floats(Path::new(&dir).join("pmrid-reference-output.f32"));
    let cancel = AtomicBool::new(false);
    let actual = model.infer_packed(&input, 32, 32, &cancel).unwrap();
    assert_eq!(expected.len(), actual.len());
    let delta = expected.iter().zip(&actual).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    let rms = (expected.iter().zip(&actual).map(|(a, b)| (*a as f64 - *b as f64).powi(2)).sum::<f64>() / actual.len() as f64).sqrt();
    eprintln!("PMRID Torch2.6/Candle0.9.2 CPU32x32: max delta={delta}, RMS={rms}");
    assert!(delta < 0.005 && rms < 0.001, "real checkpoint inference differs from official PyTorch network");
    let mut invalid = input.clone();
    invalid[0] = f32::NAN;
    assert!(model.infer_packed(&invalid, 32, 32, &cancel).is_err());
    assert!(model.infer_packed(&input, usize::MAX, 32, &cancel).is_err());
    cancel.store(true, Ordering::Relaxed);
    assert!(model.infer_packed(&input, 32, 32, &cancel).unwrap_err().to_string().contains("cancelled"));
}

#[test]
#[ignore = "requires external pinned checkpoint and independent PyTorch Bayer tile references"]
fn calibrated_overlapping_tiles_match_independent_pytorch_reference() {
    let model = model();
    let dir = std::env::var("LIGHTCRAFT_PMRID_REFERENCE").expect("set LIGHTCRAFT_PMRID_REFERENCE");
    for cfa in ["RGGB", "BGGR", "GRBG", "GBRG"] {
        let mut raw = fixture(cfa, true);
        raw.data = RawData::F32(floats(Path::new(&dir).join(format!("pmrid-tile-{cfa}-input.f32"))));
        let expected = floats(Path::new(&dir).join(format!("pmrid-tile-{cfa}-output.f32")));
        process(&mut raw, &model, &profile(), Options { strength: 70.0, tile_core: 16, halo: 16 }, &AtomicBool::new(false)).unwrap();
        let delta = expected.iter().enumerate().map(|(i, &a)| (a - raw.data.get(i)).abs()).fold(0.0f32, f32::max);
        eprintln!("{cfa}: independent calibrated/cropped tiled reference max ADC delta={delta}");
        assert!(delta < 0.5, "Bayer packing, cropped phases, calibration or inverse reconstruction differs");
    }
}

#[test]
#[ignore = "requires external pinned checkpoint; synthetic Bayer quality and output readback"]
fn calibrated_bayer_processing_crops_preserves_metadata_and_dng_clean_samples() {
    let model = model();
    let profile = profile();
    for cfa in ["RGGB", "BGGR", "GRBG", "GBRG"] {
        for float in [false, true] {
            let mut raw = fixture(cfa, float);
            let original = raw.clone();
            let report = process(&mut raw, &model, &profile, Options { strength: 70.0, tile_core: 16, halo: 16 }, &AtomicBool::new(false)).unwrap();
            assert_eq!(report.tiles, 4);
            assert_eq!(raw.info(), original.info());
            assert_ne!(raw.data, original.data);
            let mut before = 0.0;
            let mut after = 0.0;
            let mut means = [0.0; 3];
            let mut counts = [0usize; 3];
            for y in 0..raw.height {
                for x in 0..raw.width {
                    let index = y * raw.width + x;
                    if !(3..60).contains(&x) || !(2..63).contains(&y) {
                        assert_eq!(raw.data.get(index), original.data.get(index));
                    }
                    if (10..50).contains(&x) && (10..50).contains(&y) {
                        let c = raw.cfa.as_ref().unwrap().color_at(x, y) as usize;
                        let target = [0.2, 0.4, 0.6][c];
                        let a = (original.data.get(index) - 256.0) / 15000.0;
                        let b = (raw.data.get(index) - 256.0) / 15000.0;
                        before += (a - target).powi(2);
                        after += (b - target).powi(2);
                        means[c] += (b - a) as f64;
                        counts[c] += 1;
                    }
                }
            }
            eprintln!(
                "{cfa} f32={float} synthetic PMRID MSE ratio={}, channel mean deltas={:?}",
                after / before,
                std::array::from_fn::<_, 3, _>(|c| means[c] / counts[c] as f64)
            );
            assert!(after < before, "actual trained RAW model must reduce synthetic noise for the supplied calibration");
            for c in 0..3 {
                assert!((means[c] / counts[c] as f64).abs() < 0.01, "flat colour moved excessively");
            }
            let encoded = lightcraft_raw::write_dng(
                &raw,
                &lightcraft_raw::DngWriteOptions {
                    compression: lightcraft_raw::DngCompression::Deflate { tile: 64, half: false },
                    ..Default::default()
                },
            )
            .unwrap();
            let back = lightcraft_raw::decode(&encoded).unwrap();
            assert_eq!(back.data, raw.data);
            assert_eq!(back.cfa, raw.cfa);
            assert_eq!(back.active_area, raw.active_area);
            assert_eq!(back.crop, raw.crop);
            assert_eq!(back.orientation, raw.orientation);
        }
    }
    let mut raw = fixture("RGGB", false);
    let before = raw.clone();
    let cancel = AtomicBool::new(true);
    assert!(process(&mut raw, &model, &profile, Options::default(), &cancel).is_err());
    assert_eq!(raw, before);
    let mut wrong = profile.clone();
    wrong.camera_model = "Other camera".into();
    assert!(process(&mut raw, &model, &wrong, Options::default(), &AtomicBool::new(false)).is_err());
    assert_eq!(raw, before);
    raw.metadata.iso = Some(800);
    assert!(process(&mut raw, &model, &profile, Options::default(), &AtomicBool::new(false)).is_err());
    raw.metadata.iso = Some(1600);
    raw.cfa = None;
    raw.cpp = 3;
    raw.data = RawData::U16(vec![5000; raw.width * raw.height * 3]);
    assert!(process(&mut raw, &model, &profile, Options::default(), &AtomicBool::new(false)).unwrap_err().to_string().contains("linear RGB"));
}
