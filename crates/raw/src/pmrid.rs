//! Opt-in experimental PMRID inference; camera calibration is required, never inferred from ISO.
//!
//! Architecture adapted from MegEngine/PMRID `models/net_torch.py`, Apache-2.0,
//! commit 8ebb9e8e96559881dee957f34243933c5beb77dd. Official checkpoint is supplied by the
//! caller, verified before parsing, and never downloaded or bundled by this module.
//! Modified by LightCraft contributors (2026): translated to Rust/Candle, immutable loading,
//! explicit camera calibration, bounded overlapping tiles, cancellation and RAW reconstruction.
//! This file is Apache-2.0 only; see LICENSE-APACHE and NOTICE.
//! CPU inference is pure Rust. This research model is not DeepPRIME or verified Sony quality.

use crate::{RawData, RawError, RawImage, Result};
use candle_core::{
    DType, Device, Tensor,
    pickle::{Object, Stack},
};
use candle_nn::{Conv2d, Conv2dConfig, ConvTranspose2d, ConvTranspose2dConfig, Module, VarBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{BufReader, Cursor, Read},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

pub const VERSION: u32 = 1;
pub const CHECKPOINT_SHA256: &str = "9361614f3514d27351d81909f2215c0fdc38619c0288d936b7266485ac106c14";
pub const UPSTREAM_COMMIT: &str = "8ebb9e8e96559881dee957f34243933c5beb77dd";
const CHECKPOINT_BYTES: usize = 4_183_200;
const MAX_PACKED_EDGE: usize = 256;
// Fixed training anchor only: upstream OPPO Reno10x ISO1600 physical calibration / V959.
// User camera coefficients are independent normalized RAW units and never use this ISO fit.
const ANCHOR_SHOT: f64 = (0.0005995267 * 1600.0 + 0.00868861) / 959.0;
const ANCHOR_READ: f64 = (7.11772e-7 * 1600.0 * 1600.0 + 6.514934e-4 * 1600.0 + 0.11492713) / (959.0 * 959.0);

fn err(e: impl std::fmt::Display) -> RawError {
    RawError::Unsupported(format!("experimental PMRID: {e}"))
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) { Err(err("cancelled")) } else { Ok(()) }
}

/// Measured normalized variance model: variance(signal) = shot * signal + read_variance.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationPoint {
    pub iso: u32,
    pub shot: f64,
    pub read_variance: f64,
}

/// Explicit user calibration, not a camera profile distributed or estimated by LightCraft.
/// Adjacent measured points interpolate linearly in ISO; extrapolation is forbidden.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoiseProfile {
    pub schema: u32,
    pub camera_model: String,
    pub points: Vec<CalibrationPoint>,
}

impl NoiseProfile {
    pub fn validate(&self) -> Result<()> {
        if self.schema != 1
            || self.camera_model.trim().is_empty()
            || self.camera_model.len() > 128
            || self.points.is_empty()
            || self.points.len() > 64
        {
            return Err(err("noise profile schema/model/point count is invalid"));
        }
        let mut last = 0;
        for point in &self.points {
            if point.iso <= last
                || point.iso > 1_000_000
                || !point.shot.is_finite()
                || !(1e-8..=0.1).contains(&point.shot)
                || !point.read_variance.is_finite()
                || !(0.0..=0.01).contains(&point.read_variance)
            {
                return Err(err("noise calibration requires increasing ISO and finite normalized shot/read variance"));
            }
            last = point.iso;
        }
        Ok(())
    }
    pub fn at(&self, camera_model: &str, iso: u32) -> Result<(f64, f64)> {
        self.validate()?;
        if camera_model.trim() != self.camera_model.trim() {
            return Err(err("noise profile does not match the RAW camera model"));
        }
        let first = self.points.first().ok_or_else(|| err("no noise calibration"))?;
        let last = self.points.last().ok_or_else(|| err("no noise calibration"))?;
        if iso < first.iso || iso > last.iso {
            return Err(err("RAW ISO is outside the calibrated noise profile domain"));
        }
        for pair in self.points.windows(2) {
            let (Some(a), Some(b)) = (pair.first(), pair.get(1)) else { continue };
            if (a.iso..=b.iso).contains(&iso) {
                let t = (iso - a.iso) as f64 / (b.iso - a.iso) as f64;
                return Ok((a.shot + (b.shot - a.shot) * t, a.read_variance + (b.read_variance - a.read_variance) * t));
            }
        }
        Ok((first.shot, first.read_variance))
    }

    /// Stable provider/settings key, including model, algorithm, calibration, ISO and tiling.
    pub fn cache_key(&self, iso: u32, options: Options) -> Result<String> {
        self.at(&self.camera_model, iso)?;
        options.validate()?;
        let mut hash = Sha256::new();
        hash.update(b"lightcraft-experimental-pmrid-v1");
        hash.update(VERSION.to_le_bytes());
        hash.update(CHECKPOINT_SHA256.as_bytes());
        hash.update((self.camera_model.trim().len() as u32).to_le_bytes());
        hash.update(self.camera_model.trim().as_bytes());
        hash.update(iso.to_le_bytes());
        hash.update(options.strength.to_bits().to_le_bytes());
        hash.update((options.tile_core as u32).to_le_bytes());
        hash.update((options.halo as u32).to_le_bytes());
        for p in &self.points {
            hash.update(p.iso.to_le_bytes());
            hash.update(p.shot.to_bits().to_le_bytes());
            hash.update(p.read_variance.to_bits().to_le_bytes());
        }
        Ok(format!("{:x}", hash.finalize()))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub strength: f32,
    pub tile_core: usize,
    pub halo: usize,
}
impl Default for Options {
    fn default() -> Self {
        Self { strength: 100.0, tile_core: 128, halo: 64 }
    }
}
impl Options {
    pub fn validate(self) -> Result<()> {
        crate::denoise::Options { strength: self.strength }.validate()?;
        let edge = self.halo.checked_mul(2).and_then(|n| n.checked_add(self.tile_core)).ok_or_else(|| err("tile dimensions overflow"))?;
        if self.tile_core < 16
            || self.tile_core > 128
            || self.halo < 16
            || !self.tile_core.is_multiple_of(16)
            || !self.halo.is_multiple_of(16)
            || edge > MAX_PACKED_EDGE
        {
            return Err(err("packed tile core/halo must be multiples of 16, core 16..128, halo >=16, total edge <=256"));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub algorithm: &'static str,
    pub version: u32,
    pub checkpoint_sha256: &'static str,
    pub camera_model: String,
    pub iso: u32,
    pub shot: f64,
    pub read_variance: f64,
    pub strength: f32,
    pub tiles: usize,
    pub packed_core: usize,
    pub packed_halo: usize,
}

struct Conv {
    spatial: Conv2d,
    point: Option<Conv2d>,
    relu: bool,
}
impl Conv {
    fn new(vb: VarBuilder<'_>, input: usize, output: usize, kernel: usize, stride: usize, separable: bool, relu: bool) -> candle_core::Result<Self> {
        let cfg = Conv2dConfig { stride, padding: kernel / 2, groups: if separable { input } else { 1 }, ..Default::default() };
        let spatial = if separable {
            candle_nn::conv2d_no_bias(input, input, kernel, cfg, vb.pp("depthwise"))?
        } else {
            candle_nn::conv2d(input, output, kernel, cfg, vb.pp("conv"))?
        };
        let point = if separable { Some(candle_nn::conv2d(input, output, 1, Default::default(), vb.pp("pointwise"))?) } else { None };
        Ok(Self { spatial, point, relu })
    }
    fn forward(&self, x: &Tensor) -> candle_core::Result<Tensor> {
        let mut x = self.spatial.forward(x)?;
        if let Some(point) = &self.point {
            x = point.forward(&x)?;
        }
        if self.relu { x.relu() } else { Ok(x) }
    }
}
struct Encoder {
    a: Conv,
    b: Conv,
    project: Option<Conv>,
}
impl Encoder {
    fn new(vb: VarBuilder<'_>, input: usize, output: usize, stride: usize) -> candle_core::Result<Self> {
        Ok(Self {
            a: Conv::new(vb.pp("conv1"), input, output / 4, 5, stride, true, true)?,
            b: Conv::new(vb.pp("conv2"), output / 4, output, 5, 1, true, false)?,
            project: if stride == 1 && input == output { None } else { Some(Conv::new(vb.pp("proj"), input, output, 3, stride, true, false)?) },
        })
    }
    fn forward(&self, x: &Tensor) -> candle_core::Result<Tensor> {
        let shortcut = match &self.project {
            Some(c) => c.forward(x)?,
            None => x.clone(),
        };
        (self.b.forward(&self.a.forward(x)?)? + shortcut)?.relu()
    }
}
struct Residual {
    a: Conv,
    b: Conv,
}
impl Residual {
    fn new(vb: VarBuilder<'_>, channels: usize) -> candle_core::Result<Self> {
        Ok(Self {
            a: Conv::new(vb.pp("conv0"), channels, channels, 3, 1, true, true)?,
            b: Conv::new(vb.pp("conv1"), channels, channels, 3, 1, true, false)?,
        })
    }
    fn forward(&self, x: &Tensor) -> candle_core::Result<Tensor> {
        self.b.forward(&self.a.forward(x)?)? + x
    }
}
struct Decoder {
    residual: Residual,
    up: ConvTranspose2d,
    skip: Conv,
}
impl Decoder {
    fn new(vb: VarBuilder<'_>, input: usize, skip: usize, output: usize) -> candle_core::Result<Self> {
        Ok(Self {
            residual: Residual::new(vb.pp("decode_conv"), input)?,
            up: candle_nn::conv_transpose2d(input, output, 2, ConvTranspose2dConfig { stride: 2, ..Default::default() }, vb.pp("upsample"))?,
            skip: Conv::new(vb.pp("proj_conv"), skip, output, 3, 1, true, true)?,
        })
    }
    fn forward(&self, x: &Tensor, skip: &Tensor) -> candle_core::Result<Tensor> {
        self.up.forward(&self.residual.forward(x)?)? + self.skip.forward(skip)?
    }
}

/// Exact official network topology loaded from the immutable, SHA-verified official archive.
pub struct Model {
    first: Conv,
    enc: Vec<Vec<Encoder>>,
    bridge: Conv,
    dec: Vec<Decoder>,
    last: Residual,
    out: Conv,
}
impl Model {
    pub fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(err)?;
        if file.metadata().map_err(err)?.len() != CHECKPOINT_BYTES as u64 {
            return Err(err("checkpoint size does not match the pinned official model"));
        }
        let mut bytes = Vec::with_capacity(CHECKPOINT_BYTES);
        file.take(CHECKPOINT_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(err)?;
        Self::from_bytes(bytes)
    }
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() != CHECKPOINT_BYTES || format!("{:x}", Sha256::digest(&bytes)) != CHECKPOINT_SHA256 {
            return Err(err("checkpoint SHA256 does not match the pinned official model"));
        }
        // Do not reopen the caller's path: parse and load only the verified owned bytes.
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(err)?;
        if zip.len() > 256 {
            return Err(err("checkpoint archive entry limit"));
        }
        let object = {
            let member = zip.by_name("archive/data.pkl").map_err(err)?;
            if member.size() > 256 << 10 {
                return Err(err("checkpoint metadata limit"));
            }
            let mut stack = Stack::empty();
            stack.read_loop(&mut BufReader::new(member)).map_err(err)?;
            stack.finalize().map_err(err)?
        };
        let Object::Dict(pairs) = object else { return Err(err("checkpoint is not a tensor dictionary")) };
        // Candle exposes OrderedDict's _metadata attribute as one additional dictionary pair.
        if pairs.len() != 142 {
            return Err(err("checkpoint tensor count mismatch"));
        }
        let mut tensors = HashMap::new();
        for (name, value) in pairs {
            if name == Object::Unicode("_metadata".into()) {
                if !matches!(value, Object::Dict(ref entries) if entries.len() <= 512) {
                    return Err(err("checkpoint module metadata mismatch"));
                }
                continue;
            }
            let info = value.into_tensor_info(name, Path::new("archive/data")).map_err(err)?.ok_or_else(|| err("invalid checkpoint tensor"))?;
            let count =
                info.layout.dims().iter().try_fold(1usize, |n, &v| n.checked_mul(v)).ok_or_else(|| err("checkpoint tensor dimensions overflow"))?;
            if info.dtype != DType::F32
                || info.layout.dims().len() > 4
                || count > 1_000_000
                || !info.layout.is_contiguous()
                || info.layout.start_offset() != 0
            {
                return Err(err("unsupported checkpoint tensor layout"));
            }
            let mut member = zip.by_name(&info.path).map_err(err)?;
            if member.size() != count as u64 * 4 {
                return Err(err("checkpoint tensor length mismatch"));
            }
            let mut storage = Vec::with_capacity(count * 4);
            member.read_to_end(&mut storage).map_err(err)?;
            if storage.len() != count * 4 {
                return Err(err("checkpoint storage length mismatch"));
            }
            let weights: Vec<f32> = storage.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
            if weights.iter().any(|x| !x.is_finite()) {
                return Err(err("non-finite checkpoint weights"));
            }
            let tensor = Tensor::from_vec(weights, info.layout.shape().clone(), &Device::Cpu).map_err(err)?;
            if tensors.insert(info.name, tensor).is_some() {
                return Err(err("duplicate checkpoint tensor"));
            }
        }
        if tensors.len() != 141 {
            return Err(err("checkpoint tensor count mismatch"));
        }
        let vb = VarBuilder::from_tensors(tensors, DType::F32, &Device::Cpu);
        let first = Conv::new(vb.pp("conv0"), 4, 16, 3, 1, false, true).map_err(err)?;
        let mut enc = Vec::new();
        for (stage, (input, output, blocks)) in [(16, 64, 2), (64, 128, 2), (128, 256, 4), (256, 512, 4)].into_iter().enumerate() {
            let mut layers = Vec::new();
            for block in 0..blocks {
                layers.push(
                    Encoder::new(
                        vb.pp(format!("enc{}.{}", stage + 1, block)),
                        if block == 0 { input } else { output },
                        output,
                        if block == 0 { 2 } else { 1 },
                    )
                    .map_err(err)?,
                );
            }
            enc.push(layers);
        }
        let bridge = Conv::new(vb.pp("encdec"), 512, 64, 3, 1, true, true).map_err(err)?;
        let mut dec = Vec::new();
        for (stage, (input, skip, output)) in [(64, 256, 64), (64, 128, 32), (32, 64, 32), (32, 16, 16)].into_iter().enumerate() {
            dec.push(Decoder::new(vb.pp(format!("dec{}", stage + 1)), input, skip, output).map_err(err)?);
        }
        Ok(Self {
            first,
            enc,
            bridge,
            dec,
            last: Residual::new(vb.pp("out0"), 16).map_err(err)?,
            out: Conv::new(vb.pp("out1"), 16, 4, 3, 1, false, false).map_err(err)?,
        })
    }
    /// Raw model tensor API: NCHW four packed RGGB phases, already mapped to anchor and ×256.
    /// This exposes no camera/ISO guess; image processing uses the calibrated API below.
    pub fn infer_packed(&self, input: &[f32], height: usize, width: usize, cancel: &AtomicBool) -> Result<Vec<f32>> {
        let count = height.checked_mul(width).and_then(|n| n.checked_mul(4)).ok_or_else(|| err("input dimensions overflow"))?;
        if height < 32
            || width < 32
            || height > MAX_PACKED_EDGE
            || width > MAX_PACKED_EDGE
            || !height.is_multiple_of(16)
            || !width.is_multiple_of(16)
            || input.len() != count
            || input.iter().any(|v| !v.is_finite() || v.abs() > 1e6)
        {
            return Err(err("invalid bounded NCHW model input"));
        }
        check_cancel(cancel)?;
        let input = Tensor::from_slice(input, (1, 4, height, width), &Device::Cpu).map_err(err)?;
        let mut x = self.first.forward(&input).map_err(err)?;
        let mut skips = vec![x.clone()];
        for stage in &self.enc {
            for layer in stage {
                check_cancel(cancel)?;
                x = layer.forward(&x).map_err(err)?;
            }
            skips.push(x.clone());
        }
        x = self.bridge.forward(&x).map_err(err)?;
        for (stage, layer) in self.dec.iter().enumerate() {
            check_cancel(cancel)?;
            let skip = skips.get(3usize.saturating_sub(stage)).ok_or_else(|| err("network skip mismatch"))?;
            x = layer.forward(&x, skip).map_err(err)?;
        }
        check_cancel(cancel)?;
        x = self.last.forward(&x).map_err(err)?;
        let output = (self.out.forward(&x).map_err(err)? + input).map_err(err)?.flatten_all().map_err(err)?.to_vec1::<f32>().map_err(err)?;
        if output.len() != count || output.iter().any(|v| !v.is_finite()) {
            return Err(err("non-finite model output"));
        }
        check_cancel(cancel)?;
        Ok(output)
    }
}

/// Experimental full-resolution Bayer processing, with overlapping context discarded around
/// each tile. The model's large receptive field means small CPU tiles approximate full-frame
/// inference; no full-frame equivalence or Sony image-quality validation is claimed.
/// Masked borders, below-black samples, and clipped highlights remain exact. Swap on success only.
pub fn process(raw: &mut RawImage, model: &Model, profile: &NoiseProfile, options: Options, cancel: &AtomicBool) -> Result<Report> {
    options.validate()?;
    crate::denoise::validate(raw)?;
    let cfa = raw
        .cfa
        .as_ref()
        .filter(|c| c.is_bayer() && raw.cpp == 1)
        .ok_or_else(|| err("AI requires Bayer sensor samples; linear RGB uses native denoise"))?;
    let camera = raw.metadata.model.as_deref().ok_or_else(|| err("RAW has no camera model; an explicit matching calibration is required"))?;
    let iso = raw.metadata.iso.ok_or_else(|| err("RAW has no ISO; an explicit measured noise profile is required"))?;
    let (shot, read) = profile.at(camera, iso)?;
    let scale = ANCHOR_SHOT / shot;
    let shift = (read / (shot * shot) - ANCHOR_READ / (ANCHOR_SHOT * ANCHOR_SHOT)) * ANCHOR_SHOT;
    if !scale.is_finite() || !shift.is_finite() || scale > 1e4 || shift.abs() > 64.0 {
        return Err(err("noise calibration conversion is outside the model's supported numeric range"));
    }
    check_cancel(cancel)?;
    let a = raw.active_area;
    if a.width < 2 || a.height < 2 {
        return Err(err("Bayer active area must contain a complete CFA cell"));
    }
    let mut red = None;
    for y in 0..2 {
        for x in 0..2 {
            if cfa.color_at(a.x + x, a.y + y) == 0 {
                red = Some((x, y));
            }
        }
    }
    let (rx, ry) = red.ok_or_else(|| err("CFA has no red phase"))?;
    let phases = [(rx, ry), (rx ^ 1, ry), (rx, ry ^ 1), (rx ^ 1, ry ^ 1)];
    let (pw, ph) = (a.width.div_ceil(2), a.height.div_ceil(2));
    let range = raw.white_at(0) - raw.black.mean();
    let value = |x: usize, y: usize| (raw.data.get(y * raw.width + x) - raw.black.at(x - a.x, y - a.y, 0, 1)) / range;
    let edge = options.tile_core + options.halo * 2;
    let mut tiles = 0;
    if options.strength > 0.0 {
        let mut output = Vec::new();
        output.try_reserve_exact(raw.data.len()).map_err(|_| err("output allocation limit"))?;
        for i in 0..raw.data.len() {
            output.push(raw.data.get(i));
        }
        for by in (0..ph).step_by(options.tile_core) {
            for bx in (0..pw).step_by(options.tile_core) {
                check_cancel(cancel)?;
                let mut input = vec![0.0; 4 * edge * edge];
                for (s, &(px, py)) in phases.iter().enumerate() {
                    for ty in 0..edge {
                        for tx in 0..edge {
                            let gx = (bx + tx).saturating_sub(options.halo).min(pw - 1);
                            let gy = (by + ty).saturating_sub(options.halo).min(ph - 1);
                            let xx = phase_coordinate(gx * 2 + px, a.width) + a.x;
                            let yy = phase_coordinate(gy * 2 + py, a.height) + a.y;
                            let normalized = value(xx, yy).clamp(0.0, 1.0) as f64;
                            if let Some(v) = input.get_mut((s * edge + ty) * edge + tx) {
                                *v = ((normalized * scale + shift) * 256.0) as f32;
                            }
                        }
                    }
                }
                let prediction = model.infer_packed(&input, edge, edge, cancel)?;
                for (s, &(px, py)) in phases.iter().enumerate() {
                    for ty in 0..options.tile_core.min(ph - by) {
                        for tx in 0..options.tile_core.min(pw - bx) {
                            let (xx, yy) = ((bx + tx) * 2 + px, (by + ty) * 2 + py);
                            if xx >= a.width || yy >= a.height {
                                continue;
                            }
                            let (x, y) = (xx + a.x, yy + a.y);
                            let old = value(x, y);
                            if !(0.0..0.98).contains(&old) {
                                continue;
                            }
                            let index = (s * edge + ty + options.halo) * edge + tx + options.halo;
                            let p = *prediction.get(index).ok_or_else(|| err("tile output dimensions mismatch"))? as f64;
                            let clean = ((p / 256.0 - shift) / scale) as f32;
                            let reconstructed = (old + (clean - old) * (options.strength / 100.0)) * range + raw.black.at(xx, yy, 0, 1);
                            if !reconstructed.is_finite() {
                                return Err(err("non-finite reconstructed sensor samples"));
                            }
                            let v = output.get_mut(y * raw.width + x).ok_or_else(|| err("output index outside image"))?;
                            *v = reconstructed;
                        }
                    }
                }
                tiles += 1;
            }
        }
        check_cancel(cancel)?;
        let data = match &raw.data {
            RawData::F32(_) => RawData::F32(output),
            RawData::U16(_) => {
                let mut converted = Vec::new();
                converted.try_reserve_exact(output.len()).map_err(|_| err("integer output allocation limit"))?;
                converted.extend(output.into_iter().map(|v| v.round().clamp(0.0, 65535.0) as u16));
                RawData::U16(converted)
            }
        };
        check_cancel(cancel)?;
        raw.data = data;
    }
    Ok(Report {
        algorithm: "experimental-pmrid",
        version: VERSION,
        checkpoint_sha256: CHECKPOINT_SHA256,
        camera_model: camera.to_string(),
        iso,
        shot,
        read_variance: read,
        strength: options.strength,
        tiles,
        packed_core: options.tile_core,
        packed_halo: options.halo,
    })
}
fn phase_coordinate(index: usize, len: usize) -> usize {
    if index < len { index } else { index.saturating_sub(2) }
}
