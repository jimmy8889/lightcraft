//! Classical, edge-aware RAW noise reduction, before demosaic and white balance.
//!
//! Bayer samples use only the same CFA phase (including separate green phases). Linear RGB
//! uses each already-demosaiced channel independently; it does not apply white balance again.
//! This is a native baseline, not a learned model or a claim of DeepPRIME quality. Sensor
//! metadata, optically masked borders and clipped highlights are retained unchanged.

use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{RawData, RawError, RawImage, Result};

/// Bump whenever algorithm output changes. This participates in source/render cache keys.
pub const VERSION: u32 = 1;
/// Bounds the extra output buffer (512 MiB U16 / 1 GiB F32), independent of file headers.
pub const MAX_DENOISE_SAMPLES: usize = 1 << 28;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    /// 0 disables processing, 100 fully blends the edge-aware estimate.
    pub strength: f32,
}

impl Default for Options {
    fn default() -> Self {
        Self { strength: 0.0 }
    }
}

impl Options {
    pub fn validate(self) -> Result<()> {
        if !self.strength.is_finite() || !(0.0..=100.0).contains(&self.strength) {
            return Err(RawError::Corrupt("RAW denoise strength must be finite and within 0..100".into()));
        }
        Ok(())
    }
    /// Deterministic settings/algorithm fingerprint. Disabled processing keeps the old key.
    pub fn cache_key(self) -> u64 {
        if self.strength == 0.0 { 0 } else { ((VERSION as u64) << 32) | self.strength.to_bits() as u64 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub algorithm: &'static str,
    pub version: u32,
    pub mode: &'static str,
    pub strength: f32,
    /// Automatically estimated noise standard deviation in black-subtracted, white-scaled units.
    pub sigma: Vec<f32>,
    pub samples: usize,
}

fn sample(raw: &RawImage, x: usize, y: usize, s: usize) -> f32 {
    let index = (y * raw.width + x) * raw.cpp + s;
    match &raw.data {
        RawData::U16(v) => v.get(index).copied().unwrap_or(0) as f32,
        RawData::F32(v) => v.get(index).copied().unwrap_or(0.0),
    }
}

fn black(raw: &RawImage, x: usize, y: usize, s: usize) -> f32 {
    raw.black.at(x - raw.active_area.x, y - raw.active_area.y, s, raw.cpp)
}

fn normalized(raw: &RawImage, x: usize, y: usize, s: usize, range: f32) -> f32 {
    (sample(raw, x, y, s) - black(raw, x, y, s)) / range
}

/// Reject non-finite data and invalid level models before allocating or touching samples.
pub(crate) fn validate(raw: &RawImage) -> Result<()> {
    raw.validate()?;
    let count = raw.width.checked_mul(raw.height).and_then(|n| n.checked_mul(raw.cpp)).ok_or(RawError::Limit("RAW denoise dimensions"))?;
    if count > MAX_DENOISE_SAMPLES {
        return Err(RawError::Limit("RAW denoise sample count"));
    }
    if let Some(cfa) = &raw.cfa {
        if raw.cpp != 1 || !cfa.is_bayer() {
            return Err(RawError::Unsupported("native RAW denoise supports Bayer CFA or linear RGB; this CFA is unsupported".into()));
        }
    } else if raw.cpp != 3 && raw.cpp != 1 {
        return Err(RawError::Unsupported("native RAW denoise requires one sensor plane or three linear RGB channels".into()));
    }
    let b = &raw.black;
    let n = b.repeat_rows.checked_mul(b.repeat_cols).and_then(|v| v.checked_mul(raw.cpp)).ok_or(RawError::Limit("RAW black pattern"))?;
    if n == 0 || n > 1024 || (b.values.len() != n && b.values.len() != 1) {
        return Err(RawError::Corrupt("invalid RAW denoise black-level pattern".into()));
    }
    if b.values.iter().chain(&b.delta_h).chain(&b.delta_v).any(|v| !v.is_finite())
        || (0..raw.cpp).any(|s| !raw.white_at(s).is_finite() || raw.white_at(s) - b.mean() < 1e-12)
        || matches!(&raw.data, RawData::F32(v) if v.iter().any(|x| !x.is_finite()))
    {
        return Err(RawError::Corrupt("non-finite RAW samples or invalid black/white levels".into()));
    }
    // Extreme deltas must not overflow normalization or the reconstructed sample.
    if b.values.iter().chain(&b.delta_h).chain(&b.delta_v).any(|v| v.abs() > 1e12)
        || (0..raw.cpp).any(|s| raw.white_at(s).abs() > 1e12)
        || matches!(&raw.data, RawData::F32(v) if v.iter().any(|x| x.abs() > 1e12))
    {
        return Err(RawError::Corrupt("RAW samples or levels exceed processing range".into()));
    }
    Ok(())
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) { Err(RawError::Unsupported("RAW denoise cancelled".into())) } else { Ok(()) }
}

/// Estimate each phase/channel from a bounded set of nearest-neighbour same-plane differences.
/// A lower quartile reduces sensitivity to real edges; no calibrated camera noise model is implied.
fn estimate(raw: &RawImage, stride: usize, ranges: &[f32], cancel: &AtomicBool) -> Result<Vec<f32>> {
    let planes = if raw.cfa.is_some() { 4 } else { raw.cpp };
    let mut differences: Vec<Vec<f32>> = (0..planes).map(|_| Vec::with_capacity(8192)).collect();
    let a = raw.active_area;
    // Deterministic subsampling, capped at 8192 observations per phase/channel.
    let pitch = ((a.width.saturating_mul(a.height) / 8192).max(1) as f64).sqrt().ceil() as usize;
    let pitch = pitch.max(1).div_ceil(stride) * stride;
    for py in 0..stride {
        for px in 0..stride {
            for y in (a.y + py..a.y + a.height).step_by(pitch) {
                cancelled(cancel)?;
                for x in (a.x + px..a.x + a.width).step_by(pitch) {
                    for (s, &range) in ranges.iter().enumerate() {
                        let plane = if raw.cfa.is_some() { (y % 2) * 2 + x % 2 } else { s };
                        let Some(d) = differences.get_mut(plane) else { continue };
                        if d.len() >= 8192 {
                            continue;
                        }
                        let v = normalized(raw, x, y, s, range);
                        if v >= 0.98 {
                            continue;
                        }
                        for (nx, ny) in [(x.saturating_add(stride), y), (x, y.saturating_add(stride))] {
                            if nx >= a.x + a.width || ny >= a.y + a.height {
                                continue;
                            }
                            let q = normalized(raw, nx, ny, s, range);
                            if q < 0.98 && d.len() < 8192 {
                                d.push((v - q).abs());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(differences
        .into_iter()
        .map(|mut d| {
            if d.len() < 8 {
                return 0.0;
            }
            let i = d.len() / 4;
            let (_, q, _) = d.select_nth_unstable_by(i, f32::total_cmp);
            // For Gaussian noise abs(X-Y)'s 25th percentile is ~0.4506 sigma.
            // Cap uncalibrated estimates to avoid treating strong texture as a huge noise level.
            (*q / 0.4506).clamp(0.0, 0.04)
        })
        .collect())
}

fn filtered(raw: &RawImage, x: usize, y: usize, s: usize, stride: usize, sigma: f32, range: f32, blend: f32) -> f32 {
    let original = sample(raw, x, y, s);
    let center = normalized(raw, x, y, s, range);
    // Retain saturated highlights bit-for-bit, including above-white headroom.
    if center >= 0.98 || sigma < 1e-8 {
        return original;
    }
    let a = raw.active_area;
    let mut weighted = center;
    let mut total = 1.0;
    let cutoff = (3.5 * sigma).max(1e-8);
    for dy in [-1isize, 0, 1] {
        for dx in [-1isize, 0, 1] {
            if dx == 0 && dy == 0 {
                continue;
            }
            let Some(nx) = x.checked_add_signed(dx * stride as isize) else { continue };
            let Some(ny) = y.checked_add_signed(dy * stride as isize) else { continue };
            if nx < a.x || ny < a.y || nx >= a.x + a.width || ny >= a.y + a.height {
                continue;
            }
            let q = normalized(raw, nx, ny, s, range);
            if q >= 0.98 {
                continue;
            }
            let u = (q - center).abs() / cutoff;
            if u >= 1.0 {
                continue;
            }
            let spatial = if dx != 0 && dy != 0 { 0.5 } else { 0.75 };
            let weight = spatial * (1.0 - u * u).powi(2);
            weighted += weight * q;
            total += weight;
        }
    }
    let result = center + blend * (weighted / total - center);
    result * range + black(raw, x, y, s)
}

/// Process into a second buffer, then swap only after success. Cancellation leaves `raw` intact.
/// All metadata and masked-border samples stay unchanged. Source/export must use identical options.
pub fn process(raw: &mut RawImage, options: Options, cancel: &AtomicBool) -> Result<Report> {
    options.validate()?;
    validate(raw)?;
    cancelled(cancel)?;
    let stride = if raw.cfa.is_some() { 2 } else { 1 };
    let ranges: Vec<f32> = (0..raw.cpp).map(|s| raw.white_at(s) - raw.black.mean()).collect();
    let sigma = if options.strength > 0.0 { estimate(raw, stride, &ranges, cancel)? } else { vec![0.0; if raw.cfa.is_some() { 4 } else { raw.cpp }] };
    if sigma.iter().any(|v| !v.is_finite()) {
        return Err(RawError::Corrupt("non-finite RAW denoise noise estimate".into()));
    }
    let report = Report {
        algorithm: "native-phase-bilateral",
        version: VERSION,
        mode: if raw.cfa.is_some() {
            "bayer-sensor"
        } else if raw.cpp == 3 {
            "linear-rgb"
        } else {
            "monochrome"
        },
        strength: options.strength,
        sigma: sigma.clone(),
        samples: raw.data.len(),
    };
    if options.strength == 0.0 {
        return Ok(report);
    }
    let a = raw.active_area;
    let row_len = raw.width * raw.cpp;
    let calc = |y: usize, i: usize, old: f32| {
        let (x, s) = (i / raw.cpp, i % raw.cpp);
        if x < a.x || y < a.y || x >= a.x + a.width || y >= a.y + a.height {
            return old;
        }
        let plane = if raw.cfa.is_some() { (y % 2) * 2 + x % 2 } else { s };
        filtered(raw, x, y, s, stride, sigma.get(plane).copied().unwrap_or(0.0), ranges.get(s).copied().unwrap_or(1.0), options.strength / 100.0)
    };
    let data = match &raw.data {
        RawData::U16(input) => {
            let mut out = Vec::new();
            out.try_reserve_exact(input.len()).map_err(|_| RawError::Limit("RAW denoise output allocation"))?;
            out.extend_from_slice(input);
            out.par_chunks_mut(row_len).enumerate().try_for_each(|(y, row)| -> Result<()> {
                cancelled(cancel)?;
                for (i, v) in row.iter_mut().enumerate() {
                    let result = calc(y, i, *v as f32);
                    if !result.is_finite() {
                        return Err(RawError::Corrupt("non-finite RAW denoise output".into()));
                    }
                    *v = result.round().clamp(0.0, 65535.0) as u16;
                }
                Ok(())
            })?;
            RawData::U16(out)
        }
        RawData::F32(input) => {
            let mut out = Vec::new();
            out.try_reserve_exact(input.len()).map_err(|_| RawError::Limit("RAW denoise output allocation"))?;
            out.extend_from_slice(input);
            out.par_chunks_mut(row_len).enumerate().try_for_each(|(y, row)| -> Result<()> {
                cancelled(cancel)?;
                for (i, v) in row.iter_mut().enumerate() {
                    let result = calc(y, i, *v);
                    if !result.is_finite() {
                        return Err(RawError::Corrupt("non-finite RAW denoise output".into()));
                    }
                    *v = result;
                }
                Ok(())
            })?;
            RawData::F32(out)
        }
    };
    cancelled(cancel)?;
    raw.data = data;
    Ok(report)
}
