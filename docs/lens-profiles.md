# Measured lens corrections

LightCraft can resolve a small licensed Sony E profile database into non-destructive develop
settings. It applies rectilinear distortion and vignetting to linear pixels; lateral colour-fringe
correction currently uses **experimental working-RGB per-channel resampling**. Sensor-channel
TCA before the camera colour matrix remains unsupported. This is not a claim of DxO optical
module quality or coverage.

The database contains FE 14mm f/1.8 GM, FE 24mm f/1.4 GM and FE 50mm f/1.4 GM,
alongside the original FE 35mm f/1.8, FE 35mm f/2.8 ZA and FE 50mm f/1.8 subset. All
bundled rows use crop factor 1 and a 3:2 capture aspect. Its data and provenance are in
[assets/lens-profiles](../assets/lens-profiles/README.md). Only documented exact lens aliases
and measured focal lengths match. Every capture needs an explicit crop-factor assertion
because the camera model alone cannot prove its capture crop mode. Vignetting also needs
an exact measured aperture and explicit focus distance. This version accepts exact rows;
no focal/aperture/focus interpolation or coefficient extrapolation.

The current kit is a Sony A1 II (`ILCE-1M2`) with the lenses below and a Viltrox Sony E
`TC-2.0X` converter. Calibrated correction coverage is:

| Lens | Distortion / experimental RGB TCA | Vignetting aperture rows | Focus rows (m) |
| --- | --- | --- | --- |
| FE 14mm f/1.8 GM | 14 mm only | 1.8, 2, 2.8, 4, 5.6, 8, 11, 16 | 10, 1000 |
| FE 24mm f/1.4 GM | 24 mm only | 1.4, 2, 2.8, 4, 16 | 0.2, 0.5, 1.1, 1000 |
| FE 50mm f/1.4 GM | 50 mm only | No measured rows | None |
| FE 70–200mm f/2.8 GM OSS II | No exact measured profile | None | None |
| FE 300mm f/2.8 GM OSS | No exact measured profile | None | None |
| Any lens + Viltrox TC-2.0X | No exact combination profile | None | None |

The 50mm profile matches **FE 50mm f/1.4 GM**, with no Zeiss/ZA/plain-50mm aliases.
The original-generation 70–200 GM profile cannot substitute for GM II. Converter
profiles are neither inferred from the bare lens nor calculated by multiplying focal
length or other coefficients by two. A converter that leaves ambiguous bare-lens EXIF
cannot be independently identified in this version; leave measured corrections disabled
for such captures. No converter calibration is currently provided.

These absences were verified across all 56 official Lensfun XML data files on 2026-10-08
at [master commit efca2062d271f7989566701ed54210cd5736f92c](https://github.com/lensfun/lensfun/tree/efca2062d271f7989566701ed54210cd5736f92c/data/db).
The Sony E data at that commit is byte-identical to the licensed bundled pin. Optical
profile coverage is independent of RAW decoder, camera-colour and denoising support;
A1 II acceptance still depends on its actual RAW variant. Missing lens data does not
disable the existing manual optics controls.

In the Optics panel, **Measured profiles** lists covered lenses. Enter the actual crop
factor and optional focus distance, then **Check** for per-effect availability.
**Apply supported** writes the requested available corrections as one Undo step.
Experimental colour-fringe correction is a separate opt-in. The panel lists the matched
profile's measured focus rows, but never fills or guesses crop/focus assertions. Draft
inputs reset for a different photo or capture identity and are not saved to `ui.json`.

All entry points are commands. These examples are the `params` object of the control channel
`engine.execute` method:

```json
{"command":"optics.profiles","params":{}}
{"command":"optics.inspectProfile","params":{"crop_factor":1}}
{"command":"optics.applyProfile","params":{"crop_factor":1,"focus_m":10,"tca":false}}
```

`optics.profiles` returns the full bundled profiles and measured rows. Inspect reports a matched
profile, a capture-level reason when unsupported, and separate `available` entries for distortion,
TCA and vignetting. Apply selects each available requested effect and returns `effects` plus reasons
for the others; missing focus distance can leave distortion/TCA enabled. Optional `distortion`,
`tca`, and `vignetting` booleans disable individual effects. No available requested effect produces
an error without changing settings. Invalid parameters, duplicate rows, folded radial curves and
nonpositive attenuation are refused.

A caller can pass `profile_data` containing one JSON profile in the same schema as a bundled
entry. This allows independently measured/user-supplied calibrations without changing the engine,
including polynomial3, polynomial5 and PTLens distortion, polynomial3/linear TCA, and PA
attenuation. Profiles must identify their maker, mount, exact aliases, crop/aspect, source and licence.
The first version accepts an explicit JSON object, not a new profile-file library or XML/LCP importer.
A lens alias also needs a matching camera manufacturer. Matching is exact, with only case and
surrounding whitespace ignored; unknown or ambiguous matches never choose a nearby lens.

Resolved coefficients live in `optics.calibration`; the existing profile enable checkbox and
profile distortion/vignetting strength sliders control them. Applying a measured profile creates
an ordinary Undo step. Optics reset clears it. Embedded DNG lens opcodes take precedence so the
two kinds of profile do not stack. Manual correction sliders still combine with the chosen profile. An active measured TCA curve
takes precedence over automatic CA estimation to avoid correcting the same fringe twice.

A resolved calibration is bound to the original file's 128-bit content hash and its independently
read camera/lens/focal/aperture metadata. The loader verifies these against the bytes it actually
decodes; unknown, stale, camera-preview and offline smart-preview sources leave the calibration
unapplied. Copy/paste, presets and sync can carry settings, but another file's identity does not
activate them: explicitly reapply a profile for that photo. A processed DNG likewise needs profile
reapplication because its bytes have changed. Content hashing detects accidental mismatches and
is not a security signature. Cached originals follow the existing media-cache lifetime: an external
replacement is detected on a fresh load/rescan, not by continuously polling an already cached image.

The current GPU warp lacks these measured equations. Such edits explicitly render through the
CPU pipeline, including previews and exports. Vignetting is corrected before minification and
per-channel geometric lookup; normalized radii make correction independent of image resolution
and orientation. Unfilled borders use the existing crop/geometry controls.

Mathematical references (documentation only):

- [Lensfun model equations](https://lensfun.github.io/manual/latest/group__Lens.html)
- [Lensfun calibration data units](https://lensfun.github.io/manual/latest/elem_calibration.html)
- [Lensfun order of corrections](https://lensfun.github.io/manual/latest/corrections.html)
- [PanoTools radial coordinate convention](https://wiki.panotools.org/Lens_correction_model)

The Rust equations and tests are written for LightCraft. No GPL/LGPL implementation, Adobe
profiles, or proprietary calibration files are included. Synthetic tests establish the mathematical
mapping, source binding and linear-RGB correction; they do not establish real-camera optical fidelity.
