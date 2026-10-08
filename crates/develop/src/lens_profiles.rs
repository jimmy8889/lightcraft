//! Calibrated rectilinear optics, independent of any Lensfun implementation.
//!
//! Equations: https://lensfun.github.io/manual/latest/group__Lens.html . Distortion/TCA radii use
//! half the short edge (PanoTools' lens correction model); PA attenuation uses half the diagonal.
//! Only exact calibration rows and capture crop/aspect are accepted. No extrapolation, fuzzy lens
//! matching, sensor-crop inference, or assumptions about unknown focus distance.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "model", rename_all = "lowercase", deny_unknown_fields)]
pub enum RadialDistortion {
    Poly3 { k1: f64 },
    Poly5 { k1: f64, k2: f64 },
    Ptlens { a: f64, b: f64, c: f64 },
}

impl RadialDistortion {
    /// Radial scale for corrected → observed radius, in half-short-edge units.
    pub fn scale(self, r: f64) -> f64 {
        let r2 = r * r;
        match self {
            Self::Poly3 { k1 } => 1.0 - k1 + k1 * r2,
            Self::Poly5 { k1, k2 } => 1.0 + r2 * (k1 + k2 * r2),
            Self::Ptlens { a, b, c } => 1.0 - a - b - c + r * (c + r * (b + r * a)),
        }
    }

    fn valid(self, radius: f64) -> bool {
        // A conservative lower bound on d(rd)/d(ru) throughout the image. Reject folds rather
        // than trusting a sampled grid that can miss a local reversal. Coefficient bounds also
        // bound computations on hostile settings loaded without using a profile command.
        let (terms, constant) = match self {
            Self::Poly3 { k1 } => ([0.0, 3.0 * k1, 0.0, 0.0], 1.0 - k1),
            Self::Poly5 { k1, k2 } => ([0.0, 3.0 * k1, 0.0, 5.0 * k2], 1.0),
            Self::Ptlens { a, b, c } => ([2.0 * c, 3.0 * b, 4.0 * a, 0.0], 1.0 - a - b - c),
        };
        constant.is_finite()
            && constant.abs() <= 4.0
            && terms.iter().all(|t| t.is_finite() && t.abs() <= 5.0)
            && terms.iter().enumerate().fold(constant, |v, (i, t)| v + t.min(0.0) * radius.powi(i as i32 + 1)) > 0.05
    }
}

/// Third-order red/blue corrected → observed mapping; linear TCA is b=c=0.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LateralCa {
    /// [b, c, v] (green always identity).
    pub red: [f64; 3],
    pub blue: [f64; 3],
}

impl LateralCa {
    fn valid(self, radius: f64) -> bool {
        [self.red, self.blue].iter().all(|[b, c, v]| {
            b.is_finite()
                && c.is_finite()
                && v.is_finite()
                && b.abs() <= 0.1
                && c.abs() <= 0.1
                && (0.9..=1.1).contains(v)
                && v + c.min(0.0) * 2.0 * radius + b.min(0.0) * 3.0 * radius * radius > 0.5
                && v + c.max(0.0) * radius + b.max(0.0) * radius * radius < 2.0
        })
    }
}

/// Measured edits belong to the exact original byte stream and its independently decoded metadata.
/// Content hash is the existing 128-bit preview/import identity (noncryptographic).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureIdentity {
    pub metadata: u64,
    pub content: [u64; 2],
}

pub fn capture_identity(camera: &str, lens: &str, focal: Option<f32>, aperture: Option<f32>, hash: Option<&str>) -> Option<CaptureIdentity> {
    let hash = hash?;
    if camera.is_empty()
        || lens.is_empty()
        || !focal.is_some_and(|f| f.is_finite() && f > 0.0)
        || aperture.is_some_and(|f| !f.is_finite() || f <= 0.0)
        || hash.len() != 32
        || !hash.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u128::from_str_radix(hash, 16).ok()?;
    Some(CaptureIdentity { metadata: capture_key(camera, lens, focal, aperture), content: [value as u64, (value >> 64) as u64] })
}

/// Resolved coefficients bound to one camera/lens/focal/aperture metadata identity. Persistent,
/// Copy numeric data: settings still render without the database and never read a file per pixel.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibratedLens {
    pub capture_key: CaptureIdentity,
    pub crop_factor: f64,
    pub aspect_ratio: f64,
    pub distortion: Option<RadialDistortion>,
    pub tca: Option<LateralCa>,
    /// Source attenuation 1 + k1*r² + k2*r⁴ + k3*r⁶, half-diagonal radius. Correction divides.
    pub vignetting: Option<[f64; 3]>,
}

impl CalibratedLens {
    pub fn valid(self) -> bool {
        if !self.crop_factor.is_finite()
            || !(0.5..=10.0).contains(&self.crop_factor)
            || !self.aspect_ratio.is_finite()
            || !(1.0..=3.0).contains(&self.aspect_ratio)
            || self.capture_key.metadata == 0
        {
            return false;
        }
        let radius = self.aspect_ratio.hypot(1.0);
        self.distortion.is_none_or(|d| d.valid(radius))
            && self.tca.is_none_or(|t| t.valid(radius * 2.0))
            && self.vignetting.is_none_or(valid_attenuation)
    }

    pub fn fits_dimensions(self, w: f64, h: f64) -> bool {
        let (long, short) = (w.max(h), w.min(h));
        // Allow one pixel of resize rounding but no sensor-aspect guessing. The relative cap
        // prevents a tiny thumbnail from accepting an unrelated aspect ratio.
        short > 0.0
            && long.is_finite()
            && short.is_finite()
            && (long / short - self.aspect_ratio).abs() <= ((1.0 + self.aspect_ratio) / short).min(0.015)
    }
}

fn valid_attenuation(k: [f64; 3]) -> bool {
    if k.iter().any(|v| !v.is_finite() || v.abs() > 10.0) {
        return false;
    }
    let [a, b, c] = k;
    let mut at = vec![0.0, 1.0];
    // Extrema of cubic P(t), t=r² in [0,1], using its quadratic derivative.
    if c != 0.0 {
        let disc = b * b - 3.0 * c * a;
        if disc >= 0.0 {
            // Stable quadratic roots avoid cancellation hiding an interior minimum when c
            // is tiny but nonzero. Derivative is a + 2*b*t + 3*c*t².
            let q = -b - disc.sqrt().copysign(b);
            if q != 0.0 {
                at.extend([q / (3.0 * c), a / q]);
            } else {
                at.push(0.0);
            }
        }
    } else if b != 0.0 {
        at.push(-a / (2.0 * b));
    }
    at.into_iter().filter(|t| (0.0..=1.0).contains(t)).all(|t| (0.025..=1.5).contains(&(1.0 + t * (a + t * (b + t * c)))))
}

/// Stable noncryptographic identity for accidental metadata mismatch, not an authentication key.
/// All four capture facts participate; no body-only crop inference. None and zero remain distinct.
pub fn capture_key(camera: &str, lens: &str, focal: Option<f32>, aperture: Option<f32>) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for bytes in [camera.as_bytes(), lens.as_bytes()] {
        for b in bytes.iter().copied().chain([0xff]) {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    for value in [focal, aperture] {
        for b in [u8::from(value.is_some())].into_iter().chain(value.unwrap_or_default().to_bits().to_le_bytes()) {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    h.max(1)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistortionRow {
    pub focal_mm: f64,
    pub model: RadialDistortion,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaRow {
    pub focal_mm: f64,
    pub model: LateralCa,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VignetteRow {
    pub focal_mm: f64,
    pub aperture: f64,
    pub focus_m: f64,
    pub k: [f64; 3],
}

/// User-loadable JSON data uses the same schema as the bundled, attributed database subset.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LensProfile {
    pub id: String,
    pub maker: String,
    pub mount: String,
    pub aliases: Vec<String>,
    pub crop_factor: f64,
    pub aspect_ratio: f64,
    pub source: String,
    pub license: String,
    pub distortion: Vec<DistortionRow>,
    pub tca: Vec<CaRow>,
    pub vignetting: Vec<VignetteRow>,
}

pub fn same_measurement(a: f64, b: f64) -> bool {
    a.is_finite() && b.is_finite() && (a - b).abs() <= 1e-5 * a.abs().max(b.abs()).max(1.0)
}

impl LensProfile {
    pub fn matches(&self, lens: &str) -> bool {
        self.aliases.iter().any(|a| a.trim().eq_ignore_ascii_case(lens.trim()))
    }

    pub fn validate(&self) -> Result<(), String> {
        let text = [&self.id, &self.maker, &self.mount, &self.source, &self.license];
        if text.iter().any(|t| t.trim().is_empty() || t.len() > 4096)
            || self.aliases.is_empty()
            || self.aliases.len() > 32
            || self.aliases.iter().any(|a| a.trim().is_empty() || a.len() > 512)
            || self.distortion.len() + self.tca.len() + self.vignetting.len() > 2048
        {
            return Err("invalid profile identity, provenance, or row count".into());
        }
        for (i, a) in self.aliases.iter().enumerate() {
            if self.aliases.iter().take(i).any(|b| b.trim().eq_ignore_ascii_case(a.trim())) {
                return Err("duplicate lens alias".into());
            }
        }
        let base = CalibratedLens {
            capture_key: CaptureIdentity { metadata: 1, content: [1, 0] },
            crop_factor: self.crop_factor,
            aspect_ratio: self.aspect_ratio,
            distortion: None,
            tca: None,
            vignetting: None,
        };
        if !base.valid() {
            return Err("invalid profile crop/aspect".into());
        }
        let valid_focal = |f: f64| f.is_finite() && (1.0..=2000.0).contains(&f);
        for (i, row) in self.distortion.iter().enumerate() {
            if !valid_focal(row.focal_mm)
                || !(CalibratedLens { distortion: Some(row.model), ..base }).valid()
                || self.distortion.iter().take(i).any(|other| same_measurement(row.focal_mm, other.focal_mm))
            {
                return Err("invalid or ambiguous distortion row".into());
            }
        }
        for (i, row) in self.tca.iter().enumerate() {
            if !valid_focal(row.focal_mm)
                || !(CalibratedLens { tca: Some(row.model), ..base }).valid()
                || self.tca.iter().take(i).any(|other| same_measurement(row.focal_mm, other.focal_mm))
            {
                return Err("invalid or ambiguous TCA row".into());
            }
        }
        for (i, row) in self.vignetting.iter().enumerate() {
            if !valid_focal(row.focal_mm)
                || !row.aperture.is_finite()
                || !(0.5..=128.0).contains(&row.aperture)
                || !row.focus_m.is_finite()
                || !(0.01..=1e6).contains(&row.focus_m)
                || !valid_attenuation(row.k)
                || self.vignetting.iter().take(i).any(|other| {
                    same_measurement(row.focal_mm, other.focal_mm)
                        && same_measurement(row.aperture, other.aperture)
                        && same_measurement(row.focus_m, other.focus_m)
                })
            {
                return Err("invalid or ambiguous vignetting row".into());
            }
        }
        Ok(())
    }
}

/// CC BY-SA 3.0 database data only; see assets/lens-profiles for provenance and licence.
pub fn bundled() -> Result<Vec<LensProfile>, String> {
    let profiles: Vec<LensProfile> = serde_json::from_str(include_str!("../../../assets/lens-profiles/sony-e.json")).map_err(|e| e.to_string())?;
    for p in &profiles {
        p.validate()?;
    }
    Ok(profiles)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_rows_are_valid_and_exact() {
        let p = bundled().unwrap();
        assert_eq!(p.len(), 6);
        let p = p.iter().find(|p| p.id == "sony-fe-50-f18").unwrap();
        assert_eq!(p.distortion[0].model, RadialDistortion::Poly3 { k1: -0.00109 });
        assert_eq!(p.tca[0].model.red, [-0.0000433, 0.0, 1.0001119]);
        assert!(p.matches("FE 50mm f/1.8"));
        assert!(!p.matches("50mm"));
        let mut p = p.clone();
        p.distortion.push(p.distortion[0].clone());
        assert!(p.validate().is_err());
    }
    #[test]
    fn actual_sony_kit_has_only_exact_licensed_rows_and_no_converter_substitution() {
        let profiles = bundled().unwrap();
        assert_eq!(profiles.iter().map(|p| p.distortion.len() + p.tca.len() + p.vignetting.len()).sum::<usize>(), 138);
        for (alias, focal, aperture_rows, distances) in [
            ("FE 14mm f/1.8 GM", 14.0, vec![1.8, 2.0, 2.8, 4.0, 5.6, 8.0, 11.0, 16.0], vec![10.0, 1000.0]),
            ("FE 24mm f/1.4 GM", 24.0, vec![1.4, 2.0, 2.8, 4.0, 16.0], vec![0.2, 0.5, 1.1, 1000.0]),
            ("FE 50mm f/1.4 GM", 50.0, vec![], vec![]),
        ] {
            let p = profiles.iter().find(|p| p.matches(alias)).unwrap();
            assert_eq!(p.crop_factor, 1.0);
            assert_eq!(p.aspect_ratio, 1.5);
            assert_eq!(p.distortion.len(), 1);
            assert_eq!(p.tca.len(), 1);
            assert_eq!(p.distortion[0].focal_mm, focal);
            assert_eq!(p.tca[0].focal_mm, focal);
            assert_eq!(p.vignetting.len(), aperture_rows.len() * distances.len());
            for aperture in &aperture_rows {
                for focus in &distances {
                    assert!(p.vignetting.iter().any(|r| r.focal_mm == focal && r.aperture == *aperture && r.focus_m == *focus));
                }
            }
            // No base-lens coefficient is silently scaled to the converter's effective focal.
            assert!(!p.distortion.iter().any(|r| r.focal_mm == focal * 2.0));
            assert!(!p.tca.iter().any(|r| r.focal_mm == focal * 2.0));
        }
        for unsupported in [
            "FE 70-200mm f/2.8 GM OSS II",
            "FE 300mm f/2.8 GM OSS",
            "Viltrox TC2.0X",
            "FE 70-200mm f/2.8 GM OSS II + Viltrox TC2.0X",
            "FE 300mm f/2.8 GM OSS + Viltrox TC2.0X",
            "FE 50mm f/1.4",
            "FE 50mm f/1.4 ZA",
            "FE 50mm f/1.4 GM + Viltrox TC2.0X",
        ] {
            assert!(!profiles.iter().any(|p| p.matches(unsupported)), "{unsupported}");
        }
        let p = profiles.iter().find(|p| p.matches("FE 14mm f/1.8 GM")).unwrap();
        assert_eq!(p.distortion[0].model, RadialDistortion::Ptlens { a: 0.0173275, b: -0.0626253, c: 0.0548364 });
        let p = profiles.iter().find(|p| p.matches("FE 50mm f/1.4 GM")).unwrap();
        assert_eq!(p.tca[0].model.red, [0.0001813, -0.0003122, 1.0001816]);
    }
    #[test]
    fn sony_official_names_and_codes_resolve_only_the_same_exact_gm_prime() {
        let profiles = bundled().unwrap();
        for (id, name, code) in [
            ("sony-fe-14-f18-gm", "FE 14mm F1.8 GM", "SEL14F18GM"),
            ("sony-fe-24-f14-gm", "FE 24mm F1.4 GM", "SEL24F14GM"),
            ("sony-fe-50-f14-gm", "FE 50mm F1.4 GM", "SEL50F14GM"),
        ] {
            for alias in [name, code] {
                let matched: Vec<_> = profiles.iter().filter(|p| p.matches(alias)).collect();
                assert_eq!(matched.len(), 1, "{alias}");
                assert_eq!(matched[0].id, id);
            }
        }
        for alias in ["FE 50mm F1.4 ZA", "SEL50F14Z", "FE 50mm F1.4", "FE 50mm F1.4 GM + Viltrox TC-2.0X", "SEL70200GM2", "SEL300F28GM"] {
            assert!(!profiles.iter().any(|p| p.matches(alias)), "{alias}");
        }
    }
    #[test]
    fn invalid_polynomials_refused_even_between_endpoints() {
        assert!(!valid_attenuation([-8.0, 8.0, 0.0]));
        assert!(!valid_attenuation([-8.0, 8.0, 1e-300])); // endpoints positive, midpoint negative
        assert!(!RadialDistortion::Poly3 { k1: -1.0 }.valid(2.0));
        assert!(!valid_attenuation([f64::NAN, 0.0, 0.0]));
    }
    #[test]
    fn capture_identity_tracks_body_lens_focal_and_aperture() {
        let key = capture_key("ILCE-7M4", "FE 50mm f/1.8", Some(50.0), Some(4.0));
        for (camera, lens, focal, ap) in [
            ("ILCE-7M3", "FE 50mm f/1.8", Some(50.0), Some(4.0)),
            ("ILCE-7M4", "other", Some(50.0), Some(4.0)),
            ("ILCE-7M4", "FE 50mm f/1.8", Some(49.0), Some(4.0)),
            ("ILCE-7M4", "FE 50mm f/1.8", Some(50.0), None),
        ] {
            assert_ne!(key, capture_key(camera, lens, focal, ap));
        }
    }
}
