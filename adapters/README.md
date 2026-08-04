# Adapters and synthetic fixtures

Each adapter turns one vendor / OS source into canonical observations
([schema](../schema/)). This directory currently holds **synthetic fixtures** only — the
adapter transform code lands in Phase 1.

> All fixtures are hand-authored and synthetic. Real user transcripts are never used as a
> test corpus.

## Fixture format

Every `*.fixture.json` is a self-documenting bundle:

```jsonc
{
  "description": "what real situation this represents",
  "exercises": ["design points this fixture pins down"],
  "source":   { /* synthetic vendor / OS input (hook payload, OTLP span, git log…) */ },
  "expected": [ /* the canonical observation(s) that input should normalize to */ ]
}
```

The conformance harness validates every `expected[]` observation. `source` documents the
vendor-side shape and becomes the golden input once the Phase 1 adapters can transform it.

## What the fixtures demonstrate

| Fixture | Point |
|---|---|
| `claude-code/session-lifecycle` | hook lifecycle → canonical session events; opaque source |
| `claude-code/prompt-and-response` | human_attention vs agent_execution evidence; content stays local |
| `claude-code/tool-command` | tool lifecycle → `agent_execution` interval; `Bash → shell` |
| `claude-code/dedup-hook-and-otlp` | one action from two sources → correlation cluster + `possible_duplicate`, no destructive merge |
| `claude-code/integrity-gap` | `source.gap` so coverage separates "no work" from "not observed" |
| `codex/session-lifecycle` | a second adapter normalizes to the *same* canonical events |
| `codex/tool-shell` | mirrors the data-model §3 example; `traceparent` kept local |
| `codex/approval` | approval request/resolve as enums, no command body |
| `codex/transcript-path-opaque` | `transcript_path` kept as an opaque locator; the path never enters the row |
| `codex/otlp-trace` | OTLP as a complementary `source_kind` |
| `git/commit` | artifact evidence: opaque `repository_ref`, bucketed line counts, no diff, no message, no author, no workspace |

Every observation is routed to the `personal` profile.

## Run the conformance checks

```sh
cd tools/conformance
bun install
bun run validate
```

Green means every synthetic observation satisfies the schema, carries no leaked
identifiers, and keeps content out of the metadata tier.

## Rust adapter golden tests

The `expected` rows are also golden outputs for the Rust adapters
([`crates/cclogger-adapters`](../crates/cclogger-adapters/)). Each adapter is a pure function of
`(source_record, injected ctx)`; the ctx supplies the device, clocks, ids, and the
vendor-id → opaque-ref keystore, so `transform(source) == expected` holds exactly.

```sh
cargo test
```

Runs the whole-observation golden comparison for the Claude Code, Codex and git
adapters, and the round-trip conformance for every fixture.

