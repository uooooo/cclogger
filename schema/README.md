# cclog canonical observation schema — v0

> Status: pre-stable. This directory is the normative, machine-checkable encoding of
> the canonical observation. Where it and prose disagree, the schema is what runs.

`cclog.observation.v0.schema.json` is a JSON Schema (draft 2020-12) for **one canonical
observation** as stored in the local ledger. The external form is
[CloudEvents 1.0](https://github.com/cloudevents/spec) compatible: CloudEvents core
attributes plus `cclog*` extension attributes. Content is never inlined — observations
point at content only through an opaque `content_ref`, which is `null` in the default
metadata-only mode.

## Envelope

| Field | Meaning | Notes |
|---|---|---|
| `specversion` | CloudEvents version | const `1.0` |
| `id` | `observation_id` | UUIDv7; approximates emit order, not wall-time truth |
| `source` | device + adapter identity | opaque only: `cclog://device/<opaque>/adapter/<kind>`. No username / path / machine id |
| `type` | event kind + schema version | `dev.cclog.<domain>.<event>.v<n>`. Vendor event names never appear here |
| `subject` | hierarchical ref path | opaque tokens, e.g. `session/ses_2XQ/turn/trn_91M/tool/tol_4CJ` |
| `time` | `occurred_at` | source wall clock |
| `datacontenttype` | const `application/json` | |
| `traceparent` | W3C trace context | local correlation evidence only; never re-exported as-is |
| `cclogschemaversion` | schema version | const `0` |
| `cclogsourcekind` | `claude-code` \| `codex` \| `git` \| `otlp` \| `os-presence` \| `media` | |
| `cclogsourceversion` / `cclogadapterversion` | source contract + adapter fingerprints | |
| `cclogsourcerecordref` | opaque local locator of the raw record | e.g. a `transcript_path` is kept only as this ref, never as the path |
| `cclogobservedat` | `observed_at` | collector receipt clock, separate from `time` |
| `cclogmonotonicns` / `cclogbootid` | monotonic clock + boot id | for sleep / NTP / reboot handling |
| `cclogprivacyclass` | `t0_aggregate` \| `t1_structured` \| `t2_content` \| `t3_media` | |
| `cclogpurposehint` | nullable inference | never a capture fact or authorization input |
| `cclogdedupekey` | dedupe key | stable-id HMAC or canonical tuple (see data-model §4) |
| `cclogintegritystate` | `ok` \| `possible_duplicate` \| `gap` \| `clock_skew` \| `quarantined` | |
| `cclogprofile` | `personal` \| `client` | local routing zone; anything unclassified stays `personal`. Local-only |
| `cclogworkspaceref` / `cclogrepositoryref` | pseudonyms | `wsp_…` / `rep_…`, nullable |
| `cclogcorrelationcluster` | multi-source grouping | same real action from Hook + OTLP shares one `cor_…` |
| `data` | event payload | typed per kind (below) |

Unknown top-level attributes are **rejected** (`additionalProperties: false`) so leaked
fields (a stray `username`, `cwd`, …) fail loudly. Adapter-specific metadata belongs in a
size-bounded extension inside `data` (deferred past v0).

## Event catalog v0

Domains: `session` `turn` `agent` (lifecycle) · `prompt` `response` `approval`
(interaction) · `tool` `command` `file` (tool) · `commit` `branch` `pr` `ticket`
(artifact) · `device` `editor` (presence) · `source` `clock` `capture` (integrity) ·
`policy` `content` `share` (policy) · `media` (media).

v0 ships **typed `data`** (validated via `if/then` on `type`) for the kinds the fixtures
exercise: `session.started/ended`, `prompt.submitted`, `response.completed`,
`approval.requested/resolved`, `tool.started/finished`, `command.finished`,
`commit.observed`, `device.active`, `source.gap/duplicate`. Other kinds validate the
envelope strictly and leave `data` open until their shapes are pinned.

## Absence is `null`, never a zero

Two nullable fields in `data` exist specifically so a producer can say "I did not
measure this" rather than fabricate a value that a consumer cannot tell apart from a
measurement:

- **`duration_ms`** (`tool.finished`, `command.finished`) — `null` means the producer
  could not close the interval. `0` means it measured something that finished within a
  millisecond. Live hooks and historical import both write this field and are
  aggregated together, so a producer without a measurement must emit `null`.
- **`detail`** (`source.gap`) — `null` means no *safe* label was available. `detail` is
  a validated identifier, never source content: a producer must drop an implausible
  value rather than truncate or escape it.

Count buckets (`prompt_tokens`, `response_tokens`, `turn_count`, …) say it by
**absence** instead, since the bucket enum has no `null` member and every such field is
optional: `"0"` means the source reported a count that fell in the zero bucket, so a
producer whose source carries no count at all omits the field. A Claude Code `user`
transcript record and a Codex `user_message` both carry no token count, so
`prompt.submitted` rows from those channels have no `prompt_tokens` — not `"0"`.

**`time_basis`** says which clock an observation's `time` came from, and is the one
field that decides whether a row may go on a clock at all:

- `occurred_at` — the record's own timestamp, i.e. when the thing happened.
- `acquired_at` — the moment the snapshot was collected. Most record kinds carry no
  timestamp of their own, so most `source.gap` markers are dated this way.
- `copied_at` — the record's own timestamp, on a record that was **copied out of
  another transcript**. A Codex fork or subagent spawn re-writes its parent's whole
  history into the child's file in one call, and Codex stamps every record it
  serializes with the write time, so those copies carry the time of the copying. The
  original time is not recoverable from that file: history is copied as a
  `Vec<RolloutItem>`, which has no timestamp field.
- `received_at` — a receipt stamp taken by cclog's own receiver at the moment the
  source invoked it **synchronously**. Used by the Claude Code hook channel, where no
  hook event carries a timestamp of its own, so the receiver's clock is the only time
  source there is.

Absence means the producer took the source record's own timestamp and had nothing to
qualify. A consumer putting an observation on a clock must admit absence, `occurred_at`
and `received_at`, and must exclude `acquired_at` and `copied_at`; a day-window query
that does not check this will misattribute the latter two.

The line is between a measurement of the event's own instant and something else
standing in for one that was never taken. `acquired_at` can be days after the fact and
`copied_at` belongs to a different event entirely; `received_at` differs from the event
by the time the source took to spawn the receiver, which is milliseconds and bounded.
Excluding it would not buy a better number — it would leave the hook channel
contributing to no number at all.

## Invariants the harness adds on top of the schema

- **Opaque source** — enforced by the `source` / `subject` patterns.
- **Metadata-only tiers** — at `t0`/`t1`, `content_ref` and `message_ref` must be `null`.
- **No leaks** — no email, absolute home path, bearer token, JWT, cloud key, private key,
  or `password=` may appear in any string of a canonical observation.

See [`tools/conformance`](../tools/conformance/) for the runnable checks.

## Compatibility policy (v0)

- **v0 is pre-stable**: it may change without a migration path until promoted by ADR.
- **Additive is minor**: new optional fields and new event kinds do not bump the version.
- **Breaking is versioned**: removing/renaming a field, tightening a required set, or
  changing a field's meaning bumps the event's `.vN` (per-kind) and/or `cclogschemaversion`.
- Adapters declare which kinds/fields they can populate via a capability manifest
  (architecture §3.3); absence is `coverage`, not `0`.
