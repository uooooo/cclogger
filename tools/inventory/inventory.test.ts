import { expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { surveyClaude, surveyCodex, renderMarkdown } from "./inventory.ts";

const CANARY = "SUPERSECRET-CANARY-VALUE";

function corpus(): string {
  const root = mkdtempSync(join(tmpdir(), "cclog-inv-"));
  const proj = join(root, "claude", "projects", "-Users-x-repo");
  mkdirSync(proj, { recursive: true });
  writeFileSync(
    join(proj, "s1.jsonl"),
    [
      JSON.stringify({ type: "user", uuid: "u1", timestamp: "2026-07-20T01:00:00Z", cwd: "/w", message: { content: CANARY } }),
      JSON.stringify({ type: "assistant", uuid: "u2", timestamp: "2026-07-20T01:00:05Z", cwd: "/w", message: { content: [{ type: "text", text: CANARY }] } }),
    ].join("\n"),
  );
  const sess = join(root, "codex", "sessions");
  mkdirSync(sess, { recursive: true });
  writeFileSync(
    join(sess, "r1.jsonl"),
    [
      JSON.stringify({ type: "session_meta", timestamp: "2026-07-20T01:00:00Z", payload: { id: "s", cwd: "/w", cli_version: "1.2.3" } }),
      JSON.stringify({ type: "response_item", timestamp: "2026-07-20T01:00:01Z", payload: { type: "message", role: "user", content: CANARY } }),
      JSON.stringify({ type: "response_item", timestamp: "2026-07-20T01:00:02Z", payload: { type: "custom_tool_call", id: "ctc_1", call_id: "call_1", name: "shell" } }),
    ].join("\n"),
  );
  return root;
}

test("claude survey counts record types and reports uuid presence", () => {
  const r = surveyClaude(join(corpus(), "claude", "projects"));
  expect(r.files).toBe(1);
  expect(r.recordKinds.find((k) => k.kind === "user")?.count).toBe(1);
  expect(r.recordKinds.find((k) => k.kind === "user")?.stableIdField).toBe("uuid");
});

test("codex survey distinguishes record kinds with and without stable ids", () => {
  const r = surveyCodex([join(corpus(), "codex", "sessions")]);
  const msg = r.recordKinds.find((k) => k.kind === "response_item:message");
  const call = r.recordKinds.find((k) => k.kind === "response_item:custom_tool_call");
  expect(msg?.stableIdField).toBe(null);
  expect(call?.stableIdField).toBe("id");
});

test("rendered markdown never contains source content", () => {
  const root = corpus();
  const md = renderMarkdown(
    surveyClaude(join(root, "claude", "projects")),
    surveyCodex([join(root, "codex", "sessions")]),
  );
  expect(md).not.toContain(CANARY);
});

function partialIdCorpus(): string {
  const root = mkdtempSync(join(tmpdir(), "cclog-inv-partial-"));
  const sess = join(root, "codex", "sessions");
  mkdirSync(sess, { recursive: true });
  writeFileSync(
    join(sess, "r1.jsonl"),
    [
      // Same kind, but only the first record's payload carries an id-shaped field.
      JSON.stringify({ type: "response_item", timestamp: "2026-07-21T01:00:00Z", payload: { type: "partial_id_kind", id: "abc" } }),
      JSON.stringify({ type: "response_item", timestamp: "2026-07-21T01:00:01Z", payload: { type: "partial_id_kind", name: "no-id-here" } }),
    ].join("\n"),
  );
  return sess;
}

function mismatchedIdFieldCorpus(): string {
  const root = mkdtempSync(join(tmpdir(), "cclog-inv-mismatch-"));
  const sess = join(root, "codex", "sessions");
  mkdirSync(sess, { recursive: true });
  writeFileSync(
    join(sess, "r1.jsonl"),
    [
      // Same kind, both records carry an id-shaped field, but under different names.
      JSON.stringify({ type: "response_item", timestamp: "2026-07-21T02:00:00Z", payload: { type: "mixed_id_kind", id: "abc" } }),
      JSON.stringify({ type: "response_item", timestamp: "2026-07-21T02:00:01Z", payload: { type: "mixed_id_kind", call_id: "call_abc" } }),
    ].join("\n"),
  );
  return sess;
}

test("codex survey reports no stable id when only some records of a kind carry one", () => {
  const r = surveyCodex([partialIdCorpus()]);
  const k = r.recordKinds.find((k) => k.kind === "response_item:partial_id_kind");
  expect(k?.count).toBe(2);
  expect(k?.stableIdField).toBe(null);
});

test("codex survey reports no stable id when the id field name differs across records of a kind", () => {
  const r = surveyCodex([mismatchedIdFieldCorpus()]);
  const k = r.recordKinds.find((k) => k.kind === "response_item:mixed_id_kind");
  expect(k?.count).toBe(2);
  expect(k?.stableIdField).toBe(null);
});
