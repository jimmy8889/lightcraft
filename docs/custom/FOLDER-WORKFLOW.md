# Windows and Linux folder workflow

This fork develops on `custom/folder-raw-workflow`. The upstream base and custom
feature status are recorded in [features.json](features.json). Work in progress
is not an installer release; [UPSTREAM.md](UPSTREAM.md) explains release checks
and how future creator updates are tested without replacing custom work.

## Working directly in a folder

The desktop host opens a supplied file or folder in place. The catalog remains
a metadata index and cache; originals stay in their existing folder.

```sh
cargo run -p lightcraft -- /path/to/photos
```

On Windows, use the folder's normal Windows path. With no argument, choose
**Open Folder**. The desktop remembers the last folder. Procedural demo images
require `--demo`.

In the culling workspace, arrows navigate in natural filename order, `Z` toggles
Fit and 100%, `0`–`5` set ratings, `X` moves the selected file group into
`Rejected`, and `Ctrl+Z` restores it. Switch to **Develop** to use editing
shortcuts. Fit can use an unedited RAW's camera JPEG; 100% requests developed
RAW pixels. The view labels which source is displayed.

Folders are queued in natural filename order and imported in small background
batches, so the first images appear before the whole folder finishes importing.
Metadata probing and catalog content hashing still read original files. Rejection
and Undo wait until that scan/import completes to prevent late batches from
reintroducing moved paths. The 100% view also depends on the GPU texture limit;
it is not a guarantee that every camera resolution fits a single texture.

Recursive scans skip `Rejected`, `Processed` and internal workflow folders.
An explicitly opened output folder remains inspectable. Refresh reads new files.

## Safe rejection and sidecars

Rejection includes unambiguous stem and full-filename `.xmp` and `.dop`
sidecars. DOP content is preserved; it is not interpreted or rewritten.
Ambiguous ownership, occupied destination names, symlinks and changed files
refuse the operation. A durable `.lightcraft-culling` journal records rejection
and Undo; interrupted work is recovered on reopening the source folder.

Exclusive hard links followed by unlinking avoid replacement of existing files.
The source and `Rejected` must share a filesystem with reliable hard-link and
locking support. Unsupported FAT/exFAT or network filesystems refuse safely.
Windows read-only source files are refused without changing their attributes.
Windows does not expose Unix directory syncing through Rust's standard library;
native power-loss recovery must be validated separately on the target storage.

Edits and ratings save beside originals in XMP. Sidecar conflict detection and
visible failure reporting are tracked separately in the feature inventory until
their tests pass. External edits must be reread before retrying a conflicting
write. Automatic writes use persisted fingerprints and a final packet recheck;
an external program can still race the final check because it does not share
LightCraft's lock. An explicit **Save Metadata to File** is a deliberate write
and retains upstream merge/backup behavior for malformed packets. DOP files
remain opaque and byte stable during rejection and Undo.

## RAW processing

**RAW Denoise (Native)** is a full-resolution, classical CPU algorithm applied
before demosaic and white balance. It treats each Bayer phase separately and
has a distinct path for Sony lossless M/S linear RGB. Strength zero disables it.
Masked sensor borders and clipped highlights remain unchanged. Unsupported CFA
layouts return an error. This is not AI, DeepPRIME or a DxO quality-equivalence claim.

**Export Processed DNG** writes actual cleaned sensor samples, rather than
repackaging an unchanged RAW. The default is `Processed/<stem>-Clean.dng`.
Existing output names are refused. The file is decoded and its samples and
sensor interpretation are checked before writing. The original is untouched.
The derivative resets its RAW denoise setting so reopening does not process it
twice; other development edits remain metadata.

LightCraft's per-file camera JPEG colour estimate travels in private DNG XMP.
Other editors use the DNG camera tags; matching external-editor colour has not
been established. A measured camera calibration database remains required.

## Further quality work

Automatic distortion, chromatic aberration and vignetting correction need
exact, licensed lens calibration data. Lens-aware sharpening needs measured
sharpness data. An actual learned RAW denoiser also needs licensable weights,
camera-appropriate noise calibration, reference inference checks and full-size
photo evaluation. Phone-camera calibration must not be applied to Sony ISO values.

Existing tone, haze, local contrast, perspective and masking tools can support
workflows resembling Smart Lighting, ClearView and ViewPoint; they need their
own quality evaluation. Batch dust removal and richer people/sky/depth masks
are subsequent features. Do not describe these roadmap items as implemented.
