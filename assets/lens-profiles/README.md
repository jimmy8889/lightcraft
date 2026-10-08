# Measured Sony E lens data

`sony-e.json` is a modified subset of Lensfun's **database data**, licensed under CC BY-SA 3.0.
The implementation in LightCraft is independent Rust code; no Lensfun implementation or runtime is included.

Source: https://github.com/lensfun/lensfun/blob/bbd4332a9ec566fd9aa548c9e0d8ced238c56261/data/db/mil-sony.xml

Pinned commit: `bbd4332a9ec566fd9aa548c9e0d8ced238c56261`.
Original XML SHA-256: `f9f7e5d7461ac58f76eb55dcc5b211f2e9a451402ddb4a9ecb12d1e4dc04f2b8`.
Creator: Lensfun database contributors. Licence: [CC BY-SA 3.0](LICENSE-CC-BY-SA-3.0.txt).

Changes: selected FE 14mm f/1.8 GM, FE 24mm f/1.4 GM, FE 50mm f/1.4 GM,
FE 35mm f/1.8, FE 35mm f/2.8 ZA, and FE 50mm f/1.8; converted the
original distortion, TCA, and vignetting rows to JSON; supplied the mathematical defaults for
omitted TCA coefficients. Numeric calibration values are unchanged. Metadata adds an identifier,
provenance, licence, original model alias, verified Sony prime aliases and a 3:2 sensor aspect. These data derivatives remain
CC BY-SA 3.0; the surrounding application code retains its own licence.

Calibration camera comments retained from the XML:

- FE 14mm f/1.8 GM: ILCE-1; distortion/TCA coefficients fitted from EXIF data by lf_fitexif_se.
- FE 24mm f/1.4 GM: no calibration camera identified in the selected entry.
- FE 50mm f/1.4 GM: ILCE-7RM5; distortion/TCA coefficients fitted from EXIF data by lf_fitexif_se; no vignetting rows.
- FE 35mm f/1.8: no calibration camera identified in the selected entry.
- FE 35mm f/2.8 ZA: distortion/TCA taken with Sony A7R3; vignetting taken with Sony
  ILCE-7RM2 (α7R II).
- FE 50mm f/1.8: taken with ILCE-7M2 Alpha 7 II.

Bundled aliases include the original database names plus these exact Sony product names and
model codes, verified from Sony product pages on 2026-10-08:

- [FE 14mm F1.8 GM / SEL14F18GM](https://electronics.sony.com/imaging/lenses/all-e-mount/p/sel14f18gm)
- [FE 24mm F1.4 GM / SEL24F14GM](https://electronics.sony.com/imaging/lenses/all-e-mount/p/sel24f14gm)
- [FE 50mm F1.4 GM / SEL50F14GM](https://electronics.sony.com/imaging/lenses/all-e-mount/p/sel50f14gm)

Matching ignores only case and surrounding whitespace. These aliases establish product identity;
actual LensModel tags from these three lenses have not been validated against user RAW files.
No fuzzy, Zeiss/ZA, converter or other alternate spelling is inferred. Coverage is deliberately
small and exact: no interpolation between focal/aperture/focus samples, no crop transfer, no
fisheye conversion, no unsupported-model fallback. See [usage and limitations](../../docs/lens-profiles.md).

Current-kit audit: all 56 XML data files under Lensfun `data/db/` at current official master
`efca2062d271f7989566701ed54210cd5736f92c` were checked on 2026-10-08. Its Sony E XML is
byte-identical to the pinned source above. No exact FE 70–200mm f/2.8 GM OSS II, FE 300mm
f/2.8 GM OSS or Viltrox TC2.0X combination calibration was present. Original-generation
70–200 GM measurements are not substituted, and no converter profile is synthesized.
