//! Ledger: which source file produced which archived object, and when.
//!
//! The object store is content-addressed and therefore says nothing about provenance.
//! The ledger supplies it, and (from M1 on) is the same SQLite database that observations
//! and checkpoints are committed into -- see the crash-invariant note on
//! [`Ledger::open`] for why that had to be one database rather than two.
//!
//! Historically (M0) this module was `manifest.rs` and opened its database at
//! `<archive_root>/manifest.db`, colocated with the object store it described. M1
//! relocates the database to `<cclog_root>/ledger.db`, a sibling of `<cclog_root>/archive/`
//! rather than a child of it, because the ledger now also holds tables (`observation`,
//! `checkpoint`) that have nothing to do with the archive specifically. The archived
//! *bytes* do not move: `Ledger::open(root)` still points its [`ObjectStore`] at
//! `<root>/archive`, the same path the old `Archive::open(<archive_root>)` used when
//! `<archive_root>` was passed as `<root>/archive` directly. See
//! `crates/cclogger-archive/src/migrate.rs` for the one-time, non-destructive migration
//! from an M0 `manifest.db` to this layout.
//!
//! ## Observation storage: JSON body + promoted columns, not a wide typed table
//!
//! The rule this follows: known query fields become typed columns, and
//! everything else stays as an opaque extension rather than
//! being decomposed into a "convenient event bucket". Two things make the *full
//! typed-columns* alternative (one SQL column per `Observation` field) actively
//! worse here, not just unnecessary:
//!
//! - `data` is a discriminated union keyed by `event_type` (12 shapes today, see
//!   `schema/cclog.observation.v0.schema.json`'s `allOf`), so decomposing it into
//!   columns means either one column per shape's fields (mostly NULL on every row)
//!   or re-deriving the discriminant logic in SQL. Neither is simpler than storing
//!   the JSON `serde_json::Value` already carries.
//! - Design doc §8's replay rule ("同じ retained snapshot、importer version、config
//!   から observation count と current projection が変わらない") means the stored form
//!   must round-trip losslessly. `cclogger-domain`'s `Observation` already has that
//!   property by construction (`cclogger-domain/tests/round_trip.rs` pins it against
//!   every fixture); storing `serde_json::to_string(&observation)` verbatim inherits
//!   the guarantee for free. A hand-maintained set of ~20 typed columns would have to
//!   re-earn it, and silently drift out of sync with `Observation` as new optional
//!   `cclog*` fields are added.
//!
//! So `body` holds the complete canonical observation, and only the fields the next
//! milestone's queries actually need indexed -- `occurred_at` (time range),
//! `workspace_ref` (workspace), `event_type`, and `source_kind` (checkpoint scoping
//! rides along on the same column) -- are promoted to real columns. `id` and
//! `cclogdedupekey` are promoted too, but for identity/uniqueness rather than
//! querying: see the schema comment on `CREATE TABLE observation` for why those two
//! constraints target different columns.

use crate::object::{ObjectId, ObjectStore, mkdir_owner_only};
use cclogger_domain::Observation;
use rusqlite::{Connection, params};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
pub enum LedgerError {
    Io(std::io::Error),
    Db(rusqlite::Error),
    /// An `Observation` failed to serialize to JSON on the way into `body`, or a
    /// stored `body` failed to deserialize back on the way out. `Observation`'s
    /// `data` field is a `serde_json::Value`, which can never hold a non-finite
    /// float or other non-representable value, so this should be unreachable in
    /// practice; it exists so a future change that makes it reachable fails loudly
    /// instead of panicking.
    Json(serde_json::Error),
    /// `ledger.db`'s `PRAGMA user_version` is higher than this build of cclog
    /// understands. A ledger written by a newer cclog can hold schema this build has
    /// never heard of; opening and writing to it anyway risks corrupting data this
    /// build cannot interpret, so `Ledger::open` refuses rather than guessing.
    SchemaTooNew {
        found: i64,
        understood: i64,
    },
    /// Bringing an on-disk table below `SCHEMA_VERSION` up to the current shape (see
    /// `Ledger::open`'s upgrade step) would require discarding rows this build
    /// cannot losslessly carry across -- e.g. a NOT NULL column with no value this
    /// build could correctly backfill. Refused rather than silently dropped.
    SchemaUpgradeBlocked {
        table: &'static str,
        reason: String,
    },
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Io(e) => write!(f, "ledger io: {e}"),
            LedgerError::Db(e) => write!(f, "ledger db: {e}"),
            LedgerError::Json(e) => write!(f, "ledger observation (de)serialization: {e}"),
            LedgerError::SchemaTooNew { found, understood } => write!(
                f,
                "ledger.db schema version {found} is newer than this build of cclog \
                 understands (up to {understood}); refusing to open it -- upgrade cclog \
                 before using this ledger again"
            ),
            LedgerError::SchemaUpgradeBlocked { table, reason } => write!(
                f,
                "ledger.db's `{table}` table cannot be upgraded automatically: {reason}"
            ),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<std::io::Error> for LedgerError {
    fn from(e: std::io::Error) -> Self {
        LedgerError::Io(e)
    }
}

impl From<rusqlite::Error> for LedgerError {
    fn from(e: rusqlite::Error) -> Self {
        LedgerError::Db(e)
    }
}

impl From<serde_json::Error> for LedgerError {
    fn from(e: serde_json::Error) -> Self {
        LedgerError::Json(e)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// This locator gained a new snapshot.
    Created(ObjectId),
    /// This exact (locator, content) pair was already archived.
    AlreadyPresent(ObjectId),
}

pub struct Ledger {
    pub(crate) store: ObjectStore,
    pub(crate) db: Connection,
}

/// `PRAGMA user_version` this build writes on every fresh or upgraded `ledger.db`,
/// and refuses to open anything newer than -- see [`Ledger::open`]. Bump this
/// whenever `SCHEMA` changes in a way that needs the version bumped for a future
/// build to detect (e.g. a column addition an older build's blanket
/// `CREATE TABLE IF NOT EXISTS` would otherwise silently skip applying to an
/// existing database).
const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS source_object (
  object_id   TEXT PRIMARY KEY,
  size_bytes  INTEGER NOT NULL,
  created_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS source_snapshot (
  snapshot_id        INTEGER PRIMARY KEY AUTOINCREMENT,
  source_kind        TEXT NOT NULL,
  source_locator     TEXT NOT NULL,
  object_id          TEXT NOT NULL REFERENCES source_object(object_id),
  -- M0 archives bytes only -- it does not parse them, so it cannot derive a
  -- format_fingerprint (vendor + format_fingerprint + importer_version is the adapter
  -- dispatch key; see design doc section 9). Every row written by `cclogger archive`
  -- today has NULL here. NULL means not-yet-derived, not unknown-or-unsupported
  -- format: derivation is an M1 ingest-time responsibility, deferred deliberately
  -- rather than left unset by omission.
  format_fingerprint TEXT,
  acquired_at        TEXT NOT NULL,
  UNIQUE(source_locator, object_id)
);
CREATE INDEX IF NOT EXISTS source_snapshot_locator ON source_snapshot(source_locator);

-- `id` (the observation_id, a UUIDv7 minted fresh by the runtime at finalize time --
-- see cclogger-domain's ObservationDraft::finalize) is the row's identity, but it is
-- NOT the dedupe key: re-deriving the same logical observation on a re-run mints a
-- new `id` every time, so uniqueness has to be enforced on `cclogdedupekey` instead.
-- `id` stays a PRIMARY KEY anyway so a collision there (which should never happen --
-- ids are effectively random) surfaces as a genuine constraint error rather than
-- silently overwriting a row, which is why the insert below targets its `ON
-- CONFLICT` clause at `cclogdedupekey` specifically rather than using a blanket
-- `INSERT OR IGNORE` that would swallow an `id` collision too.
--
-- Everything past `body` is a query-indexable projection of fields the next
-- milestone's queries need (time range, workspace, event type, snapshot
-- provenance; source_kind comes along for free and is needed by checkpoint
-- scoping). `body` is the complete canonical Observation, serialized exactly as it
-- round-trips through cclogger-domain/serde -- see the module doc comment for why the
-- full JSON form, not a wider set of typed columns, is the source of truth here.
--
-- `occurred_at` is stored normalized to UTC with a literal `Z` suffix (see
-- `crate::occurred_at::normalize`), not verbatim from `Observation.time` -- whose
-- schema constraint (`format: date-time`) permits any offset and any
-- fractional-second precision. This column exists solely for the lexicographic
-- range queries design doc §7/§8 need; an adapter emitting `+09:00` would otherwise
-- sort before a UTC timestamp hours earlier, silently breaking every range filter
-- built on it. `body` keeps the original, unnormalized value, so the round-trip
-- property is unaffected.
--
-- `snapshot_id` is the `source_snapshot` row `Ledger::ingest` resolved this
-- observation's snapshot to (design doc §7's retention/delete story and §8's replay
-- both need to know which observations came from which snapshot). It is a real
-- FOREIGN KEY so an observation can never point at a snapshot this ledger does not
-- have.
CREATE TABLE IF NOT EXISTS observation (
  id             TEXT PRIMARY KEY,
  cclogdedupekey TEXT NOT NULL UNIQUE,
  source_kind    TEXT NOT NULL,
  event_type     TEXT NOT NULL,
  occurred_at    TEXT NOT NULL,
  workspace_ref  TEXT,
  repository_ref TEXT,
  snapshot_id    INTEGER NOT NULL REFERENCES source_snapshot(snapshot_id),
  body           TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS observation_occurred_at ON observation(occurred_at);
CREATE INDEX IF NOT EXISTS observation_workspace_ref ON observation(workspace_ref);
CREATE INDEX IF NOT EXISTS observation_repository_ref ON observation(repository_ref);
CREATE INDEX IF NOT EXISTS observation_event_type ON observation(event_type);
CREATE INDEX IF NOT EXISTS observation_source_kind ON observation(source_kind);
CREATE INDEX IF NOT EXISTS observation_snapshot_id ON observation(snapshot_id);

-- Maps an opaque identity ref (e.g. `rep_4e1a...`, the pseudonym stored on
-- `observation.repository_ref` / `.workspace_ref`) to the human-readable, but still
-- *normalized*, identity it stands for (`github.com/acme/api`) -- never a cwd, which
-- contains the username; the ledger stays metadata-only. Without this, a
-- report can group observations by ref but cannot display anything for the group
-- without re-deriving identities from the archived transcripts. `kind` is
-- `\"repository\"` or `\"workspace\"`. `first_seen` is set once, on first
-- registration (see `Ledger::register_identity`'s `INSERT OR IGNORE`), so a
-- re-import does not overwrite it with a later import's timestamp.
CREATE TABLE IF NOT EXISTS workspace_identity (
  ref        TEXT PRIMARY KEY,
  kind       TEXT NOT NULL,
  display    TEXT NOT NULL,
  first_seen TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS workspace_identity_kind ON workspace_identity(kind, display);

-- One row per (source_kind, source_locator): how far an importer has gotten reading
-- that source, so a re-run reads only what is new. `snapshot_id` names the last
-- source_snapshot the checkpoint has fully accounted for; it is a real FOREIGN KEY
-- (not just documentation) so that a checkpoint can never point at a snapshot this
-- ledger does not actually have. `cursor` is an opaque, importer-defined position
-- within that snapshot (e.g. a record offset or stable id) -- this ledger stores and
-- returns it verbatim and assigns it no meaning of its own.
CREATE TABLE IF NOT EXISTS checkpoint (
  source_kind    TEXT NOT NULL,
  source_locator TEXT NOT NULL,
  snapshot_id    INTEGER NOT NULL REFERENCES source_snapshot(snapshot_id),
  cursor         TEXT,
  updated_at     TEXT NOT NULL,
  PRIMARY KEY (source_kind, source_locator)
);

-- Records that the one-time manifest.db -> ledger.db migration (`crate::migrate`)
-- has run against this ledger, independent of whatever rows happen to be sitting in
-- source_object/source_snapshot at any given moment -- ordinary `cclogger archive` use
-- writes to those same tables, so their row counts alone cannot say whether a
-- migration happened. Exists so the CLI's \"you have an unmigrated manifest.db\"
-- nudge (crates/cclogger-cli/src/main.rs) can key off something durable instead of
-- \"ledger.db doesn't exist yet\", which is only ever true once.
CREATE TABLE IF NOT EXISTS manifest_migration (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  completed_at TEXT NOT NULL
);
";

/// Reconcile an on-disk schema below [`SCHEMA_VERSION`] with the current shape,
/// *before* `SCHEMA`'s `CREATE TABLE IF NOT EXISTS` batch runs. That batch is a
/// no-op against a table that already exists -- SQLite does not reconcile column
/// differences on its own -- so a version bump alone (stamping `user_version`
/// without actually changing anything) would falsely declare a stale table
/// current. This is where the actual reconciling happens; [`Ledger::open`] must not
/// stamp the version forward unless this returns `Ok`.
///
/// A table that does not exist yet needs no migration: `SCHEMA`'s own
/// `CREATE TABLE IF NOT EXISTS` builds it in the current shape a moment later. Only
/// a table that already exists *and* is missing a column the current shape expects
/// needs work here.
fn upgrade_schema_to_current(db: &Connection, on_disk_version: i64) -> Result<(), LedgerError> {
    if on_disk_version >= SCHEMA_VERSION {
        return Ok(());
    }

    // Version 0 -> 1: `observation` gained `snapshot_id INTEGER NOT NULL REFERENCES
    // source_snapshot(snapshot_id)`. `ALTER TABLE ... ADD COLUMN` cannot add a
    // `NOT NULL` column with no default to a table SQLite will let existing rows
    // violate, so there is no value this build could backfill for a populated
    // table. In practice `observation` did not exist before this branch, so it is
    // empty everywhere this has actually run; recreating an *empty* table in the
    // current shape loses nothing. A non-empty legacy table is refused instead of
    // silently dropping its rows to make the schema batch succeed.
    if table_exists(db, "observation")? && !column_exists(db, "observation", "snapshot_id")? {
        let row_count: i64 = db.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0))?;
        if row_count > 0 {
            return Err(LedgerError::SchemaUpgradeBlocked {
                table: "observation",
                reason: format!(
                    "{row_count} existing row(s) predate the snapshot_id column added in \
                     schema version 1; adding a NOT NULL column to a populated table cannot \
                     be done automatically without inventing data, so this ledger cannot be \
                     opened until it is repaired or rebuilt by hand"
                ),
            });
        }
        db.execute_batch("DROP TABLE observation;")?;
    }

    // Version 1 -> 2: `observation` gained a promoted, indexed `repository_ref`.
    // Nullable, so unlike `snapshot_id` above this can be added to a populated table:
    // rows imported before repository identity existed truthfully have none.
    if table_exists(db, "observation")? && !column_exists(db, "observation", "repository_ref")? {
        db.execute_batch("ALTER TABLE observation ADD COLUMN repository_ref TEXT;")?;
    }

    Ok(())
}

/// The promoted identity columns of one observation, as
/// `(workspace_ref, repository_ref)` -- what a report groups on, read back without
/// parsing the JSON `body`. Both are nullable and a `None` is meaningful: the record's
/// cwd resolved to nothing, or it carried none and its session never named one.
pub type ObservationIdentity = (Option<String>, Option<String>);

/// Whether opening the ledger at `root` would upgrade its schema in place.
///
/// [`Ledger::open`] reconciles an out-of-date `ledger.db` and stamps `user_version`
/// forward as a side effect of opening. That is a **write**, and not one the user can
/// undo: once a ledger is stamped at a version an older cclog does not understand,
/// that build refuses it outright ([`LedgerError::SchemaTooNew`]). A caller that has
/// promised to write nothing -- `cclog import --dry-run` -- therefore has to ask
/// before opening, which is what this is for.
///
/// `false` when there is no `ledger.db` at all: there is nothing to upgrade, and the
/// caller has its own answer for that case. Reading `PRAGMA user_version` is the only
/// thing done here -- no journal mode is set, no schema batch runs, nothing is
/// created.
pub fn needs_schema_upgrade(root: &Path) -> Result<bool, LedgerError> {
    let path = root.join("ledger.db");
    if !path.exists() {
        return Ok(false);
    }
    let db = Connection::open(&path)?;
    let on_disk_version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(on_disk_version < SCHEMA_VERSION)
}

fn table_exists(db: &Connection, name: &str) -> Result<bool, LedgerError> {
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Whether `table` (always a fixed, code-controlled name -- never external input)
/// has a column named `column`, via `PRAGMA table_info`.
fn column_exists(db: &Connection, table: &str, column: &str) -> Result<bool, LedgerError> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let existing_column: String = row.get(1)?; // table_info's column 1 is `name`
        if existing_column == column {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Ledger {
    /// Open (creating if needed) the ledger rooted at `root` -- the cclog home
    /// directory (e.g. `~/.cclog`), not the archive directory. Archived bytes live at
    /// `<root>/archive` (an [`ObjectStore`]); the SQLite database lives at
    /// `<root>/ledger.db`.
    ///
    /// Both live under the same `root` deliberately: design doc §7 crash invariant 2
    /// requires the source manifest, observation, checkpoint, and projection-job
    /// commits to share one SQLite transaction. M0 shipped the manifest as its own
    /// database (`<archive_root>/manifest.db`) in WAL mode; SQLite does not guarantee
    /// atomic commit across `ATTACH`ed databases under WAL (there is no master
    /// journal to fall back on), so that layout could never satisfy invariant 2. This
    /// type -- one connection, one file, four tables (`source_object`,
    /// `source_snapshot`, `observation`, `checkpoint`) -- is the fix: a single
    /// `Connection::transaction()` really does commit all of them together or none of
    /// them. See [`Ledger::ingest`] for where that matters.
    ///
    /// Refuses to open a `ledger.db` whose `PRAGMA user_version` is higher than
    /// [`SCHEMA_VERSION`] (see [`LedgerError::SchemaTooNew`]): a ledger written by a
    /// newer cclog can hold schema this build has never heard of, and proceeding
    /// anyway risks writing data this build cannot correctly interpret. A version
    /// lower than current (including the implicit `0` SQLite reports for a database
    /// that predates this check entirely) is upgraded in place by
    /// [`upgrade_schema_to_current`] *before* `SCHEMA`'s `CREATE TABLE IF NOT EXISTS`
    /// batch runs -- that batch is a no-op against a table that already exists (SQLite
    /// does not reconcile column differences on its own), so the actual reconciling
    /// has to happen first. The on-disk version is stamped forward only after both
    /// the upgrade and the schema batch have succeeded, never before.
    pub fn open(root: &Path) -> Result<Self, LedgerError> {
        mkdir_owner_only(root)?;
        let store = ObjectStore::open(&root.join("archive"))?;
        let db = Connection::open(root.join("ledger.db"))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        // rusqlite already sets a 5s busy_timeout on every new connection by
        // default, so this is a no-op in practice today; we set it explicitly
        // anyway so that "concurrent invocations wait briefly for the write lock
        // instead of failing immediately with SQLITE_BUSY" is a documented,
        // deliberate choice here rather than something we happen to inherit.
        db.busy_timeout(Duration::from_secs(5))?;

        let on_disk_version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if on_disk_version > SCHEMA_VERSION {
            return Err(LedgerError::SchemaTooNew {
                found: on_disk_version,
                understood: SCHEMA_VERSION,
            });
        }

        upgrade_schema_to_current(&db, on_disk_version)?;

        db.execute_batch(SCHEMA)?;

        if on_disk_version < SCHEMA_VERSION {
            db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        // SQLite creates ledger.db (and, under WAL, its -wal/-shm companions) at
        // the umask default (0644); the ledger lists every archived source file and
        // (from M1) every observation and checkpoint, including rows sitting in the
        // WAL companions before a checkpoint, so force owner-only on each of these
        // that exists at this point. The owner-only root (0700, `mkdir_owner_only`
        // above) is the durable guarantee -- SQLite can recreate -wal/-shm at any
        // later checkpoint outside our control -- but we chmod explicitly here too,
        // rather than silently relying on the directory alone, so the intent is
        // auditable.
        for name in ["ledger.db", "ledger.db-wal", "ledger.db-shm"] {
            let path = root.join(name);
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(Self { store, db })
    }

    /// Archive `bytes` as the current snapshot of `locator`.
    ///
    /// The object is published to the filesystem *before* the manifest row is
    /// committed. A crash in between leaves an unreferenced object, which is
    /// recoverable; the reverse order would leave a manifest row pointing at nothing.
    pub fn archive_file(
        &mut self,
        kind: &str,
        locator: &str,
        bytes: &[u8],
        acquired_at: &str,
        fingerprint: Option<&str>,
    ) -> Result<Outcome, LedgerError> {
        let (id, _created_object) = self.store.put(bytes)?;

        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO source_object (object_id, size_bytes, created_at)
             VALUES (?1, ?2, ?3)",
            params![id.as_str(), bytes.len() as i64, acquired_at],
        )?;
        // `INSERT OR IGNORE` is itself the dedup check, not just the write: a bare
        // `SELECT` followed by a separate `INSERT` leaves a check-then-act window
        // between two connections (e.g. two `cclogger archive` processes) racing to
        // archive the same (locator, bytes) pair, where the loser's `INSERT` would
        // violate `UNIQUE(source_locator, object_id)` and surface as an opaque
        // database error instead of `Outcome::AlreadyPresent`. Deriving `Outcome`
        // from `Connection::changes()` right after this statement means the
        // statement that enforces uniqueness is the same one that decides the
        // answer, so there is no window to race.
        tx.execute(
            "INSERT OR IGNORE INTO source_snapshot
               (source_kind, source_locator, object_id, format_fingerprint, acquired_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind, locator, id.as_str(), fingerprint, acquired_at],
        )?;
        let created = tx.changes() > 0;
        tx.commit()?;
        Ok(if created {
            Outcome::Created(id)
        } else {
            Outcome::AlreadyPresent(id)
        })
    }

    pub fn snapshot_count(&self, locator: &str) -> Result<u32, LedgerError> {
        let n: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM source_snapshot WHERE source_locator = ?1",
            params![locator],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    pub fn read(&self, id: &ObjectId) -> Result<Vec<u8>, LedgerError> {
        Ok(self.store.read(id)?)
    }

    /// Whether an observation carrying `dedupe_key` is already in the ledger.
    ///
    /// This is the same question [`Ledger::ingest`]'s
    /// `ON CONFLICT(cclogdedupekey) DO NOTHING` answers at write time, exposed as a
    /// pure read for a caller that must not write anything (`cclog import --dry-run`).
    ///
    /// It answers only "was this key here before the run started". `ingest` also
    /// collapses repeats *within* one call -- the second row carrying a key
    /// deduplicates against the first, which this read cannot see because that first
    /// row was never written. A caller reproducing `ingest`'s created /
    /// already-present split without writing must therefore also track the keys it has
    /// already accounted for in this run; this method alone is not that split.
    pub fn observation_present(&self, dedupe_key: &str) -> Result<bool, LedgerError> {
        let n: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM observation WHERE cclogdedupekey = ?1",
            params![dedupe_key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// The promoted identity columns of the observation carrying `dedupe_key`, as
    /// `(workspace_ref, repository_ref)`, or `None` if no such observation exists.
    ///
    /// Both columns are nullable and a `NULL` is meaningful -- the record's cwd
    /// resolved to nothing, or it carried none and its session never named one -- so
    /// the outer `Option` (no such row) is deliberately distinct from an inner one.
    ///
    /// Reads the promoted columns rather than the JSON `body`: what a report groups on
    /// is what should be checked, and a ref that reached `body` but not the column
    /// would be invisible to every query the promotion exists to make cheap.
    pub fn observation_identity(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<ObservationIdentity>, LedgerError> {
        let mut stmt = self.db.prepare(
            "SELECT workspace_ref, repository_ref FROM observation WHERE cclogdedupekey = ?1",
        )?;
        let mut rows = stmt.query(params![dedupe_key])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
            None => Ok(None),
        }
    }

    /// Every observation whose `occurred_at` sits in `[from, to)`, oldest first.
    ///
    /// The bounds are compared **lexicographically** against the promoted
    /// `occurred_at` column, which holds a UTC `Z`-suffixed RFC 3339 string of
    /// whatever fractional precision the source had (see [`crate::occurred_at`]).
    /// That makes a bound written to full precision treacherous at the boundary: `.`
    /// (0x2E) sorts before `Z` (0x5A), so `< "…T15:00:00Z"` still admits
    /// `"…T15:00:00.000Z"`, the very instant the half-open range excludes. A caller
    /// that needs an exact boundary should therefore pass bounds truncated to the
    /// second and *without* the `Z` (`"2026-07-26T15:00:00"`), which can only be
    /// wider than the instant it names, never narrower, and apply the exact
    /// comparison itself after parsing. `cclogger-cli`'s report does both.
    ///
    /// Reads the promoted columns, plus the three JSON `body` fields a day view needs:
    /// the subject, the tool family, and the time basis. The rest of the body is left
    /// in the database -- a report groups and clocks on the promoted fields, and a day
    /// of the real corpus is ~7,500 rows whose bodies are not wanted. All three are
    /// pulled out in SQL rather than by returning `body` and parsing it, so the bytes
    /// that never leave the database stay in it, and `json_extract` yields `NULL` for a
    /// missing key either way.
    ///
    /// `time_basis` is read here rather than left to a caller because it decides
    /// whether a row may go on a clock at all -- a row whose time is the write time of
    /// a copy, or of an archive run, is not evidence of activity at that instant. A
    /// caller that could not see it would silently clock those.
    ///
    /// `subject` is read for the one question that cannot be answered from the promoted
    /// columns: which *session* a row belongs to. Two observations in the same
    /// repository, in the same minute, can come from two sessions running side by side,
    /// and a measurement that pairs records within a session (see `cclogger-cli`'s
    /// response time) has to be able to tell them apart.
    pub fn observations_between(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<ObservationRow>, LedgerError> {
        let mut stmt = self.db.prepare(
            "SELECT source_kind, event_type, occurred_at, repository_ref,
                    json_extract(body, '$.subject'),
                    json_extract(body, '$.data.tool_family'),
                    json_extract(body, '$.data.time_basis')
             FROM observation
             WHERE occurred_at >= ?1 AND occurred_at < ?2
             ORDER BY occurred_at ASC",
        )?;
        let rows = stmt.query_map(params![from, to], |row| {
            Ok(ObservationRow {
                source_kind: row.get(0)?,
                event_type: row.get(1)?,
                occurred_at: row.get(2)?,
                repository_ref: row.get(3)?,
                subject: row.get(4)?,
                tool_family: row.get(5)?,
                time_basis: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LedgerError::from)
    }

    /// The earliest and latest `occurred_at` this ledger holds, per source kind,
    /// ignoring the `event_type`s in `ignoring`.
    ///
    /// This is what a report can honestly claim to know about: outside it, a day has
    /// no observations because nothing was ever collected, which is a different
    /// statement from a day that was observed and held no work (design §13's "「0」と
    /// 「source が提供不能」「期間外」を別表示する"). A source with no observations
    /// at all is absent from the result rather than present with an empty range.
    ///
    /// `ignoring` exists for gap markers: a marker for a line that could not be parsed
    /// is dated to when `cclogger archive` ran, not to any observed activity, so counting
    /// one would stretch the claimed range to the acquisition clock.
    pub fn observed_range_by_source(
        &self,
        ignoring: &[&str],
    ) -> Result<Vec<SourceRange>, LedgerError> {
        let placeholders = (1..=ignoring.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let filter = if ignoring.is_empty() {
            String::new()
        } else {
            format!(" WHERE event_type NOT IN ({placeholders})")
        };
        let sql = format!(
            "SELECT source_kind, MIN(occurred_at), MAX(occurred_at) FROM observation{filter}
             GROUP BY source_kind ORDER BY source_kind"
        );
        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ignoring.iter()), |row| {
            Ok(SourceRange {
                source_kind: row.get(0)?,
                earliest: row.get(1)?,
                latest: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LedgerError::from)
    }

    /// The span of `occurred_at` over observations that came in through one **capture
    /// channel**, or `None` if that channel has contributed nothing.
    ///
    /// The channel is the `source_kind` of the *snapshot* an observation was derived
    /// from, not of the observation itself. The two differ for exactly the case this
    /// exists for: a Claude Code hook observation is `cclogsourcekind: "claude-code"`
    /// like every other, but its snapshot is the hook spool
    /// (`claude-code-hook`), so [`Ledger::observed_range_by_source`] folds it into the
    /// transcript channel's range and a report cannot say where hook capture *starts*.
    ///
    /// That start matters and cannot be recovered any other way: hooks record from the
    /// moment they are installed, so a ledger will normally hold years of transcript
    /// history and days of hook history, and a coverage line that showed one range for
    /// both would imply the newer channel covered the older period.
    ///
    /// `ignoring` drops event types by name, for the same reason
    /// [`Ledger::observed_range_by_source`] takes it: a gap marker is dated to when the
    /// import ran, so counting one would stretch the claimed range to a collection
    /// clock.
    pub fn capture_channel_range(
        &self,
        channel: &str,
        ignoring: &[&str],
    ) -> Result<Option<SourceRange>, LedgerError> {
        let placeholders = (2..=ignoring.len() + 1)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let filter = if ignoring.is_empty() {
            String::new()
        } else {
            format!(" AND o.event_type NOT IN ({placeholders})")
        };
        let sql = format!(
            "SELECT MIN(o.occurred_at), MAX(o.occurred_at)
             FROM observation o
             JOIN source_snapshot s ON s.snapshot_id = o.snapshot_id
             WHERE s.source_kind = ?1{filter}"
        );
        let mut binds: Vec<&str> = vec![channel];
        binds.extend(ignoring.iter().copied());
        let mut stmt = self.db.prepare(&sql)?;
        // `MIN`/`MAX` over no rows are SQL NULL, not an empty result set, so the row
        // always exists and "nothing yet" has to be read off the values.
        let range: (Option<String>, Option<String>) = stmt
            .query_row(rusqlite::params_from_iter(binds.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        Ok(match range {
            (Some(earliest), Some(latest)) => Some(SourceRange {
                source_kind: channel.to_string(),
                earliest,
                latest,
            }),
            _ => None,
        })
    }

    /// How many observations the ledger holds, narrowed to one `event_type` when
    /// `event_type` is `Some`.
    pub fn observation_count(&self, event_type: Option<&str>) -> Result<u64, LedgerError> {
        let n: i64 = match event_type {
            Some(t) => self.db.query_row(
                "SELECT COUNT(*) FROM observation WHERE event_type = ?1",
                params![t],
                |r| r.get(0),
            )?,
            None => self
                .db
                .query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0))?,
        };
        Ok(n as u64)
    }

    /// The newest snapshot recorded for `locator`, or `None` if it was never
    /// archived.
    ///
    /// "Newest" means highest `snapshot_id`, not latest `acquired_at`. `snapshot_id`
    /// is a SQLite `AUTOINCREMENT`, assigned once per row in strict insertion order,
    /// so it is a total order we control. `acquired_at` is a caller-supplied
    /// wall-clock string (see `archive_file`) -- nothing stops two calls from
    /// passing the same value, an out-of-order value, or a clock that jumped
    /// backwards -- so it cannot be trusted alone to say which snapshot is later.
    pub fn latest_snapshot(&self, locator: &str) -> Result<Option<Snapshot>, LedgerError> {
        let sql = format!(
            "SELECT {SNAPSHOT_COLUMNS} FROM source_snapshot
             WHERE source_locator = ?1 ORDER BY snapshot_id DESC LIMIT 1"
        );
        let mut stmt = self.db.prepare(&sql)?;
        let mut rows = stmt.query_map(params![locator], row_to_snapshot)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Every snapshot recorded for `locator`, oldest first (ascending `snapshot_id`;
    /// see [`Ledger::latest_snapshot`] for why that, not `acquired_at`, is the order
    /// of record). Lets a caller see a file's growth history. Empty, not an error,
    /// if `locator` was never archived.
    pub fn snapshots_for_locator(&self, locator: &str) -> Result<Vec<Snapshot>, LedgerError> {
        let sql = format!(
            "SELECT {SNAPSHOT_COLUMNS} FROM source_snapshot
             WHERE source_locator = ?1 ORDER BY snapshot_id ASC"
        );
        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt.query_map(params![locator], row_to_snapshot)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LedgerError::from)
    }

    /// Enumerate snapshots matching `filter`, oldest first (see
    /// [`Ledger::latest_snapshot`] for the ordering rationale). A default
    /// `SnapshotFilter` matches every row.
    pub fn find_snapshots(&self, filter: &SnapshotFilter) -> Result<Vec<Snapshot>, LedgerError> {
        let mut sql = format!("SELECT {SNAPSHOT_COLUMNS} FROM source_snapshot WHERE 1 = 1");
        let mut binds: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(kind) = filter.source_kind {
            sql.push_str(" AND source_kind = ?");
            binds.push(kind.to_string().into());
        }
        if let Some(prefix) = filter.locator_prefix {
            // `substr` + exact compare rather than `LIKE`, so a `%` or `_` in a
            // locator (an ordinary path byte) is matched literally instead of being
            // interpreted as a wildcard.
            //
            // SQLite's `substr(X, Y, Z)` counts *characters*, not bytes, when `X` is
            // TEXT -- so the length bound passed here must be a character count too.
            // `prefix.len()` is a byte count; for any prefix containing a multi-byte
            // UTF-8 character that asks `substr` for more characters than the prefix
            // actually holds, so the equality never matches and the locator becomes
            // silently unreachable by prefix (no error, no rows). See
            // `find_snapshots_by_locator_prefix_with_a_non_ascii_component` below.
            sql.push_str(" AND substr(source_locator, 1, ?) = ?");
            binds.push((prefix.chars().count() as i64).into());
            binds.push(prefix.to_string().into());
        }
        if let Some(from) = filter.acquired_from {
            sql.push_str(" AND acquired_at >= ?");
            binds.push(from.to_string().into());
        }
        if let Some(to) = filter.acquired_to {
            sql.push_str(" AND acquired_at < ?");
            binds.push(to.to_string().into());
        }
        sql.push_str(" ORDER BY snapshot_id ASC");

        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), row_to_snapshot)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LedgerError::from)
    }

    /// Atomically ingest one snapshot's worth of observations, plus a checkpoint
    /// advance, alongside that snapshot's own manifest row -- all in a single SQLite
    /// transaction. This is design doc §7 crash invariant 2 ("source manifest、
    /// observation、checkpoint、projection job は同じ SQLite transaction で commit
    /// する"): either every one of these lands, or none of them do. See
    /// `ingest_rolls_back_every_table_when_one_observation_hard_conflicts` below for
    /// the test that pins the all-or-nothing property against a real mid-batch
    /// failure, not just the happy path.
    ///
    /// `snapshot.bytes` is published to the object store exactly as in
    /// [`Ledger::archive_file`] (filesystem-atomic, tolerant of the object already
    /// existing -- design doc §7 crash invariant 1), and that publish happens
    /// *before* the SQL transaction opens: an `ingest` that fails after this point
    /// can be retried from scratch, because re-publishing the same bytes is a no-op.
    ///
    /// `observations` may be empty (a snapshot that produced no new observations
    /// still gets its manifest row and checkpoint advance). Each observation is
    /// inserted with the dedupe key as the sole conflict target -- see the
    /// `CREATE TABLE observation` schema comment for why `id` is deliberately left
    /// out of that `ON CONFLICT` clause -- and the returned
    /// [`IngestReport::observations`] tells the caller, in the same order, which of
    /// them were new.
    pub fn ingest(
        &mut self,
        snapshot: SnapshotRef,
        observations: &[Observation],
        checkpoint: CheckpointAdvance,
    ) -> Result<IngestReport, LedgerError> {
        let (object_id, _created_object) = self.store.put(snapshot.bytes)?;

        let tx = self.db.transaction()?;

        tx.execute(
            "INSERT OR IGNORE INTO source_object (object_id, size_bytes, created_at)
             VALUES (?1, ?2, ?3)",
            params![
                object_id.as_str(),
                snapshot.bytes.len() as i64,
                snapshot.acquired_at
            ],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO source_snapshot
               (source_kind, source_locator, object_id, format_fingerprint, acquired_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                snapshot.source_kind,
                snapshot.source_locator,
                object_id.as_str(),
                snapshot.format_fingerprint,
                snapshot.acquired_at,
            ],
        )?;
        let snapshot_created = tx.changes() > 0;

        // Resolved rather than trusted from the caller: the snapshot this ingest
        // call's checkpoint advance and observations belong to is always the one
        // just published above, identified by (locator, object_id) -- never a
        // caller-supplied id that could name a different snapshot by mistake.
        let snapshot_id: i64 = tx.query_row(
            "SELECT snapshot_id FROM source_snapshot WHERE source_locator = ?1 AND object_id = ?2",
            params![snapshot.source_locator, object_id.as_str()],
            |row| row.get(0),
        )?;

        let mut outcomes = Vec::with_capacity(observations.len());
        for obs in observations {
            let body = serde_json::to_string(obs)?;
            // `obs.time` is stored verbatim in `body` (the round-trip source of
            // truth) but normalized to UTC `Z` for the promoted `occurred_at`
            // column -- see the `CREATE TABLE observation` schema comment and
            // `crate::occurred_at`.
            let occurred_at = crate::occurred_at::normalize(&obs.time);
            tx.execute(
                "INSERT INTO observation
                   (id, cclogdedupekey, source_kind, event_type, occurred_at, workspace_ref,
                    repository_ref, snapshot_id, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(cclogdedupekey) DO NOTHING",
                params![
                    obs.id,
                    obs.cclogdedupekey,
                    obs.cclogsourcekind.slug(),
                    obs.event_type,
                    occurred_at,
                    obs.cclogworkspaceref,
                    obs.cclogrepositoryref,
                    snapshot_id,
                    body,
                ],
            )?;
            outcomes.push(if tx.changes() > 0 {
                ObservationOutcome::Created
            } else {
                ObservationOutcome::AlreadyPresent
            });
        }

        tx.execute(
            "INSERT INTO checkpoint (source_kind, source_locator, snapshot_id, cursor, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(source_kind, source_locator) DO UPDATE SET
               snapshot_id = excluded.snapshot_id,
               cursor = excluded.cursor,
               updated_at = excluded.updated_at
             WHERE excluded.snapshot_id >= checkpoint.snapshot_id",
            params![
                snapshot.source_kind,
                snapshot.source_locator,
                snapshot_id,
                checkpoint.cursor,
                checkpoint.updated_at,
            ],
        )?;

        tx.commit()?;

        Ok(IngestReport {
            snapshot: if snapshot_created {
                Outcome::Created(object_id)
            } else {
                Outcome::AlreadyPresent(object_id)
            },
            observations: outcomes,
        })
    }

    /// The current checkpoint for `(source_kind, source_locator)`, or `None` if this
    /// source has never been ingested.
    pub fn checkpoint(
        &self,
        source_kind: &str,
        source_locator: &str,
    ) -> Result<Option<Checkpoint>, LedgerError> {
        let mut stmt = self.db.prepare(
            "SELECT source_kind, source_locator, snapshot_id, cursor, updated_at
             FROM checkpoint WHERE source_kind = ?1 AND source_locator = ?2",
        )?;
        let mut rows = stmt.query_map(params![source_kind, source_locator], |row| {
            Ok(Checkpoint {
                source_kind: row.get(0)?,
                source_locator: row.get(1)?,
                snapshot_id: row.get(2)?,
                cursor: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Record what an opaque identity ref stands for, so a report can name it without
    /// re-deriving identities from the archive.
    ///
    /// `INSERT OR IGNORE`: a re-import re-observes every identity, and `first_seen`
    /// must keep meaning "first observed", not "last import".
    ///
    /// `display` is a *normalized* identity (`github.com/acme/api`), never a cwd -- a
    /// cwd contains the username, and the ledger stays metadata-only.
    pub fn register_identity(
        &self,
        opaque_ref: &str,
        kind: &str,
        display: &str,
        first_seen: &str,
    ) -> Result<(), LedgerError> {
        self.db.execute(
            "INSERT OR IGNORE INTO workspace_identity (ref, kind, display, first_seen)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![opaque_ref, kind, display, first_seen],
        )?;
        Ok(())
    }

    /// The human-readable identity behind a ref, or `None` if it was never registered.
    pub fn identity_display(&self, opaque_ref: &str) -> Result<Option<String>, LedgerError> {
        let mut stmt = self
            .db
            .prepare("SELECT display FROM workspace_identity WHERE ref = ?1")?;
        let mut rows = stmt.query([opaque_ref])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Every registered identity of one kind (`"repository"` or `"workspace"`), as
    /// `(ref, display)` ordered by display name.
    pub fn identities(&self, kind: &str) -> Result<Vec<(String, String)>, LedgerError> {
        let mut stmt = self.db.prepare(
            "SELECT ref, display FROM workspace_identity WHERE kind = ?1 ORDER BY display",
        )?;
        let rows = stmt.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    #[cfg(test)]
    pub(crate) fn connection_for_test(&self) -> &Connection {
        &self.db
    }
}

/// The raw bytes and manifest metadata for one snapshot, as passed to
/// [`Ledger::ingest`]. Mirrors [`Ledger::archive_file`]'s parameters; kept as its own
/// struct (rather than more positional arguments) because `ingest` already takes two
/// other multi-field arguments and positional args of the same types (`&str`, `&str`,
/// `Option<&str>`) invite mixing them up.
pub struct SnapshotRef<'a> {
    pub source_kind: &'a str,
    pub source_locator: &'a str,
    pub bytes: &'a [u8],
    pub acquired_at: &'a str,
    pub format_fingerprint: Option<&'a str>,
}

/// The reading-progress update to apply for the source named by a [`SnapshotRef`]
/// passed to the same [`Ledger::ingest`] call. Deliberately does not carry its own
/// `source_kind` / `source_locator` -- see `ingest`'s use of `snapshot.source_kind`
/// and `snapshot.source_locator` for the checkpoint row -- so there is no way to
/// advance a checkpoint for a locator other than the one just ingested.
pub struct CheckpointAdvance<'a> {
    /// Opaque, importer-defined position within the snapshot (e.g. a record offset
    /// or the last stable record id processed). This ledger stores and returns it
    /// verbatim; it carries no meaning here.
    pub cursor: Option<&'a str>,
    pub updated_at: &'a str,
}

/// Per-observation outcome from [`Ledger::ingest`], parallel to the `observations`
/// slice passed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationOutcome {
    /// This `cclogdedupekey` had never been seen before; the row was inserted.
    Created,
    /// An observation with this exact `cclogdedupekey` was already in the ledger.
    AlreadyPresent,
}

/// Everything [`Ledger::ingest`] did in one call.
#[derive(Debug, PartialEq, Eq)]
pub struct IngestReport {
    pub snapshot: Outcome,
    pub observations: Vec<ObservationOutcome>,
}

/// Reading progress for one `(source_kind, source_locator)`: how far an importer has
/// gotten, so a re-run reads only what is new. See [`Ledger::checkpoint`] and
/// [`Ledger::ingest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub source_kind: String,
    pub source_locator: String,
    /// The last `source_snapshot` this checkpoint has fully accounted for.
    pub snapshot_id: i64,
    pub cursor: Option<String>,
    pub updated_at: String,
}

/// One observation as a query reads it back: the promoted columns, plus the tool
/// family out of the JSON `body`. See [`Ledger::observations_between`].
///
/// `repository_ref` is the opaque pseudonym (`rep_…`), not a name --
/// [`Ledger::identities`] maps it to the normalized identity it stands for. `None`
/// is meaningful: the record's cwd resolved to nothing, or it carried none and its
/// session never named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRow {
    pub source_kind: String,
    pub event_type: String,
    /// UTC, `Z`-suffixed, at whatever fractional precision the source carried.
    pub occurred_at: String,
    pub repository_ref: Option<String>,
    /// The observation's hierarchical `subject` path (`session/ses_2XQ/turn/trn_91M`),
    /// verbatim. A consumer splits it on `/`; the leading `session/<ref>` segment is
    /// what identifies the session a record belongs to.
    ///
    /// `None` only if a stored body somehow carries no `subject` -- the schema makes it
    /// required, so this is an absence to notice rather than one to fill in.
    pub subject: Option<String>,
    /// The canonical tool family (`shell`, `edit`, `read`, `search`, `web`, `mcp`,
    /// `other`) an adapter recorded on a tool event, from `data.tool_family`.
    /// `None` on every event that is not a tool event -- and on a tool event whose
    /// adapter recorded no family, which is an absence to state rather than a family
    /// to guess.
    pub tool_family: Option<String>,
    /// Which clock `occurred_at` came from, out of `data.time_basis`: `occurred_at`
    /// when it is when the thing happened, `acquired_at` when it is only when the
    /// snapshot was collected, `copied_at` when the record was copied out of another
    /// transcript and carries the copy's write time.
    ///
    /// `None` is the ordinary case -- the producer took the source record's own
    /// timestamp and had nothing to qualify. A consumer putting a row on a clock must
    /// admit `None` and `occurred_at` and no other value; the schema's own wording for
    /// the field says the same ("a consumer bucketing gaps into day windows must treat
    /// `acquired_at` markers separately or exclude them").
    pub time_basis: Option<String>,
}

/// The span of time one source's observations cover in this ledger. See
/// [`Ledger::observed_range_by_source`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRange {
    pub source_kind: String,
    pub earliest: String,
    pub latest: String,
}

/// One row of `source_snapshot`: the archived bytes (`object_id`) observed for
/// `source_locator` at `acquired_at`, tagged with `source_kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub snapshot_id: i64,
    pub source_kind: String,
    pub source_locator: String,
    pub object_id: ObjectId,
    pub format_fingerprint: Option<String>,
    pub acquired_at: String,
}

const SNAPSHOT_COLUMNS: &str =
    "snapshot_id, source_kind, source_locator, object_id, format_fingerprint, acquired_at";

/// Optional filters for [`Ledger::find_snapshots`]; every `Some` field narrows the
/// result, an all-`None` filter matches everything. Kept as a plain struct with
/// optional fields rather than a builder: this is an internal library with exactly
/// one consumer, and a builder would just be ceremony around setting a few fields.
#[derive(Debug, Default, Clone, Copy)]
pub struct SnapshotFilter<'a> {
    /// Exact match on `source_kind` (e.g. `"claude-code"`, `"codex"`).
    pub source_kind: Option<&'a str>,
    /// Only locators starting with this prefix. Locators are home-relative paths
    /// (e.g. `.claude/projects/<dir>/<session>.jsonl`), so a prefix like
    /// `".claude/projects/"` selects one vendor and `".claude/projects/<dir>/"`
    /// selects one project directory.
    pub locator_prefix: Option<&'a str>,
    /// Inclusive lower bound on `acquired_at` (an ISO-8601 string, compared
    /// lexicographically, matching how `archive_file` stores it).
    pub acquired_from: Option<&'a str>,
    /// Exclusive upper bound on `acquired_at`. The range is half-open --
    /// `[acquired_from, acquired_to)` -- so that a caller selecting one day passes
    /// that day's start as `acquired_from` and the next day's start as
    /// `acquired_to`, without needing to know the last representable instant of a
    /// day.
    pub acquired_to: Option<&'a str>,
}

/// Map one `source_snapshot` row to a [`Snapshot`], validating `object_id` on the
/// way out.
///
/// `archive_file` only ever writes a well-formed `sha256:<hex>` string, but this
/// reads back whatever is actually in the database -- a hand-edited or corrupted row
/// is possible -- so a malformed value here must surface as a `rusqlite::Error`
/// (which callers see as `LedgerError::Db`), not panic later inside
/// `ObjectStore::path`'s hex-slicing and not be silently dropped from the results.
fn row_to_snapshot(row: &rusqlite::Row) -> rusqlite::Result<Snapshot> {
    let object_id_raw: String = row.get(3)?;
    let object_id = ObjectId::parse(&object_id_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Snapshot {
        snapshot_id: row.get(0)?,
        source_kind: row.get(1)?,
        source_locator: row.get(2)?,
        object_id,
        format_fingerprint: row.get(4)?,
        acquired_at: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cclog-ledger-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn archiving_the_same_file_twice_creates_one_snapshot() {
        let mut a = Ledger::open(&tmp("idem")).unwrap();
        let first = a
            .archive_file(
                "claude-code",
                "projects/p/s1.jsonl",
                b"line one\n",
                "2026-07-29T00:00:00Z",
                None,
            )
            .unwrap();
        let second = a
            .archive_file(
                "claude-code",
                "projects/p/s1.jsonl",
                b"line one\n",
                "2026-07-29T01:00:00Z",
                None,
            )
            .unwrap();

        assert!(matches!(first, Outcome::Created(_)));
        assert!(matches!(second, Outcome::AlreadyPresent(_)));
        assert_eq!(a.snapshot_count("projects/p/s1.jsonl").unwrap(), 1);
    }

    #[test]
    fn an_appended_file_becomes_a_second_snapshot_and_both_stay_readable() {
        let mut a = Ledger::open(&tmp("append")).unwrap();
        let Outcome::Created(first) = a
            .archive_file(
                "claude-code",
                "projects/p/s2.jsonl",
                b"line one\n",
                "2026-07-29T00:00:00Z",
                None,
            )
            .unwrap()
        else {
            panic!("expected the first archive to create an object");
        };
        let Outcome::Created(second) = a
            .archive_file(
                "claude-code",
                "projects/p/s2.jsonl",
                b"line one\nline two\n",
                "2026-07-29T01:00:00Z",
                None,
            )
            .unwrap()
        else {
            panic!("expected the appended file to create a new object");
        };

        assert_ne!(first.as_str(), second.as_str());
        assert_eq!(a.snapshot_count("projects/p/s2.jsonl").unwrap(), 2);
        assert_eq!(a.read(&first).unwrap(), b"line one\n");
        assert_eq!(a.read(&second).unwrap(), b"line one\nline two\n");
    }

    #[test]
    fn identical_bytes_from_two_locators_share_one_object_but_get_separate_snapshots() {
        let mut a = Ledger::open(&tmp("share")).unwrap();
        let one = a
            .archive_file(
                "codex",
                "sessions/a.jsonl",
                b"same",
                "2026-07-29T00:00:00Z",
                None,
            )
            .unwrap();
        let two = a
            .archive_file(
                "codex",
                "sessions/b.jsonl",
                b"same",
                "2026-07-29T00:00:00Z",
                None,
            )
            .unwrap();

        assert!(matches!(one, Outcome::Created(_)));
        // The object already exists, but this locator has never been recorded before.
        assert!(matches!(two, Outcome::Created(_)));
        assert_eq!(a.snapshot_count("sessions/a.jsonl").unwrap(), 1);
        assert_eq!(a.snapshot_count("sessions/b.jsonl").unwrap(), 1);
    }

    #[test]
    fn reopening_a_ledger_preserves_its_manifest() {
        let root = tmp("reopen");
        {
            let mut a = Ledger::open(&root).unwrap();
            a.archive_file(
                "claude-code",
                "projects/p/s3.jsonl",
                b"data",
                "2026-07-29T00:00:00Z",
                None,
            )
            .unwrap();
        }
        let a = Ledger::open(&root).unwrap();
        assert_eq!(a.snapshot_count("projects/p/s3.jsonl").unwrap(), 1);
    }

    #[test]
    fn the_ledger_database_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("dbperm");
        let _a = Ledger::open(&root).unwrap();
        let mode = std::fs::metadata(root.join("ledger.db"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "ledger.db records archived files and observations"
        );
    }

    #[test]
    fn the_cclog_root_and_archive_dir_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("rootperm");
        let _a = Ledger::open(&root).unwrap();
        for dir in [
            root.clone(),
            root.join("archive"),
            root.join("archive/objects"),
        ] {
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} must be owner-only", dir.display());
        }
    }

    #[test]
    fn the_ledger_wal_companion_files_are_owner_only_when_present() {
        // Under WAL mode, SQLite creates `ledger.db-wal` and `ledger.db-shm`
        // alongside `ledger.db` at the umask default (0644) as soon as any write
        // happens (schema creation counts). These companions hold uncheckpointed
        // ledger rows -- the same source-locator/kind/fingerprint data the 0600
        // requirement on ledger.db exists to protect -- so they must be
        // owner-only too. This is a soft check ("when present") because a future
        // SQLite version could checkpoint eagerly and remove them before we look;
        // the current bundled version reliably creates both at this point.
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("dbperm-wal");
        let _a = Ledger::open(&root).unwrap();
        for name in ["ledger.db-wal", "ledger.db-shm"] {
            let path = root.join(name);
            if !path.exists() {
                continue;
            }
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "{name} holds uncheckpointed ledger rows and must be owner-only"
            );
        }
    }

    #[test]
    fn opening_a_fresh_ledger_sets_user_version_to_the_current_schema_version() {
        let root = tmp("schema-version-fresh");
        let _a = Ledger::open(&root).unwrap();
        let db = Connection::open(root.join("ledger.db")).unwrap();
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn opening_a_legacy_ledger_with_no_user_version_and_no_observation_table_upgrades_it_in_place()
    {
        // Simulates a `ledger.db` written before this check existed: SQLite reports
        // `user_version` as 0 for any database that never set it, indistinguishable
        // from a database explicitly versioned at 0. `Ledger::open` must treat that
        // as "older than current" and upgrade it rather than refuse it. This
        // particular seed has no `observation` table at all (an even older, pre-M1
        // manifest-only shape) -- see the test below for the case where
        // `observation` already exists in its pre-`snapshot_id` shape, which is the
        // one that actually needs the reconciling step, not just a version stamp.
        let root = tmp("schema-version-legacy");
        std::fs::create_dir_all(&root).unwrap();
        {
            let db = Connection::open(root.join("ledger.db")).unwrap();
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
        }

        let _a = Ledger::open(&root).unwrap();

        let db = Connection::open(root.join("ledger.db")).unwrap();
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, SCHEMA_VERSION,
            "opening a pre-versioning ledger must stamp it with the current version"
        );
        // And the additive schema (observation, checkpoint, manifest_migration) must
        // actually have been applied, not skipped because the database already existed.
        let observation_table_exists: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'observation'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(observation_table_exists, 1);
    }

    /// Seeds a `ledger.db` shaped exactly like the schema that shipped immediately
    /// before `observation.snapshot_id` and `PRAGMA user_version` existed: today's
    /// `source_object` and `source_snapshot`, plus `observation` in its old
    /// 7-column shape (no `snapshot_id`) and `checkpoint`, unchanged across this
    /// version bump. `on_disk_version` is left at SQLite's default (0, unset).
    fn seed_pre_snapshot_id_ledger(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        let db = Connection::open(root.join("ledger.db")).unwrap();
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
             );
             CREATE TABLE observation (
               id             TEXT PRIMARY KEY,
               cclogdedupekey TEXT NOT NULL UNIQUE,
               source_kind    TEXT NOT NULL,
               event_type     TEXT NOT NULL,
               occurred_at    TEXT NOT NULL,
               workspace_ref  TEXT,
               body           TEXT NOT NULL
             );
             CREATE TABLE checkpoint (
               source_kind    TEXT NOT NULL,
               source_locator TEXT NOT NULL,
               snapshot_id    INTEGER NOT NULL REFERENCES source_snapshot(snapshot_id),
               cursor         TEXT,
               updated_at     TEXT NOT NULL,
               PRIMARY KEY (source_kind, source_locator)
             );",
        )
        .unwrap();
    }

    #[test]
    fn opening_a_ledger_whose_observation_table_predates_snapshot_id_upgrades_it_and_ingest_works()
    {
        // This is the exact gap the previous version of this test missed: it seeded
        // only `source_object`/`source_snapshot`, so `CREATE TABLE IF NOT EXISTS`
        // created `observation` fresh -- already in the current shape -- sidestepping
        // the case where `observation` already exists in its *old* shape. SQLite does
        // not reconcile column differences for a table that already exists, so
        // opening this must not just stamp the version forward over an unreconciled
        // table.
        let root = tmp("schema-version-legacy-observation");
        seed_pre_snapshot_id_ledger(&root);

        let mut a = Ledger::open(&root).unwrap();

        let db = Connection::open(root.join("ledger.db")).unwrap();
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The real proof this was retrofitted, not merely left alone: ingest must
        // actually work end-to-end, which requires `observation.snapshot_id` to
        // exist and be populated -- a stale 7-column table would fail this with a
        // raw "no such column" error.
        let obs = observation(
            "obs-legacy-upgrade-1",
            "claude-code|dev_test|ses_test|legacy|1",
            "dev.cclog.tool.started.v1",
            "2026-07-29T00:00:00.000Z",
        );
        a.ingest(
            snapshot_ref("p/legacy.jsonl", b"legacy bytes", "2026-07-29T00:00:00Z"),
            std::slice::from_ref(&obs),
            CheckpointAdvance {
                cursor: None,
                updated_at: "2026-07-29T00:00:00Z",
            },
        )
        .expect("ingest must succeed against a freshly-upgraded observation table");

        let stored_snapshot_id: i64 =
            a.db.query_row(
                "SELECT snapshot_id FROM observation WHERE cclogdedupekey = ?1",
                params!["claude-code|dev_test|ses_test|legacy|1"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            stored_snapshot_id > 0,
            "the retrofitted observation table must actually store snapshot_id"
        );
    }

    /// Seeds a `ledger.db` shaped exactly like schema version 1: every table in its
    /// current shape *except* `observation`, which lacks the `repository_ref` column
    /// version 2 promotes, plus one populated observation row. This is the state every
    /// ledger written before this release is in, and unlike
    /// [`seed_pre_snapshot_id_ledger`] it is a genuinely historical shape rather than
    /// one constructed by dropping a table.
    fn seed_v1_ledger(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        let db = Connection::open(root.join("ledger.db")).unwrap();
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
             );
             CREATE TABLE observation (
               id             TEXT PRIMARY KEY,
               cclogdedupekey TEXT NOT NULL UNIQUE,
               source_kind    TEXT NOT NULL,
               event_type     TEXT NOT NULL,
               occurred_at    TEXT NOT NULL,
               workspace_ref  TEXT,
               snapshot_id    INTEGER NOT NULL REFERENCES source_snapshot(snapshot_id),
               body           TEXT NOT NULL
             );
             CREATE INDEX observation_workspace_ref ON observation(workspace_ref);
             CREATE TABLE checkpoint (
               source_kind    TEXT NOT NULL,
               source_locator TEXT NOT NULL,
               snapshot_id    INTEGER NOT NULL REFERENCES source_snapshot(snapshot_id),
               cursor         TEXT,
               updated_at     TEXT NOT NULL,
               PRIMARY KEY (source_kind, source_locator)
             );
             INSERT INTO source_object (object_id, size_bytes, created_at)
               VALUES ('sha256:v1object', 9, '2026-07-29T00:00:00Z');
             INSERT INTO source_snapshot
               (source_kind, source_locator, object_id, format_fingerprint, acquired_at)
               VALUES ('claude-code', 'p/v1.jsonl', 'sha256:v1object', NULL,
                       '2026-07-29T00:00:00Z');
             INSERT INTO observation
               (id, cclogdedupekey, source_kind, event_type, occurred_at, workspace_ref,
                snapshot_id, body)
               VALUES ('obs-v1', 'claude-code|dev_v1|ses_v1|prompt.submitted|u-1',
                       'claude-code', 'dev.cclog.prompt.submitted.v1',
                       '2026-07-29T00:00:00.000Z', 'wsp_seededbyversion1', 1, '{}');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }

    #[test]
    fn opening_a_populated_version_1_ledger_adds_repository_ref_and_keeps_every_row() {
        // The upgrade path every existing ledger runs at its first open after this
        // release. `repository_ref` is nullable, so unlike the version 0 -> 1
        // `snapshot_id` case this one can be added to a *populated* table -- and it
        // must be, without disturbing the rows already there. A NULL is the honest
        // value for them: they were imported before repository identity was resolved.
        let root = tmp("schema-version-1-repository-ref");
        seed_v1_ledger(&root);

        let mut ledger = Ledger::open(&root).unwrap();

        let version: i64 = ledger
            .db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, SCHEMA_VERSION,
            "the upgrade must stamp the version"
        );
        assert!(
            column_exists(&ledger.db, "observation", "repository_ref").unwrap(),
            "the promoted column must exist after the upgrade"
        );

        // The pre-existing row survives, keeps its own columns, and reads back with a
        // NULL repository -- not a placeholder, and not dropped to make the schema
        // batch succeed.
        let (workspace, repository): (Option<String>, Option<String>) = ledger
            .db
            .query_row(
                "SELECT workspace_ref, repository_ref FROM observation WHERE id = 'obs-v1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the row seeded before the upgrade must still be there");
        assert_eq!(workspace.as_deref(), Some("wsp_seededbyversion1"));
        assert_eq!(
            repository, None,
            "a row imported before repository identity existed truthfully has none"
        );

        // And the retrofitted table actually works: a stale one would fail this with
        // a raw "no such column: repository_ref".
        let mut obs = observation(
            "obs-after-upgrade",
            "claude-code|dev_v1|ses_v1|prompt.submitted|u-2",
            "dev.cclog.prompt.submitted.v1",
            "2026-07-29T01:00:00.000Z",
        );
        obs.cclogrepositoryref = Some("rep_afterupgrade".to_string());
        ledger
            .ingest(
                snapshot_ref("p/v1.jsonl", b"v1 bytes\n", "2026-07-29T01:00:00Z"),
                std::slice::from_ref(&obs),
                CheckpointAdvance {
                    cursor: None,
                    updated_at: "2026-07-29T01:00:00Z",
                },
            )
            .expect("ingest must succeed against a freshly-upgraded observation table");
        assert_eq!(
            ledger
                .observation_identity("claude-code|dev_v1|ses_v1|prompt.submitted|u-2")
                .unwrap(),
            Some((None, Some("rep_afterupgrade".to_string()))),
            "and must store the promoted ref the upgrade made room for"
        );
    }

    #[test]
    fn an_out_of_date_ledger_is_reported_as_needing_an_upgrade_without_being_upgraded() {
        // The question `--dry-run` has to be able to ask before opening. Asking must
        // not itself perform the upgrade, or the guard built on it writes the very
        // thing it exists to prevent.
        let root = tmp("needs-upgrade-v1");
        seed_v1_ledger(&root);

        assert!(needs_schema_upgrade(&root).unwrap());

        let version: i64 = Connection::open(root.join("ledger.db"))
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 1,
            "asking must leave the on-disk version exactly where it was"
        );
        assert!(
            !column_exists(
                &Connection::open(root.join("ledger.db")).unwrap(),
                "observation",
                "repository_ref"
            )
            .unwrap(),
            "and must not have reconciled the schema either"
        );
    }

    #[test]
    fn a_current_ledger_and_a_missing_one_both_need_no_upgrade() {
        let root = tmp("needs-upgrade-current");
        assert!(
            !needs_schema_upgrade(&root).unwrap(),
            "there is no ledger here to upgrade"
        );
        assert!(
            !root.join("ledger.db").exists(),
            "and asking must not have created one"
        );

        let _ledger = Ledger::open(&root).unwrap();
        assert!(
            !needs_schema_upgrade(&root).unwrap(),
            "a ledger this build just wrote is already current"
        );
    }

    #[test]
    fn opening_a_ledger_whose_populated_observation_table_predates_snapshot_id_is_refused() {
        // The empty-table case above is safe to recreate silently; a *populated*
        // legacy `observation` table is not -- `snapshot_id` is `NOT NULL
        // REFERENCES`, and there is no value this build could correctly backfill
        // for a row it never saw the source snapshot for. This must refuse rather
        // than silently drop existing rows just to make the schema batch succeed.
        let root = tmp("schema-version-legacy-observation-nonempty");
        seed_pre_snapshot_id_ledger(&root);
        {
            let db = Connection::open(root.join("ledger.db")).unwrap();
            db.execute(
                "INSERT INTO observation
                   (id, cclogdedupekey, source_kind, event_type, occurred_at, workspace_ref, body)
                 VALUES ('obs-1', 'dedupe-1', 'claude-code', 'dev.cclog.tool.started.v1',
                         '2026-07-29T00:00:00.000Z', NULL, '{}')",
                [],
            )
            .unwrap();
        }

        match Ledger::open(&root) {
            Err(LedgerError::SchemaUpgradeBlocked { table, .. }) => {
                assert_eq!(table, "observation");
            }
            Err(other) => panic!("expected LedgerError::SchemaUpgradeBlocked, got {other:?}"),
            Ok(_) => panic!(
                "expected LedgerError::SchemaUpgradeBlocked, but Ledger::open succeeded -- it \
                 would have silently dropped an existing observation row"
            ),
        }

        // A refused upgrade must not advance the on-disk version: a version stamped
        // forward over a schema that was never actually reconciled would make the
        // problem permanently undetectable.
        let db = Connection::open(root.join("ledger.db")).unwrap();
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 0,
            "a refused upgrade must leave the version untouched so it stays retryable"
        );
    }

    #[test]
    fn opening_a_ledger_with_a_newer_schema_version_than_this_build_understands_is_refused() {
        let root = tmp("schema-too-new");
        {
            let _a = Ledger::open(&root).unwrap(); // creates ledger.db at SCHEMA_VERSION
        }
        {
            // Simulate a ledger.db written by a future cclog build.
            let db = Connection::open(root.join("ledger.db")).unwrap();
            db.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }

        match Ledger::open(&root) {
            Err(LedgerError::SchemaTooNew { found, understood }) => {
                assert_eq!(found, SCHEMA_VERSION + 1);
                assert_eq!(understood, SCHEMA_VERSION);
            }
            Err(other) => panic!("expected LedgerError::SchemaTooNew, got {other:?}"),
            Ok(_) => panic!("expected LedgerError::SchemaTooNew, but Ledger::open succeeded"),
        }
    }

    #[test]
    fn two_connections_racing_to_archive_the_same_locator_and_bytes_do_not_error() {
        // Two separate `Ledger` handles opened on the same root simulate two
        // `cclogger archive` processes racing to archive the same (locator, bytes)
        // pair -- the ordinary re-run case, since `cclogger archive` is built to be
        // run repeatedly. Before deriving `Outcome` from
        // `INSERT OR IGNORE ...` + `Connection::changes()` on `source_snapshot`, a
        // bare `SELECT` followed by a separate `INSERT` left a check-then-act
        // window: the loser's `INSERT` could violate
        // `UNIQUE(source_locator, object_id)` and surface as an opaque
        // `LedgerError::Db(..)` instead of a clean `Outcome::AlreadyPresent`. This
        // pins that both connections resolve to `Ok(_)` and the ledger ends up
        // with exactly one snapshot, regardless of how the two calls interleave.
        let root = tmp("race");
        drop(Ledger::open(&root).unwrap()); // create the schema outside the race

        let root_a = root.clone();
        let t1 = std::thread::spawn(move || {
            let mut a = Ledger::open(&root_a).unwrap();
            a.archive_file(
                "claude-code",
                "projects/p/race.jsonl",
                b"raced bytes",
                "2026-07-29T00:00:00Z",
                None,
            )
        });
        let root_b = root.clone();
        let t2 = std::thread::spawn(move || {
            let mut a = Ledger::open(&root_b).unwrap();
            a.archive_file(
                "claude-code",
                "projects/p/race.jsonl",
                b"raced bytes",
                "2026-07-29T00:00:00Z",
                None,
            )
        });

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        assert!(
            r1.is_ok(),
            "first connection's archive_file must not error: {r1:?}"
        );
        assert!(
            r2.is_ok(),
            "second connection's archive_file must not error: {r2:?}"
        );

        let a = Ledger::open(&root).unwrap();
        assert_eq!(a.snapshot_count("projects/p/race.jsonl").unwrap(), 1);
    }

    #[test]
    fn snapshots_for_locator_returns_growth_history_oldest_first_and_latest_picks_the_newest() {
        let mut a = Ledger::open(&tmp("history")).unwrap();
        let Outcome::Created(first) = a
            .archive_file(
                "claude-code",
                "projects/p/grow.jsonl",
                b"v1",
                "2026-07-29T00:00:00Z",
                None,
            )
            .unwrap()
        else {
            panic!("expected the first archive to create an object");
        };
        let Outcome::Created(second) = a
            .archive_file(
                "claude-code",
                "projects/p/grow.jsonl",
                b"v1v2",
                "2026-07-29T01:00:00Z",
                None,
            )
            .unwrap()
        else {
            panic!("expected the second archive to create an object");
        };
        let Outcome::Created(third) = a
            .archive_file(
                "claude-code",
                "projects/p/grow.jsonl",
                b"v1v2v3",
                "2026-07-29T02:00:00Z",
                None,
            )
            .unwrap()
        else {
            panic!("expected the third archive to create an object");
        };

        let history = a.snapshots_for_locator("projects/p/grow.jsonl").unwrap();
        let ids: Vec<ObjectId> = history.into_iter().map(|s| s.object_id).collect();
        assert_eq!(ids, vec![first.clone(), second.clone(), third.clone()]);

        let latest = a
            .latest_snapshot("projects/p/grow.jsonl")
            .unwrap()
            .expect("a locator with snapshots must have a latest one");
        assert_eq!(latest.object_id, third);
    }

    #[test]
    fn an_unknown_locator_returns_empty_rather_than_erroring() {
        let a = Ledger::open(&tmp("unknown-locator")).unwrap();
        assert_eq!(
            a.snapshots_for_locator("projects/p/never-archived.jsonl")
                .unwrap(),
            Vec::new()
        );
        assert!(
            a.latest_snapshot("projects/p/never-archived.jsonl")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn find_snapshots_by_source_kind_excludes_the_other_vendors_rows() {
        let mut a = Ledger::open(&tmp("kind-filter")).unwrap();
        a.archive_file(
            "claude-code",
            "projects/p/cc.jsonl",
            b"cc bytes",
            "2026-07-29T00:00:00Z",
            None,
        )
        .unwrap();
        a.archive_file(
            "codex",
            "sessions/cx.jsonl",
            b"codex bytes",
            "2026-07-29T00:00:00Z",
            None,
        )
        .unwrap();

        let claude_only = a
            .find_snapshots(&SnapshotFilter {
                source_kind: Some("claude-code"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(claude_only.len(), 1);
        assert_eq!(claude_only[0].source_locator, "projects/p/cc.jsonl");
    }

    #[test]
    fn find_snapshots_by_acquired_at_range_excludes_rows_outside_it() {
        let mut a = Ledger::open(&tmp("range-filter")).unwrap();
        a.archive_file(
            "claude-code",
            "day0.jsonl",
            b"before the window",
            "2026-07-28T23:59:59Z",
            None,
        )
        .unwrap();
        a.archive_file(
            "claude-code",
            "day1.jsonl",
            b"inside the window",
            "2026-07-29T12:00:00Z",
            None,
        )
        .unwrap();
        a.archive_file(
            "claude-code",
            "day2.jsonl",
            b"at the exclusive upper bound",
            "2026-07-30T00:00:00Z",
            None,
        )
        .unwrap();

        let one_day = a
            .find_snapshots(&SnapshotFilter {
                acquired_from: Some("2026-07-29T00:00:00Z"),
                acquired_to: Some("2026-07-30T00:00:00Z"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(one_day.len(), 1);
        assert_eq!(one_day[0].source_locator, "day1.jsonl");
    }

    #[test]
    fn find_snapshots_by_locator_prefix_selects_one_project_directory() {
        let mut a = Ledger::open(&tmp("prefix-filter")).unwrap();
        a.archive_file(
            "claude-code",
            ".claude/projects/proj-a/s1.jsonl",
            b"a1",
            "2026-07-29T00:00:00Z",
            None,
        )
        .unwrap();
        a.archive_file(
            "claude-code",
            ".claude/projects/proj-b/s1.jsonl",
            b"b1",
            "2026-07-29T00:00:00Z",
            None,
        )
        .unwrap();

        let proj_a = a
            .find_snapshots(&SnapshotFilter {
                locator_prefix: Some(".claude/projects/proj-a/"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(proj_a.len(), 1);
        assert_eq!(proj_a[0].source_locator, ".claude/projects/proj-a/s1.jsonl");
    }

    #[test]
    fn find_snapshots_by_locator_prefix_with_a_non_ascii_component() {
        // `locator_prefix` is compared with `substr(source_locator, 1, ?) = ?`, and
        // SQLite's `substr` counts UTF-8 *characters*, not bytes, when its argument is
        // TEXT. `"あ"` is one character but three UTF-8 bytes: a version of
        // `find_snapshots` that binds `prefix.len()` (a byte count) here would ask
        // `substr` for the first 3 characters of a locator whose matching prefix only
        // has 1, so the equality would never hold and this would return zero rows
        // instead of one. This is not a synthetic corner case: a `claude-code`
        // locator embeds the vendor's project-directory name, which encodes the
        // workspace's absolute path, so any workspace path with a non-ASCII component
        // (e.g. a project directory literally named with kanji) hits exactly this.
        let mut a = Ledger::open(&tmp("prefix-non-ascii")).unwrap();
        a.archive_file(
            "claude-code",
            ".claude/projects/あ-project/s1.jsonl",
            b"kanji project bytes",
            "2026-07-29T00:00:00Z",
            None,
        )
        .unwrap();
        a.archive_file(
            "claude-code",
            ".claude/projects/other-project/s1.jsonl",
            b"other bytes",
            "2026-07-29T00:00:00Z",
            None,
        )
        .unwrap();

        let matched = a
            .find_snapshots(&SnapshotFilter {
                locator_prefix: Some(".claude/projects/あ-project/"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            matched.len(),
            1,
            "a locator_prefix containing a multi-byte character must still match"
        );
        assert_eq!(
            matched[0].source_locator,
            ".claude/projects/あ-project/s1.jsonl"
        );
    }

    #[test]
    fn a_malformed_stored_digest_surfaces_as_an_error_not_a_panic() {
        // Simulates on-disk corruption or a hand-edited row: `archive_file` itself
        // can never write a value like this, so the only way to exercise this path
        // is to insert directly through the connection.
        let mut a = Ledger::open(&tmp("corrupt")).unwrap();
        a.archive_file(
            "claude-code",
            "fine.jsonl",
            b"ok bytes",
            "2026-07-29T00:00:00Z",
            None,
        )
        .unwrap();
        a.db.execute(
            "INSERT INTO source_object (object_id, size_bytes, created_at)
                 VALUES (?1, ?2, ?3)",
            params![
                "sha256:not-a-valid-hex-digest",
                2i64,
                "2026-07-29T00:00:00Z"
            ],
        )
        .unwrap();
        a.db.execute(
            "INSERT INTO source_snapshot
                   (source_kind, source_locator, object_id, acquired_at)
                 VALUES (?1, ?2, ?3, ?4)",
            params![
                "claude-code",
                "corrupt.jsonl",
                "sha256:not-a-valid-hex-digest",
                "2026-07-29T00:00:00Z"
            ],
        )
        .unwrap();

        assert!(
            a.latest_snapshot("corrupt.jsonl").is_err(),
            "a malformed object_id must surface as an error, not panic"
        );
        assert!(
            a.snapshots_for_locator("corrupt.jsonl").is_err(),
            "a malformed object_id must surface as an error, not panic"
        );
        assert!(
            a.find_snapshots(&SnapshotFilter::default()).is_err(),
            "a malformed row must fail enumeration, not be silently skipped"
        );
    }

    // -- observation / checkpoint / atomic ingest (M1, issue #12) ------------------

    use cclogger_domain::{IntegrityState, PrivacyClass, Profile, SourceKind};
    use serde_json::json;

    /// A minimal, valid synthetic `Observation` -- the tests below construct these
    /// directly rather than routing through an adapter, per issue #12's scope
    /// ("Your tests use synthetic observations you construct directly").
    fn observation(id: &str, dedupe_key: &str, event_type: &str, time: &str) -> Observation {
        Observation {
            specversion: "1.0".to_string(),
            id: id.to_string(),
            source: "cclog://device/dev_test/adapter/claude-code".to_string(),
            event_type: event_type.to_string(),
            subject: "session/ses_test".to_string(),
            time: time.to_string(),
            datacontenttype: "application/json".to_string(),
            traceparent: None,
            cclogschemaversion: 0,
            cclogsourcekind: SourceKind::ClaudeCode,
            cclogsourceversion: "claude-code-hook/1".to_string(),
            cclogadapterversion: "claude-code@0.0.0".to_string(),
            cclogsourcerecordref: None,
            cclogobservedat: time.to_string(),
            cclogmonotonicns: None,
            cclogbootid: None,
            cclogprivacyclass: PrivacyClass::T1Structured,
            cclogpurposehint: None,
            cclogdedupekey: dedupe_key.to_string(),
            cclogintegritystate: IntegrityState::Ok,
            cclogprofile: Profile::Personal,
            cclogworkspaceref: None,
            cclogrepositoryref: None,
            cclogcorrelationcluster: None,
            data: json!({}),
        }
    }

    fn observation_row_count(ledger: &Ledger, dedupe_key: &str) -> i64 {
        ledger
            .db
            .query_row(
                "SELECT COUNT(*) FROM observation WHERE cclogdedupekey = ?1",
                params![dedupe_key],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn snapshot_ref<'a>(
        locator: &'a str,
        bytes: &'a [u8],
        acquired_at: &'a str,
    ) -> SnapshotRef<'a> {
        SnapshotRef {
            source_kind: "claude-code",
            source_locator: locator,
            bytes,
            acquired_at,
            format_fingerprint: None,
        }
    }

    #[test]
    fn reingesting_the_same_dedupe_key_does_not_duplicate_and_reports_correctly() {
        // Simulates the ordinary re-run case: the same locator is scanned again (e.g.
        // an unchanged prefix re-read from a checkpoint), the same logical record is
        // parsed again, and the adapter mints a *fresh* `id` (ids are never stable
        // across runs -- see cclogger-domain's `ObservationDraft::finalize`) but the
        // *same* `cclogdedupekey` (built from source-stable identity, not the id).
        let mut a = Ledger::open(&tmp("obs-dedupe")).unwrap();
        let bytes = b"session content";

        let first = a
            .ingest(
                snapshot_ref("p/s.jsonl", bytes, "2026-07-29T00:00:00Z"),
                &[observation(
                    "obs-id-1",
                    "claude-code|dev_test|ses_test|prompt|1",
                    "dev.cclog.prompt.submitted.v1",
                    "2026-07-29T00:00:00.000Z",
                )],
                CheckpointAdvance {
                    cursor: Some("1"),
                    updated_at: "2026-07-29T00:00:00Z",
                },
            )
            .unwrap();
        assert_eq!(first.observations, vec![ObservationOutcome::Created]);

        let second = a
            .ingest(
                snapshot_ref("p/s.jsonl", bytes, "2026-07-29T01:00:00Z"),
                &[observation(
                    "obs-id-2-different-mint",
                    "claude-code|dev_test|ses_test|prompt|1",
                    "dev.cclog.prompt.submitted.v1",
                    "2026-07-29T00:00:00.000Z",
                )],
                CheckpointAdvance {
                    cursor: Some("1"),
                    updated_at: "2026-07-29T01:00:00Z",
                },
            )
            .unwrap();
        assert_eq!(
            second.observations,
            vec![ObservationOutcome::AlreadyPresent],
            "the statement enforcing uniqueness must report AlreadyPresent, not silently succeed as Created"
        );

        assert_eq!(
            observation_row_count(&a, "claude-code|dev_test|ses_test|prompt|1"),
            1,
            "a re-observed dedupe key must not accumulate a second row"
        );
    }

    #[test]
    fn ingest_rolls_back_every_table_when_one_observation_hard_conflicts() {
        // Two observations with *different* dedupe keys but the *same* `id` --
        // exactly the corruption/bug scenario `id` staying a PRIMARY KEY (distinct
        // from the `cclogdedupekey` UNIQUE constraint `ingest`'s `ON CONFLICT`
        // targets) exists to catch: see the `CREATE TABLE observation` schema
        // comment. The first insert succeeds inside the transaction; the second hits
        // a real `id` PRIMARY KEY conflict that `ON CONFLICT(cclogdedupekey)` does
        // not cover, so it surfaces as a genuine SQLite error instead of being
        // silently ignored.
        //
        // This pins the all-or-nothing property, not merely the happy path: even
        // though the first observation "succeeded" inside the transaction, and even
        // though the snapshot's own manifest row and the checkpoint advance are
        // independent tables, none of it must be visible afterward once the whole
        // call fails.
        let mut a = Ledger::open(&tmp("ingest-atomic")).unwrap();
        let locator = "p/atomic.jsonl";
        let bytes = b"one record, two observations, one is corrupt";

        let colliding_id = "same-id-both-rows";
        let observations = vec![
            observation(
                colliding_id,
                "claude-code|dev_test|ses_test|a|1",
                "dev.cclog.tool.started.v1",
                "2026-07-29T00:00:00.000Z",
            ),
            observation(
                colliding_id,
                "claude-code|dev_test|ses_test|b|2",
                "dev.cclog.tool.finished.v1",
                "2026-07-29T00:00:01.000Z",
            ),
        ];

        let result = a.ingest(
            snapshot_ref(locator, bytes, "2026-07-29T00:00:00Z"),
            &observations,
            CheckpointAdvance {
                cursor: Some("1"),
                updated_at: "2026-07-29T00:00:00Z",
            },
        );
        assert!(
            result.is_err(),
            "an id collision between two observations in the same batch must surface as an error"
        );

        assert_eq!(
            a.snapshot_count(locator).unwrap(),
            0,
            "the manifest row must not survive a rolled-back ingest"
        );
        assert_eq!(
            observation_row_count(&a, "claude-code|dev_test|ses_test|a|1"),
            0,
            "the first observation must not survive either, even though its own \
             insert succeeded before the second one failed"
        );
        assert_eq!(
            observation_row_count(&a, "claude-code|dev_test|ses_test|b|2"),
            0
        );
        assert!(
            a.checkpoint("claude-code", locator).unwrap().is_none(),
            "the checkpoint must not advance when the ingest as a whole failed"
        );
    }

    #[test]
    fn checkpoint_round_trips_and_advances_monotonically_but_never_regresses() {
        let mut a = Ledger::open(&tmp("checkpoint")).unwrap();
        let locator = "p/growing.jsonl";

        let first = a
            .ingest(
                snapshot_ref(locator, b"v1", "2026-07-29T00:00:00Z"),
                &[],
                CheckpointAdvance {
                    cursor: Some("10"),
                    updated_at: "2026-07-29T00:00:00Z",
                },
            )
            .unwrap();
        let first_snapshot_id = match first.snapshot {
            Outcome::Created(_) => a.latest_snapshot(locator).unwrap().unwrap().snapshot_id,
            Outcome::AlreadyPresent(_) => panic!("expected the first ingest to create a snapshot"),
        };

        let cp1 = a.checkpoint("claude-code", locator).unwrap().unwrap();
        assert_eq!(cp1.snapshot_id, first_snapshot_id);
        assert_eq!(cp1.cursor.as_deref(), Some("10"));

        // The file grew: a new snapshot, a further checkpoint advance.
        a.ingest(
            snapshot_ref(locator, b"v1v2", "2026-07-29T01:00:00Z"),
            &[],
            CheckpointAdvance {
                cursor: Some("20"),
                updated_at: "2026-07-29T01:00:00Z",
            },
        )
        .unwrap();
        let second_snapshot_id = a.latest_snapshot(locator).unwrap().unwrap().snapshot_id;
        assert!(second_snapshot_id > first_snapshot_id);

        let cp2 = a.checkpoint("claude-code", locator).unwrap().unwrap();
        assert_eq!(cp2.snapshot_id, second_snapshot_id);
        assert_eq!(cp2.cursor.as_deref(), Some("20"));

        // Re-ingesting the *first* (older) snapshot's bytes again -- its object
        // already exists, so this is a no-op on `source_snapshot` -- must not move
        // the checkpoint backward, even though its resolved `snapshot_id` is lower
        // than the checkpoint's current value.
        a.ingest(
            snapshot_ref(locator, b"v1", "2026-07-29T02:00:00Z"),
            &[],
            CheckpointAdvance {
                cursor: Some("5"),
                updated_at: "2026-07-29T02:00:00Z",
            },
        )
        .unwrap();
        let cp3 = a.checkpoint("claude-code", locator).unwrap().unwrap();
        assert_eq!(
            cp3.snapshot_id, second_snapshot_id,
            "a checkpoint must not regress to an older snapshot"
        );
        assert_eq!(
            cp3.cursor.as_deref(),
            Some("20"),
            "a checkpoint's cursor must not regress alongside an older snapshot_id"
        );
    }

    #[test]
    fn checkpoint_for_a_never_ingested_locator_is_none() {
        let a = Ledger::open(&tmp("checkpoint-none")).unwrap();
        assert!(
            a.checkpoint("claude-code", "never/seen.jsonl")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_same_malformed_record_yields_the_same_gap_marker_across_runs() {
        // Design doc §8: a parse failure or unsupported format variant must leave a
        // deterministic marker -- identity derived from the snapshot digest and the
        // record locator -- so re-running an import over the same bad record
        // produces the same marker every time rather than accumulating duplicates.
        // Building that specific tuple is the importer's job (#13); this ledger's
        // job is just to make the generic dedupe mechanism hold for a
        // `dev.cclog.source.gap.v1` observation the same way it holds for any other
        // event_type, which this test exercises directly.
        let mut a = Ledger::open(&tmp("gap-marker")).unwrap();
        let bytes = b"a record too malformed to parse";
        let snapshot_digest = "sha256:deadbeef"; // stand-in for the real object digest
        let record_locator = "line:42";
        let gap_dedupe_key = format!("claude-code|source.gap|{snapshot_digest}|{record_locator}");

        let mut gap_marker = observation(
            "gap-obs-run-1",
            &gap_dedupe_key,
            "dev.cclog.source.gap.v1",
            "2026-07-29T00:00:00.000Z",
        );
        gap_marker.cclogintegritystate = IntegrityState::Gap;
        gap_marker.data = json!({ "reason": "parse_error", "detail": null });

        let first = a
            .ingest(
                snapshot_ref("p/malformed.jsonl", bytes, "2026-07-29T00:00:00Z"),
                std::slice::from_ref(&gap_marker),
                CheckpointAdvance {
                    cursor: Some("42"),
                    updated_at: "2026-07-29T00:00:00Z",
                },
            )
            .unwrap();
        assert_eq!(first.observations, vec![ObservationOutcome::Created]);

        // A re-run: same bad record, same snapshot -- but a fresh `id`, since ids are
        // minted per run, not derived from content.
        let mut gap_marker_rerun = gap_marker.clone();
        gap_marker_rerun.id = "gap-obs-run-2-different-mint".to_string();

        let second = a
            .ingest(
                snapshot_ref("p/malformed.jsonl", bytes, "2026-07-29T01:00:00Z"),
                std::slice::from_ref(&gap_marker_rerun),
                CheckpointAdvance {
                    cursor: Some("42"),
                    updated_at: "2026-07-29T01:00:00Z",
                },
            )
            .unwrap();
        assert_eq!(
            second.observations,
            vec![ObservationOutcome::AlreadyPresent],
            "the same malformed record must yield the same gap marker, not a new one"
        );

        assert_eq!(observation_row_count(&a, &gap_dedupe_key), 1);
    }

    #[test]
    fn ingest_round_trips_the_full_observation_through_the_stored_body() {
        // The storage design (module doc comment) stores the complete canonical
        // observation as JSON and promotes only a handful of columns for indexing.
        // This pins that the *promoted* columns are not the only thing preserved --
        // fields that are not promoted (e.g. `data`, `subject`, `cclogprofile`) must
        // still round-trip exactly, since design doc §8's replay rules depend on it.
        let mut a = Ledger::open(&tmp("round-trip")).unwrap();
        let mut obs = observation(
            "obs-rt-1",
            "claude-code|dev_test|ses_test|rt|1",
            "dev.cclog.tool.started.v1",
            "2026-07-29T00:00:00.000Z",
        );
        obs.cclogworkspaceref = Some("wsp_ABC123".to_string());
        obs.subject = "session/ses_test/turn/trn_1/tool/tol_1".to_string();
        obs.data = json!({ "tool_family": "shell", "content_ref": null });

        a.ingest(
            snapshot_ref("p/rt.jsonl", b"bytes", "2026-07-29T00:00:00Z"),
            std::slice::from_ref(&obs),
            CheckpointAdvance {
                cursor: None,
                updated_at: "2026-07-29T00:00:00Z",
            },
        )
        .unwrap();

        let stored_body: String =
            a.db.query_row(
                "SELECT body FROM observation WHERE cclogdedupekey = ?1",
                params!["claude-code|dev_test|ses_test|rt|1"],
                |r| r.get(0),
            )
            .unwrap();
        let round_tripped: Observation = serde_json::from_str(&stored_body).unwrap();
        assert_eq!(round_tripped, obs, "stored body must round-trip losslessly");

        // The promoted columns used for indexing must also be populated correctly.
        let (workspace_ref, event_type, occurred_at, source_kind): (
            Option<String>,
            String,
            String,
            String,
        ) =
            a.db.query_row(
                "SELECT workspace_ref, event_type, occurred_at, source_kind
                 FROM observation WHERE cclogdedupekey = ?1",
                params!["claude-code|dev_test|ses_test|rt|1"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(workspace_ref.as_deref(), Some("wsp_ABC123"));
        assert_eq!(event_type, "dev.cclog.tool.started.v1");
        assert_eq!(occurred_at, "2026-07-29T00:00:00.000Z");
        assert_eq!(source_kind, "claude-code");
    }

    #[test]
    fn ingest_stores_the_resolved_snapshot_id_as_observation_provenance() {
        // Design doc §7's retention/delete story and §8's replay both need "which
        // observations came from this snapshot" -- this pins that the snapshot_id
        // `ingest` resolves internally (see the `SELECT snapshot_id FROM
        // source_snapshot` lookup) is the same one stored on the observation row,
        // not just used transiently for the checkpoint advance.
        let mut a = Ledger::open(&tmp("obs-snapshot-provenance")).unwrap();
        let obs = observation(
            "obs-prov-1",
            "claude-code|dev_test|ses_test|prov|1",
            "dev.cclog.tool.started.v1",
            "2026-07-29T00:00:00.000Z",
        );

        a.ingest(
            snapshot_ref("p/prov.jsonl", b"provenance bytes", "2026-07-29T00:00:00Z"),
            std::slice::from_ref(&obs),
            CheckpointAdvance {
                cursor: None,
                updated_at: "2026-07-29T00:00:00Z",
            },
        )
        .unwrap();

        let expected_snapshot_id = a
            .latest_snapshot("p/prov.jsonl")
            .unwrap()
            .unwrap()
            .snapshot_id;

        let stored_snapshot_id: i64 =
            a.db.query_row(
                "SELECT snapshot_id FROM observation WHERE cclogdedupekey = ?1",
                params!["claude-code|dev_test|ses_test|prov|1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored_snapshot_id, expected_snapshot_id);
    }

    #[test]
    fn occurred_at_with_a_non_utc_offset_is_normalized_but_the_stored_body_keeps_the_original() {
        // `Observation.time`'s schema constraint (`format: date-time`) permits any
        // offset, but `occurred_at` exists purely for lexicographic range queries --
        // an unnormalized `+09:00` would sort *after* a UTC timestamp hours earlier,
        // silently breaking every range filter built on this column. This pins both
        // halves of the fix: the promoted column is normalized, and `body` keeps the
        // original value verbatim, so the round-trip property is unaffected.
        let mut a = Ledger::open(&tmp("occurred-at-offset")).unwrap();
        let obs = observation(
            "obs-offset-1",
            "claude-code|dev_test|ses_test|offset|1",
            "dev.cclog.tool.started.v1",
            "2026-07-29T09:00:00.000+09:00", // the same instant as 2026-07-29T00:00:00.000Z
        );

        a.ingest(
            snapshot_ref("p/offset.jsonl", b"offset bytes", "2026-07-29T00:00:00Z"),
            std::slice::from_ref(&obs),
            CheckpointAdvance {
                cursor: None,
                updated_at: "2026-07-29T00:00:00Z",
            },
        )
        .unwrap();

        let (occurred_at, stored_body): (String, String) =
            a.db.query_row(
                "SELECT occurred_at, body FROM observation WHERE cclogdedupekey = ?1",
                params!["claude-code|dev_test|ses_test|offset|1"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        assert_eq!(
            occurred_at, "2026-07-29T00:00:00.000Z",
            "a +09:00 offset must be normalized to the equivalent UTC instant with a Z suffix"
        );

        let round_tripped: Observation = serde_json::from_str(&stored_body).unwrap();
        assert_eq!(
            round_tripped.time, "2026-07-29T09:00:00.000+09:00",
            "body must keep the original offset timestamp verbatim, unaffected by \
             promoted-column normalization"
        );
    }

    // -- workspace_identity registry + promoted repository_ref (M1, issue #16) -----

    #[test]
    fn an_identity_is_registered_once_and_readable_by_its_ref() {
        let ledger = Ledger::open(&tmp("identity-basic")).unwrap();
        ledger
            .register_identity(
                "rep_abc",
                "repository",
                "github.com/acme/api",
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        assert_eq!(
            ledger.identity_display("rep_abc").unwrap().as_deref(),
            Some("github.com/acme/api")
        );
    }

    #[test]
    fn an_unregistered_ref_reads_back_as_none_rather_than_erroring() {
        let ledger = Ledger::open(&tmp("identity-unregistered")).unwrap();
        assert_eq!(ledger.identity_display("rep_missing").unwrap(), None);
    }

    #[test]
    fn registering_the_same_ref_twice_keeps_the_first_seen_time() {
        let ledger = Ledger::open(&tmp("identity-first-seen")).unwrap();
        ledger
            .register_identity(
                "rep_abc",
                "repository",
                "github.com/acme/api",
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        ledger
            .register_identity(
                "rep_abc",
                "repository",
                "github.com/acme/api",
                "2026-08-02T00:00:00Z",
            )
            .unwrap();
        let seen: String = ledger
            .connection_for_test()
            .query_row(
                "SELECT first_seen FROM workspace_identity WHERE ref = 'rep_abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            seen, "2026-08-01T00:00:00Z",
            "re-import must not rewrite when an identity was first observed"
        );
    }

    #[test]
    fn identities_are_listed_by_kind_and_ordered_by_display_name() {
        let ledger = Ledger::open(&tmp("identity-listing")).unwrap();
        ledger
            .register_identity(
                "rep_b",
                "repository",
                "github.com/acme/zulu",
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        ledger
            .register_identity(
                "rep_a",
                "repository",
                "github.com/acme/api",
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        ledger
            .register_identity(
                "wsp_c",
                "workspace",
                "github.com/acme/api@issue-1",
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        let repos = ledger.identities("repository").unwrap();
        assert_eq!(
            repos,
            vec![
                ("rep_a".to_string(), "github.com/acme/api".to_string()),
                ("rep_b".to_string(), "github.com/acme/zulu".to_string()),
            ],
            "workspaces must not appear under the repository kind"
        );
    }

    #[test]
    fn a_ledger_written_before_the_identity_table_existed_gains_it_on_open() {
        let root = tmp("identity-table-upgrade");
        {
            let ledger = Ledger::open(&root).unwrap();
            ledger
                .connection_for_test()
                .execute("DROP TABLE workspace_identity", [])
                .unwrap();
            ledger
                .connection_for_test()
                .pragma_update(None, "user_version", 1i64)
                .unwrap();
        }
        let reopened = Ledger::open(&root).unwrap();
        reopened
            .register_identity(
                "rep_abc",
                "repository",
                "github.com/acme/api",
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        let version: i64 = reopened
            .connection_for_test()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// Builds one `(SnapshotRef, Observation)` pair for the repository_ref
    /// promotion tests below, shaped like the other ingest tests' fixtures above
    /// (`snapshot_ref` + `observation`, e.g.
    /// `reingesting_the_same_dedupe_key_does_not_duplicate_and_reports_correctly`) --
    /// this plan's own text for these two tests calls a two-argument
    /// `ledger.ingest(snapshot_id, &[obs])`, which does not match `Ledger::ingest`'s
    /// actual three-argument `(SnapshotRef, &[Observation], CheckpointAdvance)`
    /// signature used by every other ingest test in this file; this fixture returns
    /// what the real signature needs instead. `cclogrepositoryref` is set to
    /// `repository_ref`.
    fn repository_ref_fixture(repository_ref: Option<&str>) -> (SnapshotRef<'static>, Observation) {
        let mut obs = observation(
            "obs-repository-ref-1",
            "claude-code|dev_test|ses_test|repo|1",
            "dev.cclog.prompt.submitted.v1",
            "2026-08-01T00:00:00.000Z",
        );
        obs.cclogrepositoryref = repository_ref.map(String::from);
        (
            snapshot_ref(
                "p/repository-ref.jsonl",
                b"repository ref fixture bytes",
                "2026-08-01T00:00:00Z",
            ),
            obs,
        )
    }

    #[test]
    fn an_ingested_observation_promotes_its_repository_ref_into_a_queryable_column() {
        let mut ledger = Ledger::open(&tmp("repository-ref-promoted")).unwrap();
        // Build one observation carrying a repository ref, ingest it, and read the
        // column back -- the point of promoting it is that a report can group on it
        // without parsing every body.
        let (snapshot, obs) = repository_ref_fixture(Some("rep_API"));
        ledger
            .ingest(
                snapshot,
                &[obs],
                CheckpointAdvance {
                    cursor: None,
                    updated_at: "2026-08-01T00:00:00Z",
                },
            )
            .unwrap();
        let promoted: Option<String> = ledger
            .connection_for_test()
            .query_row("SELECT repository_ref FROM observation", [], |r| r.get(0))
            .unwrap();
        assert_eq!(promoted.as_deref(), Some("rep_API"));
    }

    #[test]
    fn an_observation_with_no_repository_promotes_null_rather_than_a_placeholder() {
        let mut ledger = Ledger::open(&tmp("repository-ref-null")).unwrap();
        let (snapshot, obs) = repository_ref_fixture(None);
        ledger
            .ingest(
                snapshot,
                &[obs],
                CheckpointAdvance {
                    cursor: None,
                    updated_at: "2026-08-01T00:00:00Z",
                },
            )
            .unwrap();
        let promoted: Option<String> = ledger
            .connection_for_test()
            .query_row("SELECT repository_ref FROM observation", [], |r| r.get(0))
            .unwrap();
        assert_eq!(promoted, None);
    }
}
