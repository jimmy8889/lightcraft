# Experimental calibrated RAW AI denoising

The optional `pmrid` Cargo feature provides real CPU inference for the official
[PMRID network](https://github.com/MegEngine/PMRID/tree/8ebb9e8e96559881dee957f34243933c5beb77dd).
The default RAW slider uses native classical processing. PMRID is a research backend that
requires an external checkpoint and an explicit camera noise calibration. Its presence does
not establish Sony image quality, camera calibration, GPU performance or DxO DeepPRIME parity.

The Rust network port is Apache-2.0 only; see [NOTICE](../NOTICE) and [LICENSE-APACHE](../LICENSE-APACHE).
No weights or datasets are bundled or automatically downloaded. The pinned official repository
includes its checkpoint under Apache-2.0. External datasets retain their own terms. Sony Research's
[MIT RAW denoising code](https://github.com/SonyResearch/raw_image_denoising) is a separate candidate:
its externally hosted checkpoint, dark-shading and dataset rights require independent confirmation.

## Inputs and calibration

`lightcraft_raw::pmrid::Model::load(path)` accepts only the official 4,183,200-byte
`models/torch_pretrained.ckp` from commit `8ebb9e8e96559881dee957f34243933c5beb77dd`, SHA256
`9361614f3514d27351d81909f2215c0fdc38619c0288d936b7266485ac106c14`.
The loader verifies owned bytes before safe Rust parsing, then checks tensor counts, shapes,
storage lengths and finite weights. It does not reopen a mutable checkpoint path after verification.

`NoiseProfile` schema 1 has `camera_model` and `points`; each point contains integer `iso`,
positive `shot` and nonnegative `read_variance`. The calibration model is
`variance(signal) = shot * signal + read_variance`, with black-subtracted, white-scaled sensor
samples: 1 equals sensor white. These coefficients must be measured for the named camera.
Adjacent ISO points interpolate linearly; ISO outside the supplied domain errors. Model names
must match the RAW's metadata. Missing model/ISO, invalid units or numeric ranges error.

The model's fixed OPPO training anchor remains separate from the supplied camera coefficients.
The backend never applies OPPO ISO coefficients to a Sony camera and never guesses a Sony
profile. A schema-valid profile is not proof of a sound calibration; that remains an acceptance
requirement before enabling a model for routine photography.

## Processing and limits

`pmrid::process(raw, model, profile, options, cancel)` processes full-resolution Bayer samples
before demosaic and white balance. It packs R, green on the red row, green on the blue row and B
for all four Bayer layouts, including active areas with shifted origins and odd dimensions.
Masked borders, below-black samples and clipped highlights stay unchanged. Errors and cancellation
leave the original `RawImage` unchanged. Cleaned samples can use the existing lossless DNG writer.
Sony lossless M/S linear RGB is unsupported by this model and continues to use native processing.

CPU tiles have overlapping context and discard their context at reconstruction. Packed core and
halo are multiples of 16; core is 16..128, halo at least 16, total edge at most 256. The default is
core 128 plus halo 64. The model has a large receptive field, so bounded tiles approximate full-frame
inference and can show tile boundaries. GPU support, tile seam assessment, modern Sony calibration,
colour/texture quality and realistic full-resolution throughput require further validation.
Cancellation is checked between network blocks and tiles; an individual convolution completes
before observing cancellation. Native processing stays the practical default.

On the development machine, one CPU forward pass on a 256×256 packed tile took approximately
2.17 seconds while other builds were running. With the default 128-pixel packed core, a
6000×4000 sensor image requires 384 tiles: roughly 14 minutes of inference by extrapolation,
before decoding, colour interpretation or DNG writing. This is a research path, not measured
full-image throughput or a practical replacement for an accelerated commercial denoiser.

`NoiseProfile::cache_key(iso, options)` fingerprints algorithm version, checkpoint, calibration,
camera, ISO, strength and tiling. A future persistent editor provider must use this key and the same
preprocessor for previews and rendered exports. The initial AI path exports a derivative and does
not introduce persistent AI render settings.

## GPU work still required

No accelerated inference backend was added. A future provider must reuse the verified owned
checkpoint, explicit calibration, Bayer packing, protected samples, cancellation and DNG
publication checks. Accept it only after reference agreement on Linux Vulkan and Windows
hardware, plus whole-image timing, peak memory, tile-seam and colour/texture comparisons.
The current environment has no exposed GPU adapter, so neither GPU throughput nor quality
can be established here.

## Reproduce reference checks

The desktop and CLI propagate the optional `pmrid` feature. Build explicitly:

```sh
cargo run -p lightcraft --features pmrid -- /path/to/photos
cargo run -p lightcraft-cli --features pmrid -- run --library /path/to/library raw.processingCapabilities
```

With this feature compiled, the Detail panel exposes a collapsed **Experimental AI RAW denoise**
section. Supply the checkpoint and measured noise-profile paths before exporting. These staged
paths are not persisted in UI settings. The ordinary slider remains native processing. Experimental
exports use `Processed/<stem>-AI-Clean.dng` by default so they can coexist with native derivatives.
Both entry points share the strict mode parser; malformed AI input never falls back to native.

`raw.processingCapabilities` reports whether it was compiled and the required external inputs.
An explicit derivative export uses:

```json
{
  "ids": [42],
  "path": "/photos/Processed/Shot-AI.dng",
  "mode": "experimental-pmrid",
  "checkpoint": "/models/torch_pretrained.ckp",
  "noiseProfile": "/profiles/measured-camera-noise.json",
  "strength": 70,
  "tileCore": 128,
  "halo": 64
}
```

Dispatch this to `photo.exportProcessedDng`. It preserves the original and refuses existing
outputs. Profile input is bounded to 64 KiB. The cleaned file carries model/noise/tiling provenance,
original source colour interpretation and edit settings with native RAW strength reset to zero.
The engine command is synchronous for CLI/MCP callers; the desktop/control-channel host runs the same prepared jobs in the background with explicit provider status. Hosts prepare
`prepare_processed_dng_with_processor(..., Processor::ExperimentalPmrid { ... })` on the UI thread
and run the detached cancellation-aware job on a worker. The existing native preparation API
retains its behavior. No persistent AI provider is selected or exposed by default.
Publishing a processed DNG requires a filesystem with atomic hard-link creation (such as NTFS,
ext4 or Btrfs). Unsupported volumes return an error instead of using a rename that can overwrite
a concurrently created destination; export into a supported physical folder.

Development uses an isolated CPU PyTorch 2.6 and NumPy 2.2 environment. Python is absent from
the product. Fetch the pinned official architecture and checkpoint into an ignored scratch folder,
then generate independent reference outputs with:

```sh
python crates/raw/tests/pmrid_reference.py \
  --checkpoint /scratch/torch_pretrained.ckp \
  --architecture /scratch/net_torch.py --output /scratch/reference
LIGHTCRAFT_PMRID_CHECKPOINT=/scratch/torch_pretrained.ckp \
LIGHTCRAFT_PMRID_REFERENCE=/scratch/reference \
cargo test -p lightcraft-raw --features pmrid --test pmrid -- --include-ignored --nocapture
```

The script checks both upstream hashes before executing architecture code or using PyTorch's
weights-only checkpoint loader. It generates a fixed four-plane tensor plus independent cropped,
calibrated tile outputs for all four Bayer layouts. Tests compare real Rust inference with these
references and check synthetic noise, protected borders/highlights, cancellation, invalid input,
camera/ISO matching and cleaned DNG sample readback. Reference files remain outside the repository.
Windows uses equivalent environment assignments in PowerShell; both product implementations use
the same Rust API.
