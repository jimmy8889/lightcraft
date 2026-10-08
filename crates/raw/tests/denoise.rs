//! Procedural quality and safety regressions; no proprietary images or model weights.
use lightcraft_raw::{
    BlackLevel, Cfa, ColorData, OpcodeLists, Orientation, RawData, RawFormat, RawImage, Rect,
    denoise::{Options, process},
};
use std::sync::atomic::{AtomicBool, Ordering};

fn image(pattern: Option<&str>, float: bool) -> RawImage {
    let (width, height) = (80, 60);
    let cpp = if pattern.is_some() { 1 } else { 3 };
    let cfa = pattern.map(|p| Cfa::bayer(p).unwrap());
    let mut seed = 783u32;
    let mut data = Vec::new();
    for y in 0..height {
        for x in 0..width {
            for s in 0..cpp {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = ((seed >> 16) as f32 / 65535.0 - 0.5) * 0.035;
                let channel = cfa.as_ref().map_or(s, |p| p.color_at(x, y) as usize);
                let base = [0.20, 0.45, 0.65][channel];
                let v = if x >= 60 { 1.10 } else { base + if x >= 40 { 0.15 } else { 0.0 } + noise };
                data.push(v * 15000.0 + 256.0);
            }
        }
    }
    RawImage {
        format: RawFormat::Dng,
        width,
        height,
        cpp,
        data: if float { RawData::F32(data) } else { RawData::U16(data.iter().map(|v| v.round() as u16).collect()) },
        cfa,
        bits: 16,
        black: BlackLevel::uniform(256.0),
        white: vec![15256.0],
        active_area: Rect::new(2, 1, 76, 58),
        crop: Rect::new(1, 1, 74, 56),
        orientation: Orientation::Rotate90,
        color: ColorData { as_shot_neutral: Some([0.5, 1.0, 0.7]), baseline_exposure: 0.35, ..Default::default() },
        wb_multipliers: Some([1.0; 3]),
        linearized: false,
        opcodes: OpcodeLists::default(),
        metadata: lightcraft_meta::Metadata { model: Some("Procedural camera".into()), iso: Some(3200), ..Default::default() },
    }
}

fn mse(raw: &RawImage) -> (f64, [f64; 3]) {
    let mut squared = 0.0;
    let mut sums = [0.0; 3];
    let mut counts = [0; 3];
    for y in 6..54 {
        for x in 6..34 {
            for s in 0..raw.cpp {
                let c = raw.cfa.as_ref().map_or(s, |p| p.color_at(x, y) as usize);
                let v = (raw.data.get((y * raw.width + x) * raw.cpp + s) - 256.0) / 15000.0;
                squared += (v as f64 - [0.20, 0.45, 0.65][c]).powi(2);
                sums[c] += v as f64;
                counts[c] += 1;
            }
        }
    }
    (squared / counts.iter().sum::<usize>() as f64, std::array::from_fn(|c| sums[c] / counts[c].max(1) as f64))
}

#[test]
fn noise_reduced_all_bayer_phases_and_linear_rgb_without_colour_shift() {
    for cfa in [Some("RGGB"), Some("BGGR"), Some("GBRG"), Some("GRBG"), None] {
        for float in [false, true] {
            let mut raw = image(cfa, float);
            let old = raw.clone();
            let before = mse(&raw);
            let report = process(&mut raw, Options { strength: 100.0 }, &AtomicBool::new(false)).unwrap();
            let after = mse(&raw);
            assert!(after.0 < before.0 * 0.65, "{cfa:?} f32={float}: {} -> {} sigmas={:?}", before.0, after.0, report.sigma);
            for c in 0..3 {
                assert!((after.1[c] - before.1[c]).abs() < 0.001, "channel mean changed");
            }
            assert_eq!(raw.info(), old.info(), "all geometry/colour/metadata preserved");
            assert_eq!(raw.black, old.black);
            assert_eq!(raw.white, old.white);
            // Masked borders and saturated/above-white samples retained bit-for-bit.
            for y in 0..raw.height {
                for x in 0..raw.width {
                    if y == 0 || !(2..60).contains(&x) {
                        for s in 0..raw.cpp {
                            let i = (y * raw.width + x) * raw.cpp + s;
                            assert_eq!(raw.data.get(i), old.data.get(i));
                        }
                    }
                }
            }
            // A real step edge much larger than the measured noise is not blended across it.
            for y in 10..50 {
                for x in 38..42 {
                    for s in 0..raw.cpp {
                        let c = raw.cfa.as_ref().map_or(s, |p| p.color_at(x, y) as usize);
                        let v = (raw.data.get((y * raw.width + x) * raw.cpp + s) - 256.0) / 15000.0;
                        let expected = [0.20, 0.45, 0.65][c] + if x >= 40 { 0.15 } else { 0.0 };
                        assert!((v - expected).abs() < 0.025, "edge blurred");
                    }
                }
            }
        }
    }
}

#[test]
fn texture_and_gradient_are_retained() {
    let mut raw = image(Some("RGGB"), true);
    if let RawData::F32(data) = &mut raw.data {
        for y in 0..raw.height {
            for x in 0..raw.width {
                // Strong two-sensor-pixel texture should not be mistaken for a flat field.
                data[y * raw.width + x] = 256.0 + 15000.0 * (0.3 + (x / 2 % 2) as f32 * 0.18 + y as f32 * 0.002);
            }
        }
    }
    let old = raw.clone();
    process(&mut raw, Options { strength: 100.0 }, &AtomicBool::new(false)).unwrap();
    for y in 10..50 {
        for x in 8..70 {
            let i = y * raw.width + x;
            assert!((raw.data.get(i) - old.data.get(i)).abs() / 15000.0 < 0.005);
        }
    }
}

#[test]
fn fine_sinusoidal_texture_retains_contrast_while_noise_falls() {
    let mut raw = image(Some("RGGB"), true);
    let signal =
        |x: usize, y: usize| 0.4 + 0.04 * (std::f32::consts::TAU * x as f32 / 24.0).sin() + 0.012 * (std::f32::consts::TAU * y as f32 / 16.0).sin();
    let mut seed = 945u32;
    if let RawData::F32(data) = &mut raw.data {
        for y in 0..raw.height {
            for x in 0..raw.width {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = ((seed >> 16) as f32 / 65535.0 - 0.5) * 0.035;
                data[y * raw.width + x] = 256.0 + 15000.0 * (signal(x, y) + noise);
            }
        }
    }
    let error = |image: &RawImage| {
        let mut mse = 0.0f64;
        let mut contrast = 0.0f64;
        let mut energy = 0.0f64;
        for y in 6..54 {
            for x in 6..74 {
                let actual = (image.data.get(y * image.width + x) - 256.0) / 15000.0;
                let expected = signal(x, y);
                mse += (actual as f64 - expected as f64).powi(2);
                contrast += (actual as f64 - 0.4) * (expected as f64 - 0.4);
                energy += (expected as f64 - 0.4).powi(2);
            }
        }
        (mse, contrast / energy)
    };
    let before = error(&raw);
    process(&mut raw, Options { strength: 100.0 }, &AtomicBool::new(false)).unwrap();
    let after = error(&raw);
    assert!(after.0 < before.0 * 0.8, "fine texture noise error {} -> {}", before.0, after.0);
    assert!(after.1 > 0.85 && after.1 < 1.05, "fine texture contrast {}", after.1);
}

#[test]
fn invalid_strength_nan_dimensions_and_unsupported_cfa_return_errors() {
    for strength in [f32::NAN, f32::INFINITY, -1.0, 101.0] {
        let mut raw = image(Some("RGGB"), false);
        let old = raw.clone();
        assert!(process(&mut raw, Options { strength }, &AtomicBool::new(false)).is_err());
        assert_eq!(raw, old);
    }
    let mut raw = image(None, true);
    if let RawData::F32(data) = &mut raw.data {
        data[8] = f32::NAN;
    }
    assert!(process(&mut raw, Options { strength: 50.0 }, &AtomicBool::new(false)).is_err());
    raw.width = usize::MAX;
    assert!(process(&mut raw, Options { strength: 50.0 }, &AtomicBool::new(false)).is_err());
    let mut tiny_range = image(None, true);
    tiny_range.black = BlackLevel::uniform(0.0);
    tiny_range.white = vec![f32::from_bits(1)];
    if let RawData::F32(v) = &mut tiny_range.data {
        v.fill(-1.0);
    }
    let unchanged = tiny_range.clone();
    assert!(process(&mut tiny_range, Options { strength: 50.0 }, &AtomicBool::new(false)).is_err());
    assert_eq!(tiny_range, unchanged, "finite-but-overflowing levels never change samples");
    let mut raw = image(Some("RGGB"), false);
    raw.cfa = Some(Cfa::xtrans());
    assert!(process(&mut raw, Options { strength: 50.0 }, &AtomicBool::new(false)).is_err());
}

#[test]
fn disabled_and_cancelled_processing_never_change_samples() {
    let mut raw = image(Some("RGGB"), false);
    let old = raw.clone();
    process(&mut raw, Options::default(), &AtomicBool::new(false)).unwrap();
    assert_eq!(raw, old);
    let cancel = AtomicBool::new(false);
    cancel.store(true, Ordering::Relaxed);
    assert!(process(&mut raw, Options { strength: 50.0 }, &cancel).unwrap_err().to_string().contains("cancelled"));
    assert_eq!(raw, old);
    assert_eq!(Options::default().cache_key(), 0);
    assert_ne!(Options { strength: 50.0 }.cache_key(), Options { strength: 51.0 }.cache_key());
}

#[test]
fn processed_dng_contains_cleaned_values_and_sensor_metadata() {
    for cfa in [Some("GBRG"), None] {
        let mut raw = image(cfa, false);
        let old = raw.clone();
        process(&mut raw, Options { strength: 75.0 }, &AtomicBool::new(false)).unwrap();
        assert_ne!(raw.data, old.data);
        let bytes = lightcraft_raw::write_dng(&raw, &Default::default()).unwrap();
        let back = lightcraft_raw::decode(&bytes).unwrap();
        assert_eq!(back.data, raw.data);
        assert_eq!(back.cfa, raw.cfa);
        assert_eq!(back.crop, raw.crop);
        assert_eq!(back.active_area, raw.active_area);
        assert_eq!(back.orientation, raw.orientation);
        assert_eq!(back.black.values.first(), raw.black.values.first());
        assert_eq!(back.white_at(0), raw.white_at(0));
        assert_eq!(back.color.as_shot_neutral, raw.color.as_shot_neutral);
        assert_eq!(back.metadata.model, raw.metadata.model);
        assert_eq!(back.metadata.iso, raw.metadata.iso);
    }
}

/// Opt-in local acceptance. Files are read-only and never bundled with the project.
#[test]
#[ignore = "requires LIGHTCRAFT_SONY_CORPUS pointing at the user's local ARW sample folder"]
fn sony_corpus_native_processing_and_full_dng_readback() {
    let dir = std::env::var("LIGHTCRAFT_SONY_CORPUS").expect("set LIGHTCRAFT_SONY_CORPUS");
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw")))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    for path in paths {
        let bytes = std::fs::read(&path).unwrap();
        let mut raw = lightcraft_raw::decode(&bytes).unwrap();
        let info = raw.info();
        let started = std::time::Instant::now();
        let report = process(&mut raw, Options { strength: 60.0 }, &AtomicBool::new(false)).unwrap();
        let elapsed = started.elapsed().as_secs_f64();
        assert_eq!(raw.info(), info);
        let encoded = lightcraft_raw::write_dng(
            &raw,
            &lightcraft_raw::DngWriteOptions {
                compression: lightcraft_raw::DngCompression::Deflate { tile: 256, half: false },
                ..Default::default()
            },
        )
        .unwrap();
        let back = lightcraft_raw::decode(&encoded).unwrap();
        assert_eq!(back.data, raw.data);
        assert_eq!(back.cfa, raw.cfa);
        assert_eq!(back.orientation, raw.orientation);
        assert_eq!(back.active_area, raw.active_area);
        assert_eq!(back.crop, raw.crop);
        for s in 0..raw.cpp {
            assert_eq!(back.white_at(s), raw.white_at(s));
        }
        assert_eq!(back.black.values.first(), raw.black.values.first());
        assert_eq!(back.metadata.model, raw.metadata.model);
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "original unchanged");
        eprintln!(
            "{}: {}x{} {} native {:.3}s, sigmas {:?}, processed DNG readback exact",
            path.display(),
            raw.width,
            raw.height,
            report.mode,
            elapsed,
            report.sigma
        );
    }
}
