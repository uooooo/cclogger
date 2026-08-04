//! Local archive + ledger for raw source bytes and (from M1) the canonical
//! observations and checkpoints derived from them.
//!
//! [`object`] is the content-addressed byte store (`<root>/archive/objects`).
//! [`ledger`] is the single SQLite database (`<root>/ledger.db`) that ties archived
//! snapshots back to the files they came from, and -- so that the crash invariant in
//! design doc §7 can actually hold -- also holds the observations and checkpoints
//! derived from them, all committed through one connection.

pub mod ledger;
pub mod migrate;
pub mod object;
mod occurred_at;

pub use ledger::{
    Checkpoint, CheckpointAdvance, IngestReport, Ledger, LedgerError, ObservationIdentity,
    ObservationOutcome, ObservationRow, Outcome, Snapshot, SnapshotFilter, SnapshotRef,
    SourceRange, needs_schema_upgrade,
};
pub use object::{ObjectId, ObjectIdError, ObjectStore};
