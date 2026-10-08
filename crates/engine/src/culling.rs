//! Durable, reversible folder culling. Originals and sidecars are never rewritten.
//!
//! A manifest and immutable phase markers are synced before moving files. Hard links provide
//! exclusive destination creation on Linux/Windows; unsupported filesystems fail closed. A crash
//! between link and unlink leaves two names for the same verified file and recovery resolves them.
//! Standard Rust cannot lock directory entries against hostile external renames: identity checks
//! detect ordinary concurrent edits, but this is not a defence against an adversarial filesystem.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use lightcraft_catalog::{Op, PhotoId, Source};
use serde::{Deserialize, Serialize};

use crate::{EngineError, Result, Session};

const JOURNAL: &str = ".lightcraft-culling";
const MAX_MANIFEST: u64 = 4 * 1024 * 1024;
const MAX_FILES: usize = 10_000;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn error(message: impl Into<String>) -> EngineError {
    EngineError::Other(message.into())
}
fn io(e: std::io::Error) -> EngineError {
    error(format!("culling: {e}"))
}

pub fn is_workspace_output_dir(name: &str) -> bool {
    ["Rejected", "Processed", ".lightcraft", JOURNAL, "LightCraft Exports"].iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Hide output descendants in recursive folder views, while permitting an explicitly opened
/// output folder to show its own files.
pub(crate) fn in_folder_view(s: &Session, id: PhotoId) -> bool {
    let Some(b) = &s.browse else { return false };
    let Some(Source::File { path }) = s.catalog.photo(id).map(|p| &p.source) else { return false };
    let (Ok(path), Ok(root)) = (normalized(Path::new(path)), normalized(Path::new(&b.path))) else { return false };
    let Ok(relative) = path.strip_prefix(root) else { return false };
    relative.parent().is_some_and(|parent| !parent.components().any(|c| c.as_os_str().to_str().is_some_and(is_workspace_output_dir)))
}

/// Refuse symlinks in every existing ancestor, including broken links.
fn checked_path(path: &Path) -> Result<()> {
    for p in path.ancestors() {
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_symlink() => return Err(error(format!("symbolic-link path refused: {}", p.display()))),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
    }
    Ok(())
}

pub(crate) fn folder(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path).map_err(io)?;
    checked_path(&absolute)?;
    if !absolute.is_dir() {
        return Err(error("choose a real source folder"));
    }
    fs::canonicalize(absolute).map_err(io)
}

fn directory(path: &Path) -> Result<()> {
    checked_path(path)?;
    if fs::symlink_metadata(path).is_err() {
        fs::create_dir(path).map_err(io)?;
        sync_parent(path)?;
    }
    if !fs::symlink_metadata(path).map_err(io)?.is_dir() {
        return Err(error(format!("not a real directory: {}", path.display())));
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent).and_then(|f| f.sync_all()).map_err(io)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The OS releases this advisory lock after a crash. All LightCraft mutations in a source
/// folder use it; external editors remain subject to identity and collision checks.
fn lock(root: &Path) -> Result<File> {
    let journal = root.join(JOURNAL);
    directory(&journal)?;
    let path = journal.join(".lock");
    checked_path(&path)?;
    let file = match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(&path).map_err(io)?.is_file() {
                return Err(error("invalid culling lock"));
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .or_else(|e| if e.kind() == std::io::ErrorKind::PermissionDenied { File::open(&path) } else { Err(e) })
                .map_err(io)?
        }
        Err(e) => return Err(io(e)),
    };
    file.try_lock().map_err(|e| error(format!("folder culling is already in use or locking unsupported: {e}")))?;
    Ok(file)
}

#[cfg(test)]
thread_local! {
    static FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static CRASHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(test)]
pub(crate) fn inject_fault(phase: &'static str) {
    FAULT.with(|f| f.set(Some(phase)));
}
fn fault(phase: &str) -> Result<()> {
    #[cfg(test)]
    if FAULT.with(|f| f.get() == Some(phase)) {
        FAULT.with(|f| f.set(None));
        CRASHED.with(|f| f.set(phase.starts_with("after-")));
        return Err(error(format!("injected crash at {phase}")));
    }
    let _ = phase;
    Ok(())
}
fn crashed() -> bool {
    #[cfg(test)]
    {
        CRASHED.with(|f| f.replace(false))
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    size: u64,
    modified: String,
    created: Option<String>,
    digest: String,
    device: Option<u64>,
    inode: Option<u64>,
}

fn timestamp(t: std::io::Result<std::time::SystemTime>) -> Option<String> {
    t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|t| t.as_nanos().to_string())
}

fn identify(path: &Path) -> Result<Identity> {
    checked_path(path)?;
    let before = fs::symlink_metadata(path).map_err(io)?;
    if !before.is_file() {
        return Err(error(format!("not a regular file: {}", path.display())));
    }
    #[cfg(windows)]
    if before.permissions().readonly() {
        // Windows rejects unlinking read-only files; fail before creating a second hard link
        // rather than leave an undeletable duplicate. Do not modify the user's attributes.
        return Err(error(format!("read-only file cannot be culled safely on Windows: {}", path.display())));
    }
    let mut file = File::open(path).map_err(io)?.take(before.len().saturating_add(1));
    let mut hash = lightcraft_preview::Hasher128::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        hash.update(buffer.get(..n).ok_or_else(|| error("invalid file read length"))?);
    }
    let after = file.get_ref().metadata().map_err(io)?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(error(format!("file changed while reading: {}", path.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(error("file replaced while reading"));
        }
    }
    Ok(Identity {
        size: after.len(),
        modified: timestamp(after.modified()).ok_or_else(|| error("file modification time unavailable"))?,
        created: timestamp(after.created()),
        digest: hash.finish().to_string(),
        #[cfg(unix)]
        device: {
            use std::os::unix::fs::MetadataExt;
            Some(after.dev())
        },
        #[cfg(not(unix))]
        device: None,
        #[cfg(unix)]
        inode: {
            use std::os::unix::fs::MetadataExt;
            Some(after.ino())
        },
        #[cfg(not(unix))]
        inode: None,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    identity: Identity,
    /// Catalog ids are informational: recovery matches paths, never trusts stale ids.
    photo_id: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    folder: String,
    files: Vec<Entry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Transaction {
    pub transaction_id: String,
    pub journal_path: String,
    pub state: String,
    #[serde(skip)]
    manifest: Manifest,
}

fn single_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':'])
        && Path::new(name).components().count() == 1
        && matches!(Path::new(name).components().next(), Some(Component::Normal(_)))
        && !name.ends_with(['.', ' '])
}

fn absent(path: &Path) -> Result<bool> {
    checked_path(path)?;
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(io(e)),
        Ok(_) => Ok(false),
    }
}

/// Refuse case-insensitive conflicts on every platform, making portable folders predictable.
fn free_name(dir: &Path, name: &str) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(io)? {
        let entry = entry.map_err(io)?;
        if entry.file_name().to_string_lossy().eq_ignore_ascii_case(name) {
            return Err(error(format!("destination already contains {name}; no overwrite allowed")));
        }
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    checked_path(path)?;
    let mut f = OpenOptions::new().write(true).create_new(true).open(path).map_err(io)?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(io)?;
    sync_parent(path)
}

fn marker(tx: &Transaction, name: &str) -> Result<()> {
    write_new(&Path::new(&tx.journal_path).join(name), b"")
}

fn has_marker(dir: &Path, name: &str) -> Result<bool> {
    let path = dir.join(name);
    if absent(&path)? {
        return Ok(false);
    }
    let metadata = fs::symlink_metadata(&path).map_err(io)?;
    if !metadata.is_file() || metadata.len() != 0 {
        return Err(error("malformed culling phase marker; files untouched"));
    }
    Ok(true)
}

fn load(dir: &Path, root: &Path) -> Result<Transaction> {
    checked_path(dir)?;
    let name = dir.file_name().and_then(|s| s.to_str()).filter(|s| single_name(s)).ok_or_else(|| error("invalid transaction name"))?;
    let mut parts = name.split('-');
    let number = parts.next().unwrap_or("");
    if number.len() != 39
        || !number.bytes().all(|b| b.is_ascii_digit())
        || number.parse::<u128>().is_err()
        || parts.next().is_none_or(|n| n.parse::<u32>().is_err())
        || parts.next().is_none_or(|n| n.parse::<u64>().is_err())
        || parts.next().is_some()
    {
        return Err(error("invalid culling transaction identifier"));
    }
    if !has_marker(dir, "prepared")? {
        for phase in ["committed", "undoing", "undone", "aborted"] {
            if has_marker(dir, phase)? {
                return Err(error("unprepared culling journal has phase markers"));
            }
        }
        // No media can move until `prepared` is durable. Leave incomplete preparation intact
        // for inspection while allowing the folder to remain usable after a disk-full failure.
        return Ok(Transaction {
            transaction_id: name.to_string(),
            journal_path: dir.to_string_lossy().to_string(),
            state: "incomplete".into(),
            manifest: Manifest { version: 1, folder: root.to_string_lossy().to_string(), files: Vec::new() },
        });
    }
    let path = dir.join("manifest.json");
    checked_path(&path)?;
    let metadata = fs::symlink_metadata(&path).map_err(io)?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST {
        return Err(error("invalid or oversized culling manifest"));
    }
    let mut bytes = Vec::new();
    File::open(&path).map_err(io)?.take(MAX_MANIFEST + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(error("oversized culling manifest"));
    }
    let mut manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| error(format!("invalid culling manifest: {e}")))?;
    if manifest.version != 1 || manifest.files.is_empty() || manifest.files.len() > MAX_FILES {
        return Err(error("culling manifest does not describe this folder"));
    }
    // Entries only contain validated basenames; rebase to the journal's actual folder so a
    // same-volume folder rename preserves recoverability without trusting persisted paths.
    manifest.folder = root.to_string_lossy().to_string();
    let mut names = HashSet::new();
    for e in &manifest.files {
        if !single_name(&e.name) || !names.insert(e.name.to_lowercase()) || lightcraft_preview::Hash128::parse(&e.identity.digest).is_none() {
            return Err(error("invalid or ambiguous culling file name/identity"));
        }
    }
    let photos: Vec<&Entry> = manifest.files.iter().filter(|e| e.photo_id.is_some()).collect();
    if photos.is_empty() || photos.iter().any(|e| !crate::import::is_supported(Path::new(&e.name))) {
        return Err(error("culling journal requires supported photo files"));
    }
    for entry in manifest.files.iter().filter(|e| e.photo_id.is_none()) {
        let path = Path::new(&entry.name);
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else { return Err(error("invalid sidecar in culling journal")) };
        let Some(base) = path.file_stem().and_then(|s| s.to_str()) else { return Err(error("invalid sidecar filename")) };
        if !["xmp", "dop"].iter().any(|e| ext.eq_ignore_ascii_case(e))
            || !photos.iter().any(|photo| {
                base.eq_ignore_ascii_case(&photo.name)
                    || Path::new(&photo.name).file_stem().and_then(|s| s.to_str()).is_some_and(|stem| base.eq_ignore_ascii_case(stem))
            })
        {
            return Err(error("unassociated sidecar in culling journal"));
        }
    }
    let committed = has_marker(dir, "committed")?;
    let undoing = has_marker(dir, "undoing")?;
    let undone = has_marker(dir, "undone")?;
    let aborted = has_marker(dir, "aborted")?;
    if (undoing && !committed) || (undone && !undoing) || (aborted && committed) {
        return Err(error("inconsistent culling phase markers"));
    }
    let state = if undone {
        "undone"
    } else if aborted {
        "aborted"
    } else if undoing {
        "undoing"
    } else if committed {
        "committed"
    } else {
        "pending"
    };
    Ok(Transaction { transaction_id: name.to_string(), journal_path: dir.to_string_lossy().to_string(), state: state.into(), manifest })
}

pub(crate) fn history(root: &Path) -> Result<Vec<Transaction>> {
    let root = folder(root)?;
    let journal = root.join(JOURNAL);
    if absent(&journal)? {
        return Ok(Vec::new());
    }
    checked_path(&journal)?;
    let mut transactions = Vec::new();
    for entry in fs::read_dir(journal).map_err(io)? {
        let entry = entry.map_err(io)?;
        if entry.file_name() == ".lock" {
            checked_path(&entry.path())?;
            if !entry.file_type().map_err(io)?.is_file() {
                return Err(error("invalid culling lock entry"));
            }
            continue;
        }
        if !entry.file_type().map_err(io)?.is_dir() {
            return Err(error("unexpected entry in culling journal"));
        }
        if transactions.len() >= MAX_FILES {
            return Err(error("too many culling transactions"));
        }
        transactions.push(load(&entry.path(), &root)?);
    }
    transactions.sort_by(|a, b| a.transaction_id.cmp(&b.transaction_id));
    Ok(transactions)
}

fn matches(path: &Path, expected: &Identity) -> Result<()> {
    if identify(path)? != *expected {
        return Err(error(format!("file identity changed: {}; recovery refused", path.display())));
    }
    Ok(())
}

fn endpoints(tx: &Transaction, entry: &Entry) -> (PathBuf, PathBuf) {
    let source = Path::new(&tx.manifest.folder).join(&entry.name);
    let destination = Path::new(&tx.manifest.folder).join("Rejected").join(&entry.name);
    (source, destination)
}

/// Move using exclusive link creation; never fall back to overwriting rename or uncertain copy.
fn move_verified(from: &Path, to: &Path, identity: &Identity) -> Result<()> {
    matches(from, identity)?;
    if !absent(to)? {
        return Err(error(format!("destination occupied: {}", to.display())));
    }
    if let (Some(dir), Some(name)) = (to.parent(), to.file_name().and_then(|n| n.to_str())) {
        free_name(dir, name)?;
    }
    fs::hard_link(from, to).map_err(|e| error(format!("exclusive hard-link move unsupported or failed: {e}; original retained")))?;
    // Failure leaves both names for recovery; neither unverified file is removed.
    // On Unix fsync accepts a read handle. Windows FlushFileBuffers requires write access;
    // media bytes were not changed, so do not flush a read-only media handle there. Manifest
    // and marker write handles are flushed on every platform. Directory-entry power-loss
    // durability on Windows depends on the filesystem: std Rust exposes no directory fsync.
    #[cfg(unix)]
    File::open(to).and_then(|f| f.sync_all()).map_err(io)?;
    sync_parent(to)?;
    fault("after-link")?;
    matches(from, identity)?;
    matches(to, identity)?;
    fault("fail-before-unlink")?;
    fs::remove_file(from).map_err(io)?;
    sync_parent(from)?;
    fault("after-unlink")
}

/// All files are inspected before changing any entry; resume a move interrupted after link.
fn settle(tx: &Transaction, original: bool) -> Result<()> {
    let mut work = Vec::new();
    for entry in &tx.manifest.files {
        let (src, dst) = endpoints(tx, entry);
        let (from, to) = if original { (dst, src) } else { (src, dst) };
        let from_present = !absent(&from)?;
        let to_present = !absent(&to)?;
        if !from_present && !to_present {
            return Err(error(format!("both transaction paths missing: {}", entry.name)));
        }
        if from_present {
            matches(&from, &entry.identity)?;
        }
        if to_present {
            matches(&to, &entry.identity)?;
        }
        // On Unix matching identity includes inode. Other platforms compare content/time.
        work.push((entry, from, to, from_present, to_present));
    }
    for (entry, from, to, from_present, to_present) in work {
        if !from_present {
            continue;
        }
        if to_present {
            matches(&from, &entry.identity)?;
            matches(&to, &entry.identity)?;
            fs::remove_file(&from).map_err(io)?;
            sync_parent(&from)?;
        } else {
            move_verified(&from, &to, &entry.identity)?;
        }
    }
    Ok(())
}

fn normalized(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path).map_err(io)?;
    #[cfg(windows)]
    let absolute = PathBuf::from(lightcraft_catalog::query::folder_key(absolute.to_str().ok_or_else(|| error("unsupported path encoding"))?));
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                if result.file_name().is_some() {
                    result.pop();
                }
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}

/// Catalog reconciliation happens only after a complete file transaction. Recovery uses paths
/// rather than manifest ids, which may belong to another catalog after restarting.
pub(crate) fn reconcile(s: &mut Session, tx: &Transaction, original: bool) -> Result<Vec<PhotoId>> {
    let mut ops = Vec::new();
    let mut ids = Vec::new();
    let mut paths: HashMap<PathBuf, Vec<PhotoId>> = HashMap::new();
    for photo in s.catalog.photos() {
        if let Source::File { path } = &photo.source {
            paths.entry(normalized(Path::new(path))?).or_default().push(photo.id);
        }
    }
    for entry in tx.manifest.files.iter().filter(|e| e.photo_id.is_some()) {
        let (old, new) = endpoints(tx, entry);
        let (from, to) = if original { (new, old) } else { (old, new) };
        if let Some(existing) = paths.get(&to) {
            ids.extend(existing);
        }
        let Some(relinked) = paths.get(&from) else { continue };
        if !absent(&from)? {
            return Err(error(format!("catalog recovery is ambiguous: original path reused at {}; no relink performed", from.display())));
        }
        matches(&to, &entry.identity)?;
        for id in relinked {
            ids.push(*id);
            ops.push(Op::Relink {
                id: *id,
                file_name: entry.name.clone(),
                source: Source::File { path: to.to_string_lossy().to_string() },
                format: None,
            });
        }
    }
    if !ops.is_empty() {
        let op = Op::Batch { ops };
        s.catalog.apply(op.clone())?;
        s.pending_log.push(op);
        for id in &ids {
            s.media.forget(*id);
        }
    }
    s.skip_auto_write = true;
    Ok(ids)
}

fn latest_owners(transactions: &[Transaction]) -> HashMap<String, &str> {
    let mut owners = HashMap::new();
    for tx in transactions.iter().rev() {
        for entry in &tx.manifest.files {
            owners.entry(entry.name.to_lowercase()).or_insert(tx.transaction_id.as_str());
        }
    }
    owners
}

fn superseded(tx: &Transaction, owners: &HashMap<String, &str>) -> bool {
    tx.manifest.files.iter().any(|e| owners.get(&e.name.to_lowercase()).is_some_and(|owner| *owner != tx.transaction_id))
}

pub(crate) fn recover(s: &mut Session, root: &Path) -> Result<Vec<Transaction>> {
    let root = folder(root)?;
    if absent(&root.join(JOURNAL))? {
        return Ok(Vec::new());
    }
    let _lock = lock(&root)?;
    let _ = crashed();
    recover_unlocked(s, &root)
}

fn recover_unlocked(s: &mut Session, root: &Path) -> Result<Vec<Transaction>> {
    let transactions = history(root)?;
    let owners = latest_owners(&transactions);
    if transactions.iter().any(|tx| (tx.state == "pending" || tx.state == "undoing") && superseded(tx, &owners)) {
        return Err(error("overlapping pending culling transactions; recovery refused before moving files"));
    }
    for tx in &transactions {
        if tx.state == "pending" || tx.state == "undoing" {
            settle(tx, true)?;
            marker(tx, if tx.state == "pending" { "aborted" } else { "undone" })?;
        }
    }
    // A later transaction supersedes each file individually; one shared entry must not hide
    // unrelated photos from a previous multi-photo operation.
    let transactions = history(root)?;
    let mut seen = HashSet::new();
    for tx in transactions.iter().rev() {
        let mut latest = tx.clone();
        latest.manifest.files.retain(|e| seen.insert(e.name.to_lowercase()));
        reconcile(s, &latest, tx.state != "committed")?;
    }
    history(root)
}

fn group(root: &Path, raw: &Path, id: u64) -> Result<Vec<Entry>> {
    let name = raw.file_name().and_then(|n| n.to_str()).filter(|n| single_name(n)).ok_or_else(|| error("unsupported photo filename"))?;
    let stem = raw.file_stem().and_then(|n| n.to_str()).ok_or_else(|| error("unsupported photo stem"))?;
    let mut paths = vec![raw.to_path_buf()];
    let entries = fs::read_dir(root).map_err(io)?.take(1_000_001).collect::<std::io::Result<Vec<_>>>().map_err(io)?;
    if entries.len() > 1_000_000 {
        return Err(error("folder has too many entries for safe culling"));
    }
    for entry in &entries {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else { continue };
        if !["xmp", "dop"].iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            continue;
        }
        let Some(base) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        if !base.eq_ignore_ascii_case(name) && !base.eq_ignore_ascii_case(stem) {
            continue;
        }
        // Stem-only sidecars can be shared by RAW/JPEG pairs. Full-name sidecars also
        // become ambiguous when two photo filenames differ only in case.
        if entries.iter().any(|other| {
            let p = other.path();
            p != raw
                && crate::import::is_supported(&p)
                && (p.file_name().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(base))
                    || p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case(base)))
        }) {
            return Err(error(format!("ambiguous sidecar ownership for {base}; no files moved")));
        }
        paths.push(path);
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let filename = path
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| single_name(s))
                .ok_or_else(|| error("unsupported sidecar filename"))?
                .to_string();
            Ok(Entry { name: filename, identity: identify(&path)?, photo_id: (path == raw).then_some(id) })
        })
        .collect()
}

pub(crate) fn reject(s: &mut Session, ids: &[PhotoId]) -> Result<Transaction> {
    if ids.is_empty() || ids.len() > MAX_FILES {
        return Err(error("select between 1 and 10000 photos"));
    }
    let mut root = None;
    let mut raws = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(*id) {
            return Err(error("duplicate photo id"));
        }
        let photo = s.catalog.photo(*id).ok_or_else(|| error("photo not found"))?;
        let Source::File { path } = &photo.source else { return Err(error("culling requires original files")) };
        let raw = std::path::absolute(Path::new(path)).map_err(io)?;
        if !crate::import::is_supported(&raw) {
            return Err(error("culling requires supported photo files"));
        }
        checked_path(&raw)?;
        let parent = folder(raw.parent().ok_or_else(|| error("photo has no parent folder"))?)?;
        if parent.file_name().is_some_and(|n| n.eq_ignore_ascii_case("Rejected")) {
            return Err(error("cannot reject files from Rejected"));
        }
        if root.as_ref().is_some_and(|r| *r != parent) {
            return Err(error("reject one source folder at a time"));
        }
        root = Some(parent.clone());
        raws.push((*id, parent.join(raw.file_name().ok_or_else(|| error("photo has no filename"))?)));
    }
    let root = root.ok_or_else(|| error("no source folder"))?;
    let _lock = lock(&root)?;
    let previous = recover_unlocked(s, &root)?;
    let mut files = Vec::new();
    let mut names = HashSet::new();
    for (id, raw) in raws {
        checked_path(&raw)?;
        let raw = fs::canonicalize(&raw).map_err(io)?;
        if normalized(raw.parent().ok_or_else(|| error("photo has no parent"))?)? != normalized(&root)? {
            return Err(error("source file escaped its folder"));
        }
        for entry in group(&root, &raw, id.0)? {
            if !names.insert(entry.name.to_lowercase()) {
                return Err(error("ambiguous or overlapping file groups"));
            }
            files.push(entry);
        }
    }
    if files.len() > MAX_FILES {
        return Err(error("too many files in culling group"));
    }
    let rejected = root.join("Rejected");
    for entry in fs::read_dir(&root).map_err(io)? {
        let name = entry.map_err(io)?.file_name();
        if name.eq_ignore_ascii_case("Rejected") && name != "Rejected" {
            return Err(error("differently cased Rejected directory exists"));
        }
    }
    directory(&rejected)?;
    for entry in &files {
        free_name(&rejected, &entry.name)?;
    }
    let journal = root.join(JOURNAL);
    directory(&journal)?;
    let now = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| error(e.to_string()))?.as_nanos();
    let last = previous.last().and_then(|tx| tx.transaction_id.split('-').next()).and_then(|n| n.parse::<u128>().ok()).unwrap_or(0);
    let nanos = now.max(last.checked_add(1).ok_or_else(|| error("transaction sequence exhausted"))?);
    let name = format!("{nanos:039}-{}-{:08}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let dir = journal.join(&name);
    fs::create_dir(&dir).map_err(io)?;
    sync_parent(&dir)?;
    let tx = Transaction {
        transaction_id: name,
        journal_path: dir.to_string_lossy().to_string(),
        state: "pending".into(),
        manifest: Manifest { version: 1, folder: root.to_string_lossy().to_string(), files },
    };
    let bytes = serde_json::to_vec(&tx.manifest).map_err(|e| error(e.to_string()))?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(error("culling manifest exceeds safety limit"));
    }
    write_new(&dir.join("manifest.json"), &bytes)?;
    marker(&tx, "prepared")?;
    if let Err(e) = settle(&tx, false).and_then(|()| marker(&tx, "committed")) {
        if crashed() {
            return Err(error(format!("{e}; recover journal {}", tx.journal_path)));
        }
        let rollback = settle(&tx, true).and_then(|()| marker(&tx, "aborted"));
        return Err(error(format!(
            "{e}; {}",
            match rollback {
                Ok(()) => "originals restored".into(),
                Err(e) => format!("recovery required at {}: {e}", tx.journal_path),
            }
        )));
    }
    reconcile(s, &tx, false)?;
    Ok(Transaction { state: "committed".into(), ..tx })
}

pub(crate) fn undo(s: &mut Session, root: &Path, explicit: Option<&str>) -> Result<Transaction> {
    let root = folder(root)?;
    let _lock = lock(&root)?;
    let transactions = recover_unlocked(s, &root)?;
    let owners = latest_owners(&transactions);
    let index = match explicit {
        Some(path) => {
            let requested = normalized(Path::new(path))?;
            transactions
                .iter()
                .position(|tx| normalized(Path::new(&tx.journal_path)).is_ok_and(|actual| actual == requested) && tx.state == "committed")
        }
        None => transactions.iter().rposition(|tx| tx.state == "committed" && !superseded(tx, &owners)),
    }
    .ok_or_else(|| error("no completed current rejection to undo"))?;
    let tx = transactions.get(index).ok_or_else(|| error("culling transaction disappeared"))?;
    if superseded(tx, &owners) {
        return Err(error("a newer culling transaction owns these files; undo the current rejection instead"));
    }
    let tx = tx.clone();
    // Undo requires every original name to remain vacant; matching contents do not permit overwrite.
    for entry in &tx.manifest.files {
        let (src, dst) = endpoints(&tx, entry);
        if !absent(&src)? {
            return Err(error(format!("undo destination occupied: {}", src.display())));
        }
        free_name(&root, &entry.name)?;
        matches(&dst, &entry.identity)?;
    }
    marker(&tx, "undoing")?;
    settle(&tx, true).map_err(|e| error(format!("{e}; retry cull.recover for {}", tx.journal_path)))?;
    marker(&tx, "undone")?;
    reconcile(s, &tx, true)?;
    Ok(Transaction { state: "undone".into(), ..tx })
}

pub(crate) fn moved(tx: &Transaction, original: bool) -> serde_json::Value {
    serde_json::Value::Array(
        tx.manifest
            .files
            .iter()
            .filter_map(|entry| {
                let id = entry.photo_id?;
                let (old, new) = endpoints(tx, entry);
                let (old, new) = if original { (new, old) } else { (old, new) };
                Some(serde_json::json!({"id":id,"old_path":old,"new_path":new}))
            })
            .collect(),
    )
}
