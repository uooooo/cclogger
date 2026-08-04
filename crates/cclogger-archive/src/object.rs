//! Content-addressed object store for archived source bytes.
//!
//! Objects are named by the sha256 of their content, so re-archiving an unchanged file
//! is a no-op and an appended file simply becomes a new object. Publication is atomic
//! (write to a temp file in the same directory, then rename), and everything is created
//! owner-only: the archive holds raw prompts and code.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// `sha256:<64 hex chars>`.
///
/// The only two ways to build one are [`ObjectId::from_digest`] (crate-internal,
/// infallible, used exclusively with a digest this crate just computed itself) and
/// [`ObjectId::parse`] (public, fallible, used for a `sha256:<hex>` string read back
/// from somewhere we do not control, e.g. a manifest row). Both leave `.0` holding
/// exactly `"sha256:"` plus 64 lowercase hex characters, which is the invariant
/// [`ObjectStore::path`] relies on when it slices `&hex[..2]` / `&hex[2..]` -- so an
/// `ObjectId` can never exist in a state where that slice panics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectId(String);

/// A string that was not a well-formed `sha256:<64 lowercase hex chars>` object id.
///
/// Returned by [`ObjectId::parse`] when a stored value fails validation -- e.g. a
/// truncated or corrupted `source_snapshot.object_id` row -- so the caller gets an
/// error instead of an `ObjectId` that would later panic in [`ObjectStore::path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectIdError(String);

impl std::fmt::Display for ObjectIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a valid object id: {:?}", self.0)
    }
}

impl std::error::Error for ObjectIdError {}

impl ObjectId {
    /// Build an id from a digest this crate just computed (see [`ObjectStore::put`]).
    /// Deliberately `pub(crate)` and unvalidated: every call site passes the output of
    /// `hex_encode(&Sha256::digest(..))`, which is always exactly 64 lowercase hex
    /// characters, so validating here would only re-check an invariant the caller
    /// already guarantees. Never widen this to accept untrusted input -- that is what
    /// [`ObjectId::parse`] is for.
    pub(crate) fn from_digest(hex: impl Into<String>) -> Self {
        Self(format!("sha256:{}", hex.into()))
    }

    /// Parse the `sha256:<hex>` form read back from storage (a manifest row today).
    ///
    /// Unlike [`ObjectId::from_digest`], the input here was not just computed by this
    /// process -- it came out of SQLite, where a corrupted or hand-edited row is
    /// possible -- so this validates the `sha256:` prefix and that exactly 64
    /// lowercase hex characters follow it, and returns [`ObjectIdError`] instead of
    /// producing an `ObjectId` that could later panic in [`ObjectStore::path`].
    pub fn parse(s: &str) -> Result<Self, ObjectIdError> {
        let Some(hex) = s.strip_prefix("sha256:") else {
            return Err(ObjectIdError(s.to_string()));
        };
        let well_formed = hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !well_formed {
            return Err(ObjectIdError(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or(&self.0)
    }
}

impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct ObjectStore {
    objects: PathBuf,
}

/// `pub(crate)`: also used by [`crate::ledger`] to bring the cclog root directory
/// itself (parent of `archive/` and `ledger.db`) up to owner-only before the
/// database file is created inside it.
pub(crate) fn mkdir_owner_only(path: &Path) -> io::Result<()> {
    // Carry the restrictive mode on the creation syscall itself, so a newly created
    // directory is never briefly world/group-readable at the umask-derived default.
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    // Defensive: tighten `path` even if it (or an ancestor `create_dir_all` skipped
    // because it already existed) was left with looser permissions by something else.
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Monotonic per-process counter, appended to temp-file names alongside the pid.
///
/// Two threads in the same process racing to `put()` identical bytes hash to the same
/// digest and would otherwise target the same `.tmp-<pid>-<digest>` path; since
/// `File::create` truncates on open, the second thread could truncate the first
/// thread's in-progress write out from under it, publishing a corrupt object on
/// rename. Combining pid with a fetch_add'd counter makes every `put()` call's temp
/// name unique within (and effectively across) processes, so no two writers can ever
/// target the same temp path.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ObjectStore {
    /// Open (creating if needed) an object store rooted at `root`.
    pub fn open(root: &Path) -> io::Result<Self> {
        mkdir_owner_only(root)?;
        let objects = root.join("objects");
        mkdir_owner_only(&objects)?;
        Ok(Self { objects })
    }

    pub fn path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.hex();
        self.objects.join(&hex[..2]).join(&hex[2..])
    }

    /// Store `bytes`. Returns the id and whether this call created the object.
    pub fn put(&self, bytes: &[u8]) -> io::Result<(ObjectId, bool)> {
        let digest = hex_encode(&Sha256::digest(bytes));
        let id = ObjectId::from_digest(digest);
        let final_path = self.path(&id);
        if final_path.exists() {
            return Ok((id, false));
        }

        let dir = final_path.parent().expect("object path has a parent");
        mkdir_owner_only(dir)?;

        let unique = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(
            ".tmp-{}-{}-{}",
            std::process::id(),
            unique,
            id.hex()
        ));
        publish_via_temp_file(&tmp, &final_path, bytes)?;
        Ok((id, true))
    }

    pub fn read(&self, id: &ObjectId) -> io::Result<Vec<u8>> {
        fs::read(self.path(id))
    }
}

/// Open `tmp` fresh (never truncating something already there) with owner-only
/// permissions carried on the creation syscall itself.
///
/// `create_new` fails loudly (`ErrorKind::AlreadyExists`) instead of silently
/// truncating if the name were ever reused — see [`publish_via_temp_file`] for why
/// that's recoverable rather than fatal.
fn open_temp_owner_only(tmp: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)
}

/// Write `bytes` to `tmp` and publish by renaming it to `final_path`.
///
/// `tmp`'s name is process-unique (see [`TMP_COUNTER`]), so `open_temp_owner_only`
/// hitting `AlreadyExists` here should be unreachable in practice — the only way to
/// reach it is a leftover from a *different* process that crashed between creating
/// its temp file and renaming it (requires PID reuse landing on the same counter
/// value and the same content digest). A leftover temp file was, by definition,
/// never renamed into place, so it was never published as an object: removing it
/// can never destroy archived data. We clear it and retry the create exactly once,
/// so a stale leftover self-heals instead of making every future `put()` of that
/// content fail forever; a second collision propagates as an error rather than
/// looping.
fn publish_via_temp_file(tmp: &Path, final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = final_path.parent().expect("object path has a parent");
    let mut f = match open_temp_owner_only(tmp) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(tmp)?;
            open_temp_owner_only(tmp)?
        }
        Err(e) => return Err(e),
    };
    f.write_all(bytes)?;
    f.sync_all()?;
    // Defensive: guarantee the bits regardless of umask quirks.
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    // Rename is atomic within a directory; a crash leaves either nothing or a
    // complete object, never a truncated one.
    fs::rename(tmp, final_path)?;
    // The rename above is atomic with respect to concurrent readers, but the new
    // directory entry only lives in the parent directory's in-memory state until
    // that directory itself is fsynced — `sync_all()` on the temp file only made the
    // *contents* durable, not the fact that `final_path` now points at them. Without
    // this, a power cut between the rename and the next boot can drop the directory
    // entry while SQLite's own WAL/fsync has already made the manifest row durable,
    // leaving a manifest row that points at an object that was never actually
    // written to disk: exactly the dangling reference the publish-before-commit
    // ordering exists to prevent. Ordinary process crashes don't need this (the
    // kernel's in-memory dentry survives those); only a power-loss / hard-crash
    // scenario does.
    //
    // Note: on macOS, `fsync(2)` (what `File::sync_all` calls) only asks the drive
    // to flush and does not guarantee the write reaches physical media —
    // `F_FULLFSYNC` is required for that stronger guarantee. We do not pull in a
    // `libc` dependency to issue `F_FULLFSYNC` here, so this call narrows the
    // window but does not fully close it on macOS; stated honestly rather than
    // implying a guarantee the code doesn't deliver.
    fs::File::open(dir)?.sync_all()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Per-test root. Tests run in parallel in one process, so each needs its own
    /// directory — a shared base that one test deletes races the others.
    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cclog-obj-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn digest_is_stable_and_matches_the_known_sha256_of_empty_input() {
        let store = ObjectStore::open(&tmp("digest")).unwrap();
        let (id, created) = store.put(b"").unwrap();
        assert!(created);
        assert_eq!(
            id.as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn putting_the_same_bytes_twice_is_idempotent() {
        use std::os::unix::fs::MetadataExt;

        let store = ObjectStore::open(&tmp("idem")).unwrap();
        let (first, created_first) = store.put(b"hello cclog").unwrap();
        assert!(created_first);
        // Capture the inode, not just a timestamp: a regression that kept the `bool`
        // flags correct while still rewriting the file underneath (e.g. a version
        // that always writes-then-renames but reports `created` from a separate
        // existence check) would still pass an assertion on the flags alone. A
        // rewrite-via-rename always produces a new inode, so this is a genuine
        // "left untouched" check rather than a restatement of the flag.
        let ino_before = fs::metadata(store.path(&first)).unwrap().ino();

        let (second, created_second) = store.put(b"hello cclog").unwrap();
        assert_eq!(first.as_str(), second.as_str());
        assert!(!created_second, "second put must not rewrite the object");

        let ino_after = fs::metadata(store.path(&second)).unwrap().ino();
        assert_eq!(
            ino_after, ino_before,
            "second put must not replace the underlying file"
        );
    }

    #[test]
    fn concurrent_puts_of_identical_bytes_from_the_same_process_do_not_corrupt_the_object() {
        // Two threads racing to put() the same bytes both hash to the same digest and
        // both find `final_path` absent, so both proceed to write+rename. Before the
        // per-call unique temp name (TMP_COUNTER), both threads would target the same
        // `.tmp-<pid>-<digest>` path; `File::create` truncates on open, so the second
        // thread's open could truncate the first thread's partially written temp file
        // out from under it, and either the truncated (short) content or an
        // interleaved write could get renamed into place. This test does not
        // reliably fail on that old code — it's a race, not a guaranteed
        // reproduction — so the real guarantee is structural: TMP_COUNTER is a
        // process-wide AtomicU64 and every put() call does exactly one fetch_add
        // before building its temp name, so no two calls in this process can ever
        // compute the same temp path, and `create_new(true)` turns any residual
        // collision into a loud error instead of a silent truncation. This test
        // exercises the path and pins the still-correct end state.
        let root = tmp("concurrent");
        let store = std::sync::Arc::new(ObjectStore::open(&root).unwrap());
        let payload: &'static [u8] = b"same bytes raced from two threads";

        let store_a = std::sync::Arc::clone(&store);
        let t1 = std::thread::spawn(move || store_a.put(payload));
        let store_b = std::sync::Arc::clone(&store);
        let t2 = std::thread::spawn(move || store_b.put(payload));

        let (id1, _) = t1.join().unwrap().unwrap();
        let (id2, _) = t2.join().unwrap().unwrap();
        assert_eq!(id1.as_str(), id2.as_str());
        assert_eq!(store.read(&id1).unwrap(), payload);
    }

    #[test]
    fn put_recovers_from_a_stale_temp_file_left_by_a_crashed_prior_attempt() {
        // Reaching this through `put()` would require guessing the exact value of
        // the process-global TMP_COUNTER at the moment this test's call runs, which
        // is not deterministic when tests run in parallel (other tests bump the
        // same counter concurrently). `publish_via_temp_file` is the private helper
        // `put()` delegates the actual create/retry/write/rename to, so calling it
        // directly with a temp path we choose ourselves exercises the exact same
        // recovery logic deterministically, without widening the public API.
        let root = tmp("stale-tmp");
        let store = ObjectStore::open(&root).unwrap();
        let bytes = b"bytes that would collide with a stale temp file";

        let digest = hex_encode(&Sha256::digest(bytes));
        let id = ObjectId::from_digest(digest);
        let final_path = store.path(&id);
        let dir = final_path.parent().unwrap();
        mkdir_owner_only(dir).unwrap();

        // Simulate a prior process that crashed after creating its temp file but
        // before renaming it into place: a leftover at the exact name the next
        // attempt will use, holding stale content that must not end up published.
        let stale_tmp = dir.join(".tmp-stale-simulated");
        fs::write(&stale_tmp, b"leftover from a crashed write").unwrap();

        publish_via_temp_file(&stale_tmp, &final_path, bytes)
            .expect("a stale temp file must not permanently fail put()");

        assert_eq!(
            fs::read(&final_path).unwrap(),
            bytes,
            "the published object must hold the new bytes, not the stale leftover"
        );
    }

    #[test]
    fn publish_succeeds_and_object_is_readable_after_the_directory_sync() {
        // This does NOT and cannot simulate power loss — there is no way to kill
        // power to the test machine mid-syscall from within a unit test, and a test
        // that pretended to would be lying about what it establishes. All this
        // proves is that adding `File::open(dir)?.sync_all()` after the rename does
        // not itself break the happy path: put() still succeeds, still reports
        // `created`, and the object is still readable back with the right bytes.
        // The actual durability property (a crash between rename and the directory
        // fsync leaves no dangling manifest row) is only verifiable by inspection
        // of the code path, not by an automated test.
        let store = ObjectStore::open(&tmp("dirsync")).unwrap();
        let (id, created) = store.put(b"durable across the directory fsync").unwrap();
        assert!(created);
        assert_eq!(
            store.read(&id).unwrap(),
            b"durable across the directory fsync"
        );
    }

    #[test]
    fn objects_round_trip() {
        let store = ObjectStore::open(&tmp("rt")).unwrap();
        let (id, _) = store.put(b"payload bytes").unwrap();
        assert_eq!(store.read(&id).unwrap(), b"payload bytes");
    }

    #[test]
    fn object_and_directory_permissions_are_owner_only() {
        let root = tmp("perm");
        let store = ObjectStore::open(&root).unwrap();
        let (id, _) = store.put(b"secret-ish").unwrap();

        let file_mode = std::fs::metadata(store.path(&id))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "archive objects must be owner read/write only"
        );

        let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "archive root must be owner-only");
    }

    #[test]
    fn reading_an_unknown_object_is_an_error_not_a_panic() {
        let store = ObjectStore::open(&tmp("missing")).unwrap();
        let id = ObjectId::from_digest("0".repeat(64));
        assert!(store.read(&id).is_err());
    }

    #[test]
    fn parse_accepts_a_well_formed_id_and_round_trips_its_string_form() {
        let raw = format!("sha256:{}", "a".repeat(64));
        let id = ObjectId::parse(&raw).unwrap();
        assert_eq!(id.as_str(), raw);
    }

    #[test]
    fn parse_rejects_a_string_missing_the_sha256_prefix() {
        assert!(ObjectId::parse(&"a".repeat(64)).is_err());
    }

    #[test]
    fn parse_rejects_a_truncated_digest() {
        // One of the ways a corrupted manifest row could look: a short hex tail.
        // This must surface as an error, never panic inside `ObjectStore::path`'s
        // `&hex[..2]` slice.
        assert!(ObjectId::parse("sha256:ab").is_err());
    }

    #[test]
    fn parse_rejects_a_digest_with_non_hex_characters() {
        let raw = format!("sha256:{}", "z".repeat(64));
        assert!(ObjectId::parse(&raw).is_err());
    }

    #[test]
    fn parse_rejects_uppercase_hex() {
        // Every digest this crate ever writes is lowercase (`{:02x}` formatting);
        // accepting uppercase here would just be accepting a form we never produce.
        let raw = format!("sha256:{}", "A".repeat(64));
        assert!(ObjectId::parse(&raw).is_err());
    }
}
