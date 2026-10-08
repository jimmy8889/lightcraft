# Sony A1 II verification corpus

The fork targets the Sony A1 II (`ILCE-1M2`). Nine individually CC0-licensed
samples from raw.pixls.us were downloaded and verified against the site's
published SHA256 digests on 2026-10-08. Exact URLs, variants and hashes are in
[A1-II-CORPUS.json](A1-II-CORPUS.json). Media stays outside Git.

Each file passed the real CLI import, native RAW strength 60 and
`photo.exportProcessedDng` path. Before publication, the engine decoded the
cleaned DNG and compared every cleaned sample, active area, CFA/linear-RGB
interpretation and black/white levels. SHA256 checks confirmed each original
remained unchanged. Pixel counts below include masked sensor samples; linear
RGB counts contain three samples per pixel.

| Sample | Capture variant as submitted | Decoded storage | Sensor samples | CLI seconds |
|---|---|---|---:|---:|
| 7827 | Uncompressed full-frame | bayer-sensor | 50,158,848 | 7.545 |
| 7828 | Lossless compressed Large-Size full-frame Sony Alpha1 II | bayer-sensor | 53,477,376 | 7.728 |
| 7829 | Lossless compressed Medium-Size Sony Alpha1 II | linear-rgb | 69,206,016 | 11.064 |
| 7830 | Lossless compressed Small-Size full-frame Sony Alpha1 II | linear-rgb | 42,467,328 | 6.561 |
| 7831 | Lossy compressed full-frame | bayer-sensor | 50,158,848 | 7.900 |
| 7832 | Uncompressed aps-c | bayer-sensor | 21,341,952 | 4.564 |
| 7833 | Lossless compressed Medium-Size APS-C mode Sony Alpha1 II | bayer-sensor | 25,165,824 | 3.151 |
| 7834 | Lossless compressed Small-Size APS-C mode Sony Alpha1 II | linear-rgb | 42,467,328 | 5.700 |
| 7835 | Lossy compressed APS-C | bayer-sensor | 21,341,952 | 2.680 |

Timings include import and cleaned-DNG export on the Linux development host
while other builds ran. They establish neither GPU speed nor image-quality
parity with DxO. The test does not establish every camera setting, lens/body
combination, Sony colour calibration or external-editor interpretation.

The running headless interface also opened an in-place folder of A1 II samples,
showed natural filename ordering and camera-JPEG Fit, saved a four-star rating,
rejected and restored an ARW/XMP/DOP group with X/Ctrl+Z, inspected developed RAW
at physical 100%, and completed a cancellable background cleaned-DNG job.
Original ARW and DOP hashes remained unchanged. The 50 MP full-resolution CPU
render took approximately 16.3 seconds under build load; no GPU adapter was
available. Ratings/native settings were saved in XMP. This was a test folder,
not the user's personal media or hardware.

The integrated optional-backend build also ran the native source-versus-derivative
render acceptance on all nine files. Reopened preview RGB differences were zero;
white-balance mode, tone curve and original bytes matched. No file-local camera-JPEG
look was accepted for these samples. This verifies preservation of LightCraft's
source interpretation, not measured Sony colour accuracy or DxO output quality.

## Reproduce native processing

Download the pinned files into a disposable local folder and verify each SHA256
from the manifest before using it. An isolated test library and fresh output
filenames avoid touching an existing photo workflow:

```sh
cargo run -p lightcraft-cli -- run --library /scratch/a1-test-library \
  --import /scratch/a1-raw/7827-ILCE-1M2.ARW \
  photo.rawDenoise strength=60 photo.exportProcessedDng
```

Repeat with each sample. Verify the source SHA256 afterwards. The output goes
to `Processed/<stem>-Clean.dng`; occupied destinations are refused, and no
original is overwritten. On Windows use normal paths and equivalent PowerShell
environment assignments. Windows runtime, installer, real NTFS recovery and
GPU acceptance remain separate checks from Linux execution/cross-compilation.
