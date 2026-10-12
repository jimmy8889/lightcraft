//! Synthetic fixtures only: original bytes, collision refusal, crash recovery and catalog state.
use crate::{Browse, LibrarySource, Selection, Session};
use lightcraft_catalog::{Op, Photo, PhotoId, Source};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("lightcraft-culling-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&dir).unwrap();
        Self(std::fs::canonicalize(dir).unwrap())
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.0.join(name), bytes).unwrap();
    }
    fn session(&self) -> Session {
        let mut s = Session::new();
        s.browse = Some(Browse { path: self.0.to_string_lossy().to_string(), subfolders: true });
        s.source = LibrarySource::Folder;
        s.xmp.auto_write = true;
        s
    }
    fn add(&self, s: &mut Session, name: &str) -> PhotoId {
        let id = s.catalog.alloc_photo_id();
        let photo = Photo::new(id, Source::File { path: self.0.join(name).to_string_lossy().to_string() }, name, "ARW", 1, 1, "2026-10-08");
        s.catalog.apply(Op::AddPhoto { photo: Box::new(photo) }).unwrap();
        s.selection = Selection::single(id);
        id
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn path(s: &Session, id: PhotoId) -> &str {
    match &s.catalog.photo(id).unwrap().source {
        Source::File { path } => path,
        _ => panic!("file expected"),
    }
}

#[test]
fn rejection_moves_every_sidecar_byte_exact_and_undo_restores_selection() {
    let f = Fixture::new();
    for name in ["IMG_0001.ARW", "IMG_0001.xmp", "IMG_0001.ARW.XMP", "IMG_0001.dop", "IMG_0001.ARW.DOP"] {
        f.write(name, format!("untouched {name}\0").as_bytes());
    }
    let mut s = f.session();
    let id = f.add(&mut s, "IMG_0001.ARW");
    let result = s.execute("cull.reject", &json!({})).unwrap();
    assert!(s.visible().is_empty());
    assert!(s.selection.active.is_none());
    assert!(path(&s, id).contains("Rejected"));
    for name in ["IMG_0001.ARW", "IMG_0001.xmp", "IMG_0001.ARW.XMP", "IMG_0001.dop", "IMG_0001.ARW.DOP"] {
        assert!(!f.0.join(name).exists());
        assert_eq!(std::fs::read(f.0.join("Rejected").join(name)).unwrap(), format!("untouched {name}\0").as_bytes());
    }
    assert!(s.undo.is_empty(), "ordinary edit undo must never relink without moving files");
    let undo = s.execute("cull.undo", &json!({"journal_path":result["journal_path"]})).unwrap();
    assert_eq!(undo["restored_ids"], json!([id]));
    assert_eq!(s.selection.active, Some(id));
    assert_eq!(s.visible(), &[id]);
    assert_eq!(std::fs::read(f.0.join("IMG_0001.dop")).unwrap(), b"untouched IMG_0001.dop\0");
}

#[test]
fn group_collision_and_ambiguous_sidecars_leave_all_originals() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("A.xmp", b"xmp");
    std::fs::create_dir(f.0.join("Rejected")).unwrap();
    std::fs::write(f.0.join("Rejected/a.XMP"), b"foreign").unwrap();
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    assert!(s.execute("cull.reject", &json!({})).is_err());
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"raw");
    assert_eq!(std::fs::read(f.0.join("Rejected/a.XMP")).unwrap(), b"foreign");
    std::fs::remove_file(f.0.join("Rejected/a.XMP")).unwrap();
    f.write("A.JPG", b"jpeg");
    assert!(s.execute("cull.reject", &json!({})).unwrap_err().to_string().contains("ambiguous"));
    assert!(f.0.join("A.xmp").exists());
}

#[test]
fn full_name_sidecar_is_owned_even_with_same_stem_jpeg() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("A.JPG", b"jpeg");
    f.write("A.ARW.dop", b"dop");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    s.execute("cull.reject", &json!({})).unwrap();
    assert_eq!(std::fs::read(f.0.join("A.JPG")).unwrap(), b"jpeg");
    assert_eq!(std::fs::read(f.0.join("Rejected/A.ARW.dop")).unwrap(), b"dop");
}

#[test]
fn undo_refuses_changed_sidecar_or_new_original_without_partial_move() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("A.dop", b"dop");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    s.execute("cull.reject", &json!({})).unwrap();
    std::fs::write(f.0.join("Rejected/A.dop"), b"changed").unwrap();
    assert!(s.execute("cull.undo", &json!({})).is_err());
    assert!(!f.0.join("A.ARW").exists());
    f.write("A.ARW", b"foreign");
    assert!(s.execute("cull.undo", &json!({})).is_err());
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"foreign");
}

#[test]
fn interrupted_link_and_unlink_recover_after_restart_without_rewriting_bytes() {
    for phase in ["after-link", "after-unlink"] {
        let f = Fixture::new();
        f.write("A.ARW", b"raw bytes");
        f.write("A.xmp", b"sidecar bytes");
        let mut s = f.session();
        let id = f.add(&mut s, "A.ARW");
        crate::culling::inject_fault(phase);
        assert!(s.execute("cull.reject", &json!({})).is_err());
        assert_eq!(Path::new(path(&s, id)), f.0.join("A.ARW"));
        let mut restarted = f.session();
        let new_id = f.add(&mut restarted, "A.ARW");
        let r = restarted.execute("cull.recover", &json!({})).unwrap();
        assert_eq!(r["transactions"][0]["state"], "aborted");
        assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"raw bytes");
        assert_eq!(std::fs::read(f.0.join("A.xmp")).unwrap(), b"sidecar bytes");
        assert!(!f.0.join("Rejected/A.ARW").exists());
        assert_eq!(Path::new(path(&restarted, new_id)), f.0.join("A.ARW"));
    }
}

#[test]
fn completed_rejection_recovers_catalog_path_after_crash_before_save() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    s.execute("cull.reject", &json!({})).unwrap();
    let mut restarted = f.session();
    let id = f.add(&mut restarted, "A.ARW");
    restarted.execute("cull.recover", &json!({})).unwrap();
    assert_eq!(Path::new(path(&restarted, id)), f.0.join("Rejected/A.ARW"));
    assert!(restarted.visible().is_empty());
    restarted.execute("cull.undo", &json!({})).unwrap();
    assert_eq!(restarted.selection.active, Some(id));
}

#[test]
fn interrupted_undo_finishes_restoration_and_reconciles_catalog() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("A.dop", b"dop");
    let mut s = f.session();
    let id = f.add(&mut s, "A.ARW");
    s.execute("cull.reject", &json!({})).unwrap();
    crate::culling::inject_fault("after-link");
    assert!(s.execute("cull.undo", &json!({})).is_err());
    // Clear test crash tracking; recovery itself is the restart boundary.
    s.execute("cull.recover", &json!({})).unwrap();
    assert_eq!(Path::new(path(&s, id)), f.0.join("A.ARW"));
    assert_eq!(std::fs::read(f.0.join("A.dop")).unwrap(), b"dop");
}

#[test]
fn malformed_manifest_does_not_move_external_paths() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    let result = s.execute("cull.reject", &json!({})).unwrap();
    let manifest = PathBuf::from(result["journal_path"].as_str().unwrap()).join("manifest.json");
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    value["files"][0]["name"] = json!("../outside.ARW");
    std::fs::write(manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(s.execute("cull.recover", &json!({})).is_err());
    assert_eq!(std::fs::read(f.0.join("Rejected/A.ARW")).unwrap(), b"raw");
}

#[cfg(unix)]
#[test]
fn symlink_sidecar_and_destination_directory_are_refused() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("foreign", b"foreign");
    symlink(f.0.join("foreign"), f.0.join("A.xmp")).unwrap();
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    assert!(s.execute("cull.reject", &json!({})).is_err());
    assert!(f.0.join("A.ARW").exists());
    std::fs::remove_file(f.0.join("A.xmp")).unwrap();
    std::fs::create_dir(f.0.join("destination")).unwrap();
    symlink(f.0.join("destination"), f.0.join("Rejected")).unwrap();
    assert!(s.execute("cull.reject", &json!({})).is_err());
    assert!(f.0.join("A.ARW").exists());
}

#[test]
fn multi_photo_history_reconciles_latest_state_per_file() {
    let f = Fixture::new();
    f.write("A.ARW", b"a");
    f.write("B.ARW", b"b");
    let mut s = f.session();
    let a = f.add(&mut s, "A.ARW");
    let b = f.add(&mut s, "B.ARW");
    s.execute("cull.reject", &json!({"ids":[a,b]})).unwrap();
    s.execute("cull.undo", &json!({})).unwrap();
    s.execute("cull.reject", &json!({"ids":[a]})).unwrap();
    let mut restarted = f.session();
    let a = f.add(&mut restarted, "A.ARW");
    let b = f.add(&mut restarted, "B.ARW");
    restarted.execute("cull.recover", &json!({})).unwrap();
    assert_eq!(Path::new(path(&restarted, a)), f.0.join("Rejected/A.ARW"));
    assert_eq!(Path::new(path(&restarted, b)), f.0.join("B.ARW"));
}

#[test]
fn ordinary_move_failure_rolls_back_and_records_aborted_transaction() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("A.dop", b"dop");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    crate::culling::inject_fault("fail-before-unlink");
    let e = s.execute("cull.reject", &json!({})).unwrap_err();
    assert!(e.to_string().contains("originals restored"));
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"raw");
    assert_eq!(std::fs::read(f.0.join("A.dop")).unwrap(), b"dop");
    assert!(!f.0.join("Rejected/A.ARW").exists());
    assert_eq!(s.execute("cull.history", &json!({})).unwrap()["transactions"][0]["state"], "aborted");
}

#[test]
fn pending_recovery_refuses_external_changes_before_touching_other_files() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("A.xmp", b"xmp");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    crate::culling::inject_fault("after-unlink");
    assert!(s.execute("cull.reject", &json!({})).is_err());
    std::fs::write(f.0.join("Rejected/A.ARW"), b"external change").unwrap();
    assert!(s.execute("cull.recover", &json!({})).unwrap_err().to_string().contains("identity changed"));
    assert_eq!(std::fs::read(f.0.join("Rejected/A.ARW")).unwrap(), b"external change");
    assert_eq!(std::fs::read(f.0.join("A.xmp")).unwrap(), b"xmp");
    assert!(!f.0.join("A.ARW").exists());
}

#[test]
fn recursive_folder_hides_output_descendants_but_allows_explicit_output_root() {
    let f = Fixture::new();
    let mut s = f.session();
    f.write("A.ARW", b"raw");
    let root = f.add(&mut s, "A.ARW");
    let hidden = f.add(&mut s, "Processed/B.ARW");
    let rejected = f.add(&mut s, "Rejected/C.ARW");
    let visible = f.add(&mut s, "Camera/D.ARW");
    assert!(s.visible().contains(&root));
    assert!(s.visible().contains(&visible));
    assert!(!s.visible().contains(&hidden));
    assert!(!s.visible().contains(&rejected));
    assert_eq!(s.source_total(), Some(2));
    s.browse.as_mut().unwrap().path = f.0.join("Processed").to_string_lossy().to_string();
    assert_eq!(s.visible(), &[hidden]);
    assert_eq!(s.source_total(), Some(1));
}

#[test]
fn malformed_params_are_rejected_without_using_selection() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    for p in [json!({"ids":"wrong"}), json!({"ids":["bad"]}), json!({"ids":[]})] {
        assert!(s.execute("cull.reject", &p).is_err());
    }
    assert!(f.0.join("A.ARW").exists());
}

#[test]
fn incomplete_preparation_retains_originals_and_does_not_block_browsing() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    let dir = f.0.join(".lightcraft-culling").join(format!("{:039}-1-00000000", 1));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), b"partial json").unwrap();
    let result = s.execute("cull.recover", &json!({})).unwrap();
    assert_eq!(result["transactions"][0]["state"], "incomplete");
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"raw");
    s.execute("cull.reject", &json!({})).unwrap();
    assert!(f.0.join("Rejected/A.ARW").exists());
}

#[test]
fn invalid_phase_marker_preserves_files() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    let result = s.execute("cull.reject", &json!({})).unwrap();
    let journal = PathBuf::from(result["journal_path"].as_str().unwrap());
    std::fs::write(journal.join("committed"), b"corrupt").unwrap();
    assert!(s.execute("cull.recover", &json!({})).is_err());
    assert_eq!(std::fs::read(f.0.join("Rejected/A.ARW")).unwrap(), b"raw");
}

#[cfg(unix)]
#[test]
fn full_name_sidecar_with_case_ambiguous_photo_names_is_refused() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("a.arw", b"other raw");
    f.write("A.ARW.xmp", b"sidecar");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    assert!(s.execute("cull.reject", &json!({})).unwrap_err().to_string().contains("ambiguous"));
    assert_eq!(std::fs::read(f.0.join("A.ARW.xmp")).unwrap(), b"sidecar");
    assert!(f.0.join("A.ARW").exists());
    assert!(f.0.join("a.arw").exists());
}

#[test]
fn completed_recovery_refuses_reused_original_name_in_stale_catalog() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    s.execute("cull.reject", &json!({})).unwrap();
    f.write("A.ARW", b"new original");
    let mut restarted = f.session();
    let id = f.add(&mut restarted, "A.ARW");
    assert!(restarted.execute("cull.recover", &json!({})).unwrap_err().to_string().contains("ambiguous"));
    assert_eq!(Path::new(path(&restarted, id)), f.0.join("A.ARW"));
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"new original");
    assert_eq!(std::fs::read(f.0.join("Rejected/A.ARW")).unwrap(), b"raw");
}

#[test]
fn folder_advisory_lock_refuses_concurrent_culling_without_moving_files() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    let journal = f.0.join(".lightcraft-culling");
    std::fs::create_dir(&journal).unwrap();
    let held = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(journal.join(".lock")).unwrap();
    held.try_lock().unwrap();
    assert!(s.execute("cull.reject", &json!({})).unwrap_err().to_string().contains("already in use"));
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"raw");
    drop(held);
    s.execute("cull.reject", &json!({})).unwrap();
    assert!(f.0.join("Rejected/A.ARW").exists());
}

#[test]
fn same_volume_folder_rename_preserves_journal_recovery_and_undo() {
    let mut f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("A.dop", b"dop");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    s.execute("cull.reject", &json!({})).unwrap();
    let relocated = f.0.with_extension("relocated");
    std::fs::rename(&f.0, &relocated).unwrap();
    f.0 = std::fs::canonicalize(relocated).unwrap();
    let mut restarted = f.session();
    let id = f.add(&mut restarted, "A.ARW");
    restarted.execute("cull.recover", &json!({})).unwrap();
    assert_eq!(Path::new(path(&restarted, id)), f.0.join("Rejected/A.ARW"));
    restarted.execute("cull.undo", &json!({})).unwrap();
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"raw");
    assert_eq!(std::fs::read(f.0.join("A.dop")).unwrap(), b"dop");
}

#[test]
fn opening_rejected_output_folder_allows_read_only_recovery_but_refuses_nested_reject() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    let id = f.add(&mut s, "A.ARW");
    s.execute("cull.reject", &json!({})).unwrap();
    s.browse.as_mut().unwrap().path = f.0.join("Rejected").to_string_lossy().to_string();
    assert!(s.execute("cull.recover", &json!({})).is_ok());
    assert_eq!(s.visible(), &[id]);
    s.selection = Selection::single(id);
    assert!(s.execute("cull.reject", &json!({})).unwrap_err().to_string().contains("from Rejected"));
    assert!(f.0.join("Rejected/A.ARW").exists());
}

#[test]
fn contradictory_undone_and_aborted_markers_are_refused() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    let result = s.execute("cull.reject", &json!({})).unwrap();
    s.execute("cull.undo", &json!({})).unwrap();
    let journal = PathBuf::from(result["journal_path"].as_str().unwrap());
    std::fs::write(journal.join("aborted"), b"").unwrap();
    assert!(s.execute("cull.recover", &json!({})).unwrap_err().to_string().contains("inconsistent"));
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"raw");
}

#[test]
fn explicit_undo_refuses_older_superseded_transaction() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    let id = f.add(&mut s, "A.ARW");
    let first = s.execute("cull.reject", &json!({})).unwrap();
    // An external application restores the name without updating LightCraft's journal.
    std::fs::rename(f.0.join("Rejected/A.ARW"), f.0.join("A.ARW")).unwrap();
    // A later valid transaction is simulated by restoring via engine first, then deliberately
    // retaining the older committed marker as a malformed history record.
    std::fs::rename(f.0.join("A.ARW"), f.0.join("Rejected/A.ARW")).unwrap();
    s.execute("cull.undo", &json!({})).unwrap();
    let second = s.execute("cull.reject", &json!({"ids":[id]})).unwrap();
    let first_journal = PathBuf::from(first["journal_path"].as_str().unwrap());
    std::fs::remove_file(first_journal.join("undoing")).unwrap();
    std::fs::remove_file(first_journal.join("undone")).unwrap();
    assert!(s.execute("cull.undo", &json!({"journal_path":first["journal_path"]})).unwrap_err().to_string().contains("newer"));
    assert!(!f.0.join("A.ARW").exists());
    assert_eq!(std::fs::read(f.0.join("Rejected/A.ARW")).unwrap(), b"raw");
    s.execute("cull.undo", &json!({"journal_path":second["journal_path"]})).unwrap();
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"raw");
}

#[test]
fn overlapping_pending_transactions_are_refused_before_any_move() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    let id = f.add(&mut s, "A.ARW");
    let first = s.execute("cull.reject", &json!({})).unwrap();
    s.execute("cull.undo", &json!({})).unwrap();
    s.execute("cull.reject", &json!({"ids":[id]})).unwrap();
    let first_journal = PathBuf::from(first["journal_path"].as_str().unwrap());
    for marker in ["committed", "undoing", "undone"] {
        std::fs::remove_file(first_journal.join(marker)).unwrap();
    }
    assert!(s.execute("cull.recover", &json!({})).unwrap_err().to_string().contains("overlapping pending"));
    assert!(!f.0.join("A.ARW").exists());
    assert_eq!(std::fs::read(f.0.join("Rejected/A.ARW")).unwrap(), b"raw");
}

#[cfg(windows)]
#[test]
#[allow(clippy::permissions_set_readonly_false)] // Windows-only: clear a disposable fixture's read-only attribute.
fn windows_read_only_original_is_refused_before_link_without_changing_attributes() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    let path = f.0.join("A.ARW");
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions).unwrap();
    let result = s.execute("cull.reject", &json!({}));
    let remains_readonly = std::fs::metadata(&path).unwrap().permissions().readonly();
    // Restore the disposable fixture's attribute so Drop can remove it on Windows.
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).unwrap();
    assert!(result.unwrap_err().to_string().contains("read-only"));
    assert!(remains_readonly);
    assert_eq!(std::fs::read(path).unwrap(), b"raw");
    assert!(!f.0.join("Rejected/A.ARW").exists());
}

#[cfg(windows)]
#[test]
fn windows_ordinary_case_variants_relink_and_undo_canonical_source_paths() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("A.xmp", b"sidecar");
    let mut s = f.session();
    let id = f.add(&mut s, "A.ARW");
    let canonical = f.0.to_string_lossy().to_string();
    let ordinary = canonical.strip_prefix(r"\\?\").unwrap_or(&canonical).to_lowercase();
    let original = PathBuf::from(&ordinary).join("a.arw");
    s.catalog
        .apply(Op::Relink { id, file_name: "A.ARW".into(), source: Source::File { path: original.to_string_lossy().to_string() }, format: None })
        .unwrap();
    s.browse.as_mut().unwrap().path = ordinary.clone();
    assert_eq!(s.visible(), &[id]);
    let rejected = s.execute("cull.reject", &json!({})).unwrap();
    assert!(s.visible().is_empty());
    assert_eq!(s.source_total(), Some(0));
    assert!(std::fs::read(f.0.join("Rejected/A.ARW")).is_ok());
    assert!(path(&s, id).contains("Rejected"));
    let canonical_journal = rejected["journal_path"].as_str().unwrap();
    let ordinary_journal = canonical_journal.strip_prefix(r"\\?\").unwrap_or(canonical_journal).to_lowercase();
    let restored = s.execute("cull.undo", &json!({"folder":ordinary,"journal_path":ordinary_journal})).unwrap();
    assert_eq!(restored["restored_ids"], json!([id]));
    assert_eq!(s.visible(), &[id]);
    assert_eq!(s.source_total(), Some(1));
    assert_eq!(std::fs::read(f.0.join("A.ARW")).unwrap(), b"raw");
    assert_eq!(std::fs::read(f.0.join("A.xmp")).unwrap(), b"sidecar");
}

#[test]
fn full_name_sidecar_shared_with_derivative_stem_is_refused() {
    let f = Fixture::new();
    f.write("A.ARW", b"raw");
    f.write("A.ARW.JPG", b"derivative");
    f.write("A.ARW.xmp", b"shared sidecar");
    let mut s = f.session();
    f.add(&mut s, "A.ARW");
    assert!(s.execute("cull.reject", &json!({})).unwrap_err().to_string().contains("ambiguous"));
    assert_eq!(std::fs::read(f.0.join("A.ARW.xmp")).unwrap(), b"shared sidecar");
    assert!(f.0.join("A.ARW").exists());
    assert!(f.0.join("A.ARW.JPG").exists());
}
