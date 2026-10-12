//! Optimistic sidecar ownership: only packets observed on import/read or written by this session
//! are automatically replaced. Fingerprints persist with the library so ordinary reopen works.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use lightcraft_catalog::PhotoId;
use serde_json::{Value, json};

use crate::{EngineError, Result, Session};

const MAX_PACKET_BYTES: u64 = 8 << 20;
const MAX_BASELINES: usize = 100_000;
const MAX_ERRORS: usize = 10_000;

#[derive(Default)]
pub(crate) struct State {
    expected: BTreeMap<String, Option<String>>,
    errors: BTreeMap<PhotoId, String>,
    dirty: bool,
}

/// A packet snapshot captured by the import worker before its metadata is consumed.
#[derive(Debug)]
pub(crate) struct Snapshot {
    pub path: PathBuf,
    pub hash: Option<String>,
}

fn key(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    lightcraft_catalog::query::folder_key(&absolute.to_string_lossy())
}

pub(crate) fn fingerprint(bytes: Option<&[u8]>) -> Option<String> {
    bytes.map(|bytes| lightcraft_preview::hash_bytes(bytes).to_string())
}

/// Refuse symlinks throughout the sidecar path. Pure Rust's atomic writer cannot safely bind a
/// write through a user-replaced symlink or a symlinked parent directory.
pub(crate) fn validate_path(path: &Path) -> std::io::Result<()> {
    let absolute = std::path::absolute(path)?;
    for ancestor in absolute.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(std::io::Error::other(format!("{} is a symlink; sidecar write refused", ancestor.display())));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && ancestor == absolute => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub(crate) fn read(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    validate_path(path)?;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if file.metadata()?.len() > MAX_PACKET_BYTES {
        return Err(std::io::Error::other("XMP packet exceeds the 8 MiB safety limit"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PACKET_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PACKET_BYTES {
        return Err(std::io::Error::other("XMP packet exceeds the 8 MiB safety limit"));
    }
    Ok(Some(bytes))
}

/// Capture the preferred existing packet only; absence is checked when a writer creates it.
/// The packet comes from the same read as its fingerprint, avoiding a read/read race. Alternate
/// aliases are not trusted because their owned fields were not consumed by this import.
pub(crate) fn capture(original: &str, naming: crate::sidecar::SidecarNaming) -> (Vec<Snapshot>, Option<String>) {
    use crate::sidecar::{SidecarNaming, sidecar_path};
    let other = if naming == SidecarNaming::Stem { SidecarNaming::Full } else { SidecarNaming::Stem };
    for naming in [naming, other] {
        let path = sidecar_path(original, naming);
        for path in [path.clone(), path.with_extension("XMP")] {
            match read(&path) {
                Ok(Some(bytes)) => {
                    let packet = std::str::from_utf8(&bytes).ok().map(str::to_string);
                    return (vec![Snapshot { path, hash: fingerprint(Some(&bytes)) }], packet);
                }
                Ok(None) => {}
                Err(error) => {
                    log::warn!("{}: could not observe XMP: {error}", path.display());
                    // An unsafe preferred alias must not silently fall through to another packet.
                    return (Vec::new(), None);
                }
            }
        }
    }
    (Vec::new(), None)
}

impl Session {
    fn sidecar_guard(&self) -> MutexGuard<'_, State> {
        self.sidecars.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn restore_sidecar_baselines(&self, baselines: BTreeMap<String, Option<String>>) {
        let expected = baselines
            .into_iter()
            .filter(|(path, hash)| path.len() <= 4096 && hash.as_ref().is_none_or(|hash| lightcraft_preview::Hash128::parse(hash).is_some()))
            .take(MAX_BASELINES)
            .collect();
        *self.sidecar_guard() = State { expected, ..Default::default() };
    }

    pub(crate) fn restore_sidecar_pending(&self, pending: BTreeMap<u64, String>) {
        let errors = pending
            .into_iter()
            .filter(|(id, error)| self.catalog.photo(PhotoId(*id)).is_some() && error.len() <= 16_384)
            .take(MAX_ERRORS)
            .map(|(id, error)| (PhotoId(id), error))
            .collect();
        self.sidecar_guard().errors = errors;
    }

    pub(crate) fn sidecar_pending(&self) -> BTreeMap<u64, String> {
        self.sidecar_guard().errors.iter().map(|(id, error)| (id.0, error.clone())).collect()
    }

    pub(crate) fn sidecar_baselines(&self) -> BTreeMap<String, Option<String>> {
        self.sidecar_guard().expected.clone()
    }

    pub(crate) fn observe_sidecar(&self, snapshot: Snapshot) {
        let mut state = self.sidecar_guard();
        let key = key(&snapshot.path);
        if state.expected.get(&key) != Some(&snapshot.hash) {
            state.expected.insert(key, snapshot.hash);
            while state.expected.len() > MAX_BASELINES {
                state.expected.pop_first();
            }
            state.dirty = true;
        }
    }

    pub(crate) fn check_sidecar(&self, path: &Path, hash: &Option<String>) -> std::io::Result<()> {
        let state = self.sidecar_guard();
        match state.expected.get(&key(path)) {
            Some(expected) if expected == hash => Ok(()),
            None if hash.is_none() => Ok(()),
            _ => Err(std::io::Error::other(
                "XMP changed outside LightCraft or has no trusted baseline; use Read Metadata from File to review/rebase before saving automatically",
            )),
        }
    }

    pub(crate) fn record_sidecar_result(&self, id: PhotoId, result: &Result<crate::sidecar::SidecarSaved>) {
        let mut state = self.sidecar_guard();
        match result {
            Ok(_) => {
                if state.errors.remove(&id).is_some() {
                    state.dirty = true;
                }
            }
            Err(error) => {
                let error = error.to_string();
                if state.errors.get(&id) != Some(&error) {
                    state.errors.insert(id, error);
                    while state.errors.len() > MAX_ERRORS {
                        state.errors.pop_first();
                    }
                    state.dirty = true;
                }
            }
        }
    }

    pub(crate) fn clear_sidecar_error(&self, id: PhotoId) {
        let mut state = self.sidecar_guard();
        if state.errors.remove(&id).is_some() {
            state.dirty = true;
        }
    }

    pub fn sidecar_errors(&self) -> Vec<(PhotoId, String)> {
        self.sidecar_guard().errors.iter().map(|(id, error)| (*id, error.clone())).collect()
    }

    /// Cheap frame-loop warning; avoids cloning every failed packet's message each frame.
    pub fn sidecar_summary(&self) -> (usize, Option<String>) {
        let state = self.sidecar_guard();
        (state.errors.len(), state.errors.first_key_value().map(|(_, error)| error.clone()))
    }

    pub fn sidecar_status(&self) -> Value {
        json!({"pending": self.sidecar_errors().into_iter().map(|(id, error)| json!({"id": id.0, "error": error})).collect::<Vec<_>>()})
    }

    pub(crate) fn persist_sidecar_baselines(&mut self) -> Result<()> {
        if !self.sidecar_guard().dirty {
            return Ok(());
        }
        self.save_prefs().map_err(|error| EngineError::Other(format!("XMP persistence state could not be saved: {error}")))?;
        self.sidecar_guard().dirty = false;
        Ok(())
    }
}

pub(crate) fn new_state() -> Mutex<State> {
    Mutex::new(State::default())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        photo: PathBuf,
        xmp: PathBuf,
        library: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("lc-sidecar-guard-{}-{}", std::process::id(), FIXTURE.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir(&root).unwrap();
            let photo = root.join("original.png");
            let image = lightcraft_raster::Rgba8::from_fn(24, 16, |x, y| [(x * 9) as u8, (y * 13) as u8, 42, 255]);
            let png =
                lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&image), &lightcraft_codecs::EncodeMeta::default()).unwrap();
            std::fs::write(&photo, png).unwrap();
            Self { photo, xmp: root.join("original.xmp"), library: root.join("library"), root }
        }
        fn session(&self) -> Session {
            let mut session = Session::new().with_fs();
            session.open_library(&self.library, false).unwrap();
            session.execute("library.import", &json!({"paths": [self.photo.to_string_lossy()]})).unwrap();
            session.xmp.auto_write = true;
            session
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn foreign(rating: u8) -> String {
        format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:foreign="urn:photo:test" xmp:Rating="{rating}" foreign:marker="keep-me-byte-for-byte"/></rdf:RDF></x:xmpmeta>"#
        )
    }

    #[test]
    fn external_edit_is_preserved_and_explicit_read_rebases_then_saves() {
        let fixture = Fixture::new();
        std::fs::write(&fixture.xmp, foreign(1)).unwrap();
        let mut session = fixture.session();
        let id = session.active().unwrap();
        std::fs::write(&fixture.xmp, foreign(4)).unwrap();
        let external = std::fs::read(&fixture.xmp).unwrap();
        let error = session.execute("photo.rate", &json!({"rating": 5})).unwrap_err();
        assert!(error.to_string().contains("XMP is pending"));
        assert_eq!(session.catalog.photo(id).unwrap().rating, 5, "catalog edit remains applied");
        assert_eq!(std::fs::read(&fixture.xmp).unwrap(), external);
        assert_eq!(session.sidecar_errors().len(), 1);
        let read = session.execute("photo.readMetadataFromFile", &json!({})).unwrap();
        assert!(read["failed"].as_array().unwrap().is_empty());
        assert_eq!(session.catalog.photo(id).unwrap().rating, 4);
        assert_eq!(std::fs::read(&fixture.xmp).unwrap(), external, "reading itself does not rewrite XMP");
        session.execute("photo.rate", &json!({"rating": 3})).unwrap();
        let packet = std::fs::read_to_string(&fixture.xmp).unwrap();
        assert!(packet.contains("foreign:marker=\"keep-me-byte-for-byte\""));
        assert_eq!(crate::sidecar::parse_sidecar(&packet, crate::crs::Target::Rendered).unwrap().rating, Some(3));
        assert!(session.sidecar_errors().is_empty());
    }

    #[test]
    fn alternate_packet_not_consumed_on_import_is_not_trusted_for_automatic_write() {
        let fixture = Fixture::new();
        std::fs::write(&fixture.xmp, foreign(1)).unwrap();
        let alternate = fixture.root.join("original.png.xmp");
        let external = foreign(4);
        std::fs::write(&alternate, &external).unwrap();
        let sibling = fixture.root.join("original.jpg");
        std::fs::copy(&fixture.photo, &sibling).unwrap();
        let mut session = fixture.session();
        let id = session.active().unwrap();
        // The newly catalogued sibling owns the shared stem by file-name rank. The PNG now
        // writes its full-name alias, whose metadata this import deliberately did not consume.
        session.execute("library.import", &json!({"paths": [sibling.to_string_lossy()], "local": true})).unwrap();
        assert_eq!(session.sidecar_naming(id), crate::sidecar::SidecarNaming::Full);
        assert!(session.execute("photo.rate", &json!({"ids": [id.0], "rating": 5})).is_err());
        assert_eq!(std::fs::read_to_string(&alternate).unwrap(), external);
    }

    #[test]
    fn failed_write_is_reported_catalog_is_durable_and_retry_preserves_original() {
        let fixture = Fixture::new();
        let original = std::fs::read(&fixture.photo).unwrap();
        let mut session = fixture.session();
        let id = session.active().unwrap();
        {
            let _fault = lightcraft_catalog::safe_file::fail_writes_after(3);
            assert!(session.execute("photo.rate", &json!({"rating": 5})).is_err());
        }
        assert_eq!(session.catalog.photo(id).unwrap().rating, 5);
        assert!(!fixture.xmp.exists());
        assert_eq!(session.execute("photo.sidecarStatus", &json!({})).unwrap()["pending"].as_array().unwrap().len(), 1);
        let retry = session.execute("photo.retrySidecars", &json!({})).unwrap();
        assert!(retry["failed"].as_array().unwrap().is_empty());
        assert_eq!(retry["written"].as_array().unwrap().len(), 1);
        assert_eq!(std::fs::read(&fixture.photo).unwrap(), original);
        drop(session);
        let mut reopened = fixture.session();
        assert_eq!(reopened.catalog.photo(id).unwrap().rating, 5, "catalog persisted despite failed sidecar write");
        reopened.execute("photo.rate", &json!({"rating": 2})).unwrap();
    }

    #[test]
    fn ordinary_reopen_uses_saved_baseline_but_foreign_edit_after_restart_conflicts() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.execute("photo.rate", &json!({"rating": 2})).unwrap();
        drop(session);
        let mut session = fixture.session();
        session.execute("photo.rate", &json!({"rating": 3})).unwrap();
        drop(session);
        std::fs::write(&fixture.xmp, foreign(4)).unwrap();
        let external = std::fs::read(&fixture.xmp).unwrap();
        let mut session = fixture.session();
        assert!(session.execute("photo.rate", &json!({"rating": 5})).is_err());
        assert_eq!(std::fs::read(&fixture.xmp).unwrap(), external);
        drop(session);
        let mut session = fixture.session();
        assert_eq!(session.sidecar_errors().len(), 1, "pending sidecar status persists across reopen");
        session.execute("photo.readMetadataFromFile", &json!({})).unwrap();
        session.execute("photo.rate", &json!({"rating": 1})).unwrap();
    }

    #[test]
    fn malformed_sidecar_is_never_automatically_replaced_or_backed_up() {
        let fixture = Fixture::new();
        let malformed = b"<broken packet";
        std::fs::write(&fixture.xmp, malformed).unwrap();
        let mut session = fixture.session();
        assert!(session.execute("photo.rate", &json!({"rating": 5})).is_err());
        assert_eq!(std::fs::read(&fixture.xmp).unwrap(), malformed);
        assert!(!std::fs::read_dir(&fixture.root).unwrap().flatten().any(|entry| entry.file_name().to_string_lossy().contains("bak")));
    }

    #[test]
    fn oversized_packet_is_refused_without_reading_or_replacing_it() {
        let fixture = Fixture::new();
        let packet = std::fs::File::create(&fixture.xmp).unwrap();
        packet.set_len(MAX_PACKET_BYTES + 1).unwrap();
        assert!(read(&fixture.xmp).unwrap_err().to_string().contains("8 MiB"));
        let mut session = fixture.session();
        assert!(session.execute("photo.rate", &json!({"rating": 5})).is_err());
        assert_eq!(std::fs::metadata(&fixture.xmp).unwrap().len(), MAX_PACKET_BYTES + 1);
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_and_parent_symlinks_are_refused_without_touching_target() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let target = fixture.root.join("foreign.xmp");
        std::fs::write(&target, foreign(4)).unwrap();
        symlink(&target, &fixture.xmp).unwrap();
        let mut session = fixture.session();
        assert!(session.execute("photo.rate", &json!({"rating": 5})).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), foreign(4));
        let alias = fixture.root.join("linked-folder");
        symlink(&fixture.library, &alias).unwrap();
        assert!(validate_path(&alias.join("photo.xmp")).is_err());
    }
}
