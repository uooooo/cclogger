//! One-time, non-destructive migration from an M0 `<archive_root>/manifest.db` to the
//! M1 unified `<cclog_root>/ledger.db` (see `crate::ledger` for why the two tables
//! moved).
//!
//! This module never writes to, truncates, or deletes `manifest.db`: it is opened
//! read-only, and the migration's job is done the moment the new database has been
//! built and verified against it. `manifest.db` may hold the only surviving copy of
//! transcripts vendors have already deleted from their own retention window, so
//! leaving it untouched is the whole point -- a bug in the *new* path must never be
//! able to destroy data reachable only through the *old* one.

use crate::ledger::{Ledger, LedgerError};
use crate::object::ObjectId;
use rusqlite::{Connection, OpenFlags, params};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One `(source_locator, object_id)` pair from the source manifest with no matching
/// row in the destination ledger after the copy -- see [`MigrationReport::missing_snapshots`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSnapshot {
    pub source_locator: String,
    pub object_id: String,
}

/// Outcome of one migration run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Row count of the source manifest's `source_snapshot`, for display only. A raw
    /// count comparison against the destination is not a correctness check once the
    /// destination can already hold rows this migration did not put there (ordinary
    /// `cclog archive` use, or a previous run of this same migration) -- see
    /// `missing_snapshots` for the check that replaces it.
    pub source_snapshot_count: i64,
    /// Every `(source_locator, object_id)` pair present in the source manifest that
    /// has no matching row in the destination after the copy. Empty on a clean
    /// migration.
    ///
    /// This, not a row count, is the property the migration actually needs to
    /// guarantee. `INSERT OR IGNORE` with explicit `snapshot_id`s silently drops a
    /// row when that id is already taken in the destination (e.g. by a live
    /// `cclog archive` row written before the migration ran); a table-count
    /// comparison can still match after that -- the destination merely has a
    /// different row occupying the id -- so only checking that the specific pair
    /// this migration needed to preserve actually landed can catch it.
    pub missing_snapshots: Vec<MissingSnapshot>,
    /// How many distinct `object_id`s in the *new* database resolved to a readable
    /// file under `<cclog_root>/archive/objects`.
    pub objects_verified: u64,
    /// `object_id`s that did not resolve (missing file, or not a well-formed id).
    /// Empty on a clean migration.
    pub objects_missing: Vec<String>,
}

impl MigrationReport {
    /// Every source row is provably present in the destination and every object
    /// resolved to a readable file.
    pub fn is_clean(&self) -> bool {
        self.missing_snapshots.is_empty() && self.objects_missing.is_empty()
    }
}

/// What to do when the destination's `source_object` / `source_snapshot` tables
/// already hold rows before this migration starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingRows {
    /// Refuse and make no changes. The default, safe choice: a migration into a
    /// populated ledger is not a normal operation and should not happen by accident
    /// (see `MigrateError::NonEmptyDestination`).
    Refuse,
    /// Proceed anyway. This does not by itself make the migration safe -- explicit
    /// `snapshot_id`s can still collide with whatever the destination already has --
    /// it only says the caller has acknowledged the destination is non-empty.
    /// [`MigrationReport::missing_snapshots`] is what actually catches a collision.
    Proceed,
}

/// Everything that can keep [`migrate_manifest_to_ledger`] from running.
#[derive(Debug)]
pub enum MigrateError {
    Ledger(LedgerError),
    /// The destination already held rows in `source_object` or `source_snapshot` and
    /// the caller did not pass [`ExistingRows::Proceed`].
    NonEmptyDestination {
        existing_object_rows: i64,
        existing_snapshot_rows: i64,
    },
    /// `archive_root` was not `<cclog_root>/archive`. `Ledger::open` always points
    /// its object store at `<cclog_root>/archive`, so a migration from any other
    /// archive root would produce a ledger whose `source_object` rows resolve to
    /// nothing -- every migrated object unreadable. Rather than silently building
    /// that broken ledger, this is rejected up front.
    UnsupportedArchiveRoot {
        expected: PathBuf,
        actual: PathBuf,
    },
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::Ledger(e) => write!(f, "{e}"),
            MigrateError::NonEmptyDestination {
                existing_object_rows,
                existing_snapshot_rows,
            } => write!(
                f,
                "destination already holds {existing_object_rows} source_object row(s) and \
                 {existing_snapshot_rows} source_snapshot row(s); refusing to migrate into a \
                 populated ledger without an explicit opt-in"
            ),
            MigrateError::UnsupportedArchiveRoot { expected, actual } => write!(
                f,
                "--archive-root {} is not the default {}; migrating from a customized archive \
                 root is not supported (the destination ledger's object store is always \
                 <cclog-root>/archive, so migrated objects from any other root would be \
                 unresolvable)",
                actual.display(),
                expected.display()
            ),
        }
    }
}

impl std::error::Error for MigrateError {}

impl From<LedgerError> for MigrateError {
    fn from(e: LedgerError) -> Self {
        MigrateError::Ledger(e)
    }
}

impl From<rusqlite::Error> for MigrateError {
    fn from(e: rusqlite::Error) -> Self {
        MigrateError::Ledger(LedgerError::from(e))
    }
}

/// Build (or update) `<cclog_root>/ledger.db` from `<archive_root>/manifest.db`,
/// then verify it.
///
/// `archive_root` must be exactly `<cclog_root>/archive` -- see
/// `MigrateError::UnsupportedArchiveRoot`. The archive object store never moves, and
/// `Ledger::open` always looks for it at `<cclog_root>/archive`; a caller who
/// customized `--archive-root` away from that default is rejected rather than
/// silently handed a ledger whose objects cannot be read back.
///
/// `existing_rows` gates what happens if the destination is not empty when this
/// starts -- see [`ExistingRows`]. Every row is copied with `INSERT OR IGNORE` keyed
/// on the same constraints `crate::ledger::Ledger` already enforces (`source_object`'s
/// primary key, `source_snapshot`'s `UNIQUE(source_locator, object_id)`), and
/// `source_snapshot.snapshot_id` is copied explicitly rather than left to
/// `AUTOINCREMENT`. Re-running this against the same `manifest.db` with
/// `ExistingRows::Proceed` reproduces the same clean report (every source row is
/// already present); the returned [`MigrationReport::missing_snapshots`] is what
/// verifies that held, rather than assuming it from the copy alone.
///
/// The durable `manifest_already_migrated` marker is set only if the returned
/// report is clean (checked *after* the copy transaction commits, using the same
/// containment and object checks the report itself reports) -- a migration that
/// copied something but left rows missing or objects unresolvable must not durably
/// claim to be migrated.
pub fn migrate_manifest_to_ledger(
    archive_root: &Path,
    cclog_root: &Path,
    existing_rows: ExistingRows,
) -> Result<MigrationReport, MigrateError> {
    let expected_archive_root = cclog_root.join("archive");
    if archive_root != expected_archive_root {
        return Err(MigrateError::UnsupportedArchiveRoot {
            expected: expected_archive_root,
            actual: archive_root.to_path_buf(),
        });
    }

    let old_db_path = archive_root.join("manifest.db");
    // SQLITE_OPEN_READ_ONLY, not just "don't call any write method": this is a
    // structural guarantee that nothing this function does can modify `manifest.db`,
    // not merely a discipline this function's authors intended to keep.
    let old = Connection::open_with_flags(&old_db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(LedgerError::from)?;

    let mut ledger = Ledger::open(cclog_root)?;

    let existing_object_rows: i64 =
        ledger
            .db
            .query_row("SELECT COUNT(*) FROM source_object", [], |r| r.get(0))?;
    let existing_snapshot_rows: i64 =
        ledger
            .db
            .query_row("SELECT COUNT(*) FROM source_snapshot", [], |r| r.get(0))?;
    if existing_rows == ExistingRows::Refuse
        && (existing_object_rows > 0 || existing_snapshot_rows > 0)
    {
        return Err(MigrateError::NonEmptyDestination {
            existing_object_rows,
            existing_snapshot_rows,
        });
    }

    let source_snapshot_count: i64 =
        old.query_row("SELECT COUNT(*) FROM source_snapshot", [], |r| r.get(0))?;

    {
        let tx = ledger.db.transaction()?;
        {
            let mut stmt =
                old.prepare("SELECT object_id, size_bytes, created_at FROM source_object")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let object_id: String = row.get(0)?;
                let size_bytes: i64 = row.get(1)?;
                let created_at: String = row.get(2)?;
                tx.execute(
                    "INSERT OR IGNORE INTO source_object (object_id, size_bytes, created_at)
                     VALUES (?1, ?2, ?3)",
                    params![object_id, size_bytes, created_at],
                )?;
            }
        }
        {
            // `snapshot_id` is carried over explicitly (not regenerated) so migrated
            // rows keep the same identity and relative order they had in
            // `manifest.db`; SQLite's `AUTOINCREMENT` tracks the high-water mark in
            // `sqlite_sequence` regardless of whether the id came from the sequence
            // or was supplied directly, so any snapshot ingested into this ledger
            // afterwards still gets a fresh, non-colliding id. The cost of that
            // choice is exactly what `missing_snapshots` below exists to catch: an
            // explicit id can collide with a row already occupying it in the
            // destination, in which case this `INSERT OR IGNORE` silently drops the
            // migrated row rather than erroring.
            let mut stmt = old.prepare(
                "SELECT snapshot_id, source_kind, source_locator, object_id, format_fingerprint, acquired_at
                 FROM source_snapshot",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let snapshot_id: i64 = row.get(0)?;
                let source_kind: String = row.get(1)?;
                let source_locator: String = row.get(2)?;
                let object_id: String = row.get(3)?;
                let format_fingerprint: Option<String> = row.get(4)?;
                let acquired_at: String = row.get(5)?;
                tx.execute(
                    "INSERT OR IGNORE INTO source_snapshot
                       (snapshot_id, source_kind, source_locator, object_id, format_fingerprint, acquired_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        snapshot_id,
                        source_kind,
                        source_locator,
                        object_id,
                        format_fingerprint,
                        acquired_at
                    ],
                )?;
            }
        }
        tx.commit()?;
    }

    // Set containment, not a row count: for every `(source_locator, object_id)` pair
    // the source manifest has, there must be a matching row in the destination now.
    // Unlike a count, this cannot be satisfied by coincidence -- see
    // `MigrationReport::missing_snapshots`.
    let mut destination_pairs: HashSet<(String, String)> = HashSet::new();
    {
        let mut stmt = ledger
            .db
            .prepare("SELECT source_locator, object_id FROM source_snapshot")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            destination_pairs.insert((row.get(0)?, row.get(1)?));
        }
    }
    let mut missing_snapshots = Vec::new();
    {
        let mut stmt = old.prepare("SELECT source_locator, object_id FROM source_snapshot")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let source_locator: String = row.get(0)?;
            let object_id: String = row.get(1)?;
            if !destination_pairs.contains(&(source_locator.clone(), object_id.clone())) {
                missing_snapshots.push(MissingSnapshot {
                    source_locator,
                    object_id,
                });
            }
        }
    }

    let (verified, missing) = verify_objects(&ledger)?;

    let report = MigrationReport {
        source_snapshot_count,
        missing_snapshots,
        objects_verified: verified,
        objects_missing: missing,
    };

    // The marker is written only now, after containment and object verification
    // both ran, and only if the result is actually clean -- not inside the copy's
    // transaction, and not unconditionally. `manifest_already_migrated` is what the
    // CLI's unmigrated-manifest nudge trusts to mean "provably safe to stop
    // reminding the caller about this manifest.db"; a migration that copied
    // *something* but left rows missing or objects unresolvable must not durably
    // claim to be migrated, or that nudge -- the safety net for data that may exist
    // nowhere else -- would be permanently and silently suppressed.
    if report.is_clean() {
        ledger.db.execute(
            "INSERT INTO manifest_migration (id, completed_at) VALUES (1, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET completed_at = excluded.completed_at",
            [],
        )?;
    }

    Ok(report)
}

/// Whether `<root>/ledger.db` already carries the manifest-migration marker, without
/// creating or modifying anything. Suitable for `--dry-run` and the `cclog archive`
/// unmigrated-manifest nudge, neither of which should have the side effect of
/// creating a fresh `ledger.db` just to check this -- `Ledger::open` would do exactly
/// that, since it creates the database if absent.
///
/// Returns `false` (never an error) if `ledger.db` does not exist yet, or exists but
/// predates the marker table -- both mean "not migrated", not "unknown".
pub fn manifest_already_migrated(root: &Path) -> Result<bool, LedgerError> {
    let db_path = root.join("ledger.db");
    if !db_path.exists() {
        return Ok(false);
    }
    let db = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let table_exists: i64 = db.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'manifest_migration'",
        [],
        |r| r.get(0),
    )?;
    if table_exists == 0 {
        return Ok(false);
    }
    let marked: i64 = db.query_row(
        "SELECT COUNT(*) FROM manifest_migration WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    Ok(marked > 0)
}

/// Every `object_id` in the new ledger's `source_object` table must resolve to a
/// file that can actually be opened for reading under the (unmoved) archive object
/// store. A row that fails this is exactly the failure mode the migration exists to
/// catch: a manifest entry for bytes that are no longer there.
fn verify_objects(ledger: &Ledger) -> Result<(u64, Vec<String>), LedgerError> {
    let mut stmt = ledger.db.prepare("SELECT object_id FROM source_object")?;
    let mut rows = stmt.query([])?;
    let mut verified = 0u64;
    let mut missing = Vec::new();
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        match ObjectId::parse(&raw) {
            Ok(id) => {
                let path = ledger.store.path(&id);
                match std::fs::File::open(&path) {
                    Ok(_) => verified += 1,
                    Err(_) => missing.push(raw),
                }
            }
            Err(_) => missing.push(raw),
        }
    }
    Ok((verified, missing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Outcome;
    use sha2::{Digest, Sha256};
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cclog-migrate-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn file_digest(path: &Path) -> String {
        let bytes = fs::read(path).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        format!("{:x}", hasher.finalize())
    }

    /// Build an M0-shaped archive: `<archive_root>/manifest.db` (old schema, old
    /// path) plus `<archive_root>/objects/..` -- exactly what `Archive::open`
    /// (pre-M1) would have produced. `archive_root` here is always
    /// `<root>/archive`, matching what `migrate_manifest_to_ledger` now requires.
    fn seed_old_archive(archive_root: &Path, rows: &[(&str, &str, &[u8], &str)]) {
        fs::create_dir_all(archive_root.join("objects")).unwrap();
        let db = Connection::open(archive_root.join("manifest.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE source_object (
               object_id   TEXT PRIMARY KEY,
               size_bytes  INTEGER NOT NULL,
               created_at  TEXT NOT NULL
             );
             CREATE TABLE source_snapshot (
               snapshot_id        INTEGER PRIMARY KEY AUTOINCREMENT,
               source_kind        TEXT NOT NULL,
               source_locator     TEXT NOT NULL,
               object_id          TEXT NOT NULL REFERENCES source_object(object_id),
               format_fingerprint TEXT,
               acquired_at        TEXT NOT NULL,
               UNIQUE(source_locator, object_id)
             );",
        )
        .unwrap();

        for (kind, locator, bytes, acquired_at) in rows {
            let digest = {
                let mut hasher = Sha256::new();
                hasher.update(bytes);
                format!("{:x}", hasher.finalize())
            };
            let object_id = format!("sha256:{digest}");
            db.execute(
                "INSERT INTO source_object (object_id, size_bytes, created_at) VALUES (?1, ?2, ?3)",
                params![object_id, bytes.len() as i64, acquired_at],
            )
            .unwrap();
            db.execute(
                "INSERT INTO source_snapshot (source_kind, source_locator, object_id, acquired_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![kind, locator, object_id, acquired_at],
            )
            .unwrap();

            // Publish the object where `Ledger`'s `ObjectStore` will look for it:
            // `<archive_root>/objects/<first-2-hex>/<rest-of-hex>`.
            let dir = archive_root.join("objects").join(&digest[..2]);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(&digest[2..]), bytes).unwrap();
        }
    }

    #[test]
    fn migrating_a_clean_archive_copies_every_row_and_verifies_every_object() {
        let root = tmp("clean");
        let archive_root = root.join("archive");
        seed_old_archive(
            &archive_root,
            &[
                (
                    "claude-code",
                    "projects/p/a.jsonl",
                    b"alpha bytes",
                    "2026-07-29T00:00:00Z",
                ),
                (
                    "codex",
                    "sessions/b.jsonl",
                    b"beta bytes",
                    "2026-07-29T01:00:00Z",
                ),
            ],
        );

        let report =
            migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Refuse).unwrap();

        assert_eq!(report.source_snapshot_count, 2);
        assert!(report.missing_snapshots.is_empty());
        assert_eq!(report.objects_verified, 2);
        assert!(report.objects_missing.is_empty());
        assert!(report.is_clean());

        // The new ledger actually has the rows, reachable through its own API.
        let ledger = Ledger::open(&root).unwrap();
        assert_eq!(
            ledger.snapshot_count("projects/p/a.jsonl").unwrap(),
            1,
            "migrated snapshot must be visible through Ledger, not just present in raw SQL"
        );
    }

    #[test]
    fn migrating_leaves_manifest_db_byte_for_byte_untouched() {
        let root = tmp("untouched");
        let archive_root = root.join("archive");
        seed_old_archive(
            &archive_root,
            &[(
                "claude-code",
                "p/a.jsonl",
                b"untouched bytes",
                "2026-07-29T00:00:00Z",
            )],
        );

        let manifest_path = archive_root.join("manifest.db");
        let digest_before = file_digest(&manifest_path);

        migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Refuse).unwrap();

        let digest_after = file_digest(&manifest_path);
        assert_eq!(
            digest_before, digest_after,
            "manifest.db must not be modified by migration -- it may be the only \
             surviving copy of data a vendor has already deleted"
        );
    }

    #[test]
    fn a_manifest_row_whose_object_was_deleted_is_reported_missing_not_silently_dropped() {
        let root = tmp("missing-object");
        let archive_root = root.join("archive");
        seed_old_archive(
            &archive_root,
            &[(
                "claude-code",
                "p/a.jsonl",
                b"will be deleted",
                "2026-07-29T00:00:00Z",
            )],
        );

        // Simulate bytes that vanished from the archive object store (e.g. disk
        // corruption, accidental deletion) while the manifest row describing them
        // survives -- exactly what this verification step exists to catch.
        let digest = {
            let mut hasher = Sha256::new();
            hasher.update(b"will be deleted");
            format!("{:x}", hasher.finalize())
        };
        let object_path = archive_root
            .join("objects")
            .join(&digest[..2])
            .join(&digest[2..]);
        fs::remove_file(&object_path).unwrap();

        let report =
            migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Refuse).unwrap();

        assert_eq!(report.source_snapshot_count, 1);
        assert!(
            report.missing_snapshots.is_empty(),
            "the snapshot row itself did land -- only the object bytes are gone"
        );
        assert_eq!(report.objects_verified, 0);
        assert_eq!(report.objects_missing.len(), 1);
        assert!(!report.is_clean());
    }

    #[test]
    fn running_the_migration_twice_is_idempotent_when_the_caller_acknowledges_the_second_run() {
        let root = tmp("rerun");
        let archive_root = root.join("archive");
        seed_old_archive(
            &archive_root,
            &[(
                "claude-code",
                "p/a.jsonl",
                b"idempotent bytes",
                "2026-07-29T00:00:00Z",
            )],
        );

        let first = migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Refuse).unwrap();
        // The first run left the destination non-empty, so a second run must say so
        // explicitly -- re-running the migration is not an ordinary, walk-up-and-run
        // operation now that "the destination already has rows" no longer implies
        // "and therefore this must be a no-op re-run" (it might instead mean live
        // `cclog archive` rows are sitting where the migration's explicit ids want
        // to land).
        let refused = migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Refuse);
        assert!(
            matches!(refused, Err(MigrateError::NonEmptyDestination { .. })),
            "a second run must not silently proceed against a non-empty destination: {refused:?}"
        );

        let second =
            migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Proceed).unwrap();
        assert_eq!(
            first, second,
            "an acknowledged second run against the same manifest.db must not duplicate \
             rows or report anything new as missing"
        );
        assert!(second.is_clean());
    }

    #[test]
    fn migrating_into_a_destination_with_a_colliding_snapshot_id_reports_the_dropped_row_instead_of_a_false_clean()
     {
        // Reproduces the critical failure mode directly: `ledger.db` already has a
        // live `cclog archive` row occupying `snapshot_id = 1` (exactly what
        // happens if a user runs `cclog archive` before `cclog migrate`, against
        // advice) before the source manifest -- whose first row also carries
        // `snapshot_id = 1` -- is migrated in. `INSERT OR IGNORE` silently drops
        // the source's first row rather than erroring, and the two tables' row
        // counts still happen to match afterward (one live row occupies the slot
        // the dropped migrated row would have used), which is exactly the
        // "arithmetic luck" the old count-based `is_clean()` was vulnerable to.
        // The new containment check must catch it regardless.
        let root = tmp("colliding-id");
        let archive_root = root.join("archive");
        seed_old_archive(
            &archive_root,
            &[
                (
                    "claude-code",
                    "p/first.jsonl",
                    b"source row one -- wants snapshot_id 1",
                    "2026-07-29T00:00:00Z",
                ),
                (
                    "claude-code",
                    "p/second.jsonl",
                    b"source row two -- wants snapshot_id 2",
                    "2026-07-29T00:00:01Z",
                ),
                (
                    "claude-code",
                    "p/third.jsonl",
                    b"source row three -- wants snapshot_id 3",
                    "2026-07-29T00:00:02Z",
                ),
            ],
        );

        // A live archive row, written before migration ever ran, takes snapshot_id 1
        // in the destination ledger.
        {
            let mut live = Ledger::open(&root).unwrap();
            live.archive_file(
                "claude-code",
                "p/already-live.jsonl",
                b"a locator that was archived live, before migration",
                "2026-07-29T00:00:00Z",
                None,
            )
            .unwrap();
        }

        let report =
            migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Proceed).unwrap();

        // The old count-based check would have seen this as clean: the destination
        // ends up with 3 source_snapshot rows (1 live + 2 successfully migrated),
        // which happens to equal the source's row count of 3.
        let ledger = Ledger::open(&root).unwrap();
        let destination_snapshot_count: i64 = ledger
            .db
            .query_row("SELECT COUNT(*) FROM source_snapshot", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            destination_snapshot_count, 3,
            "sanity check on the reproduction: row counts must coincidentally match \
             for this to demonstrate the bug the old check missed"
        );

        assert!(
            !report.is_clean(),
            "a migration that silently dropped a source row to a snapshot_id \
             collision must not report clean, even though the destination's total \
             row count happens to match the source's"
        );
        assert_eq!(
            report.missing_snapshots,
            vec![MissingSnapshot {
                source_locator: "p/first.jsonl".to_string(),
                object_id: {
                    let mut hasher = Sha256::new();
                    hasher.update(b"source row one -- wants snapshot_id 1");
                    format!("sha256:{:x}", hasher.finalize())
                },
            }],
            "the specific row that lost the snapshot_id collision must be named"
        );
    }

    #[test]
    fn migrating_into_a_non_empty_destination_is_refused_by_default() {
        let root = tmp("refuse-nonempty");
        let archive_root = root.join("archive");
        seed_old_archive(
            &archive_root,
            &[(
                "claude-code",
                "p/a.jsonl",
                b"source bytes",
                "2026-07-29T00:00:00Z",
            )],
        );

        // Any pre-existing row -- not necessarily a colliding one -- must trigger
        // the refusal; the whole point is that a migration into a populated ledger
        // is not a normal operation, not that we can prove in advance whether this
        // particular non-empty destination would actually collide.
        {
            let mut live = Ledger::open(&root).unwrap();
            live.archive_file(
                "claude-code",
                "p/unrelated-live-row.jsonl",
                b"unrelated live bytes",
                "2026-07-29T00:00:00Z",
                None,
            )
            .unwrap();
        }

        let result = migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Refuse);
        match result {
            Err(MigrateError::NonEmptyDestination {
                existing_object_rows,
                existing_snapshot_rows,
            }) => {
                assert_eq!(existing_object_rows, 1);
                assert_eq!(existing_snapshot_rows, 1);
            }
            other => panic!("expected MigrateError::NonEmptyDestination, got {other:?}"),
        }

        // And nothing from the source manifest was copied in as a side effect of the
        // refused attempt.
        let ledger = Ledger::open(&root).unwrap();
        assert_eq!(
            ledger.snapshot_count("p/a.jsonl").unwrap(),
            0,
            "a refused migration must not have copied any source rows"
        );
    }

    #[test]
    fn a_snapshot_ingested_after_migration_gets_a_fresh_non_colliding_id() {
        // The migration copies `snapshot_id` values explicitly rather than letting
        // `AUTOINCREMENT` regenerate them. If that left `sqlite_sequence` behind
        // (rather than caught up to the migrated high-water mark), the next
        // `archive_file` call on the migrated ledger could reuse an id that already
        // names a migrated snapshot.
        let root = tmp("post-migration-insert");
        let archive_root = root.join("archive");
        seed_old_archive(
            &archive_root,
            &[
                ("claude-code", "p/a.jsonl", b"one", "2026-07-29T00:00:00Z"),
                ("claude-code", "p/b.jsonl", b"two", "2026-07-29T00:00:01Z"),
                ("claude-code", "p/c.jsonl", b"three", "2026-07-29T00:00:02Z"),
            ],
        );
        migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Refuse).unwrap();

        let mut ledger = Ledger::open(&root).unwrap();
        let existing_ids: Vec<i64> = ledger
            .find_snapshots(&Default::default())
            .unwrap()
            .iter()
            .map(|s| s.snapshot_id)
            .collect();

        let Outcome::Created(_) = ledger
            .archive_file(
                "claude-code",
                "p/new-after-migration.jsonl",
                b"fresh",
                "2026-07-29T01:00:00Z",
                None,
            )
            .unwrap()
        else {
            panic!("expected a new snapshot to be created");
        };
        let new_snapshot = ledger
            .latest_snapshot("p/new-after-migration.jsonl")
            .unwrap()
            .unwrap();
        assert!(
            !existing_ids.contains(&new_snapshot.snapshot_id),
            "post-migration insert must not reuse a migrated snapshot_id: {} already used by {:?}",
            new_snapshot.snapshot_id,
            existing_ids
        );
    }

    #[test]
    fn migrating_from_a_customized_archive_root_is_rejected() {
        let root = tmp("custom-archive-root");
        let custom_archive_root = std::env::temp_dir().join(format!(
            "cclog-migrate-{}-custom-archive-elsewhere",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&custom_archive_root);
        seed_old_archive(
            &custom_archive_root,
            &[(
                "claude-code",
                "p/a.jsonl",
                b"bytes under a customized archive root",
                "2026-07-29T00:00:00Z",
            )],
        );

        let result = migrate_manifest_to_ledger(&custom_archive_root, &root, ExistingRows::Refuse);
        match result {
            Err(MigrateError::UnsupportedArchiveRoot { expected, actual }) => {
                assert_eq!(expected, root.join("archive"));
                assert_eq!(actual, custom_archive_root);
            }
            other => panic!("expected MigrateError::UnsupportedArchiveRoot, got {other:?}"),
        }
        assert!(
            !root.join("ledger.db").exists(),
            "rejecting a customized archive root must happen before anything is created"
        );

        fs::remove_dir_all(&custom_archive_root).unwrap();
    }

    #[test]
    fn manifest_already_migrated_is_false_before_migrating_and_true_after() {
        let root = tmp("marker");
        let archive_root = root.join("archive");
        seed_old_archive(
            &archive_root,
            &[(
                "claude-code",
                "p/a.jsonl",
                b"marker bytes",
                "2026-07-29T00:00:00Z",
            )],
        );

        assert!(
            !manifest_already_migrated(&root).unwrap(),
            "nothing has been migrated yet, and ledger.db does not even exist"
        );

        migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Refuse).unwrap();

        assert!(
            manifest_already_migrated(&root).unwrap(),
            "a completed migration must set the durable marker"
        );
    }

    #[test]
    fn a_non_clean_migration_does_not_set_the_manifest_already_migrated_marker() {
        // The marker must not outrun what was actually verified: a migration whose
        // report is not clean (here, an object's bytes vanished from the archive
        // store between seeding and migrating -- see
        // `a_manifest_row_whose_object_was_deleted_is_reported_missing_not_silently_dropped`)
        // must leave `manifest_already_migrated` false. Setting it anyway would
        // permanently suppress the CLI's unmigrated-manifest nudge -- the safety net
        // for exactly the data this migration failed to verify -- even though the
        // manifest's contents are not provably present in the ledger.
        let root = tmp("marker-not-set-on-dirty");
        let archive_root = root.join("archive");
        seed_old_archive(
            &archive_root,
            &[(
                "claude-code",
                "p/a.jsonl",
                b"will be deleted",
                "2026-07-29T00:00:00Z",
            )],
        );
        let digest = {
            let mut hasher = Sha256::new();
            hasher.update(b"will be deleted");
            format!("{:x}", hasher.finalize())
        };
        let object_path = archive_root
            .join("objects")
            .join(&digest[..2])
            .join(&digest[2..]);
        fs::remove_file(&object_path).unwrap();

        let report =
            migrate_manifest_to_ledger(&archive_root, &root, ExistingRows::Refuse).unwrap();
        assert!(
            !report.is_clean(),
            "sanity check: this migration must end non-clean for the test to prove anything"
        );

        assert!(
            !manifest_already_migrated(&root).unwrap(),
            "a migration that did not verify clean must not durably claim to be migrated"
        );
    }
}
