/**
 * cclog Phase 0 conformance harness.
 *
 * Validates every synthetic canonical observation in
 * `adapters/**​/fixtures/*.fixture.json` and `adapters/**​/shapes/*.shape.json`
 * (issue #13 turned the Claude Code historical adapter's shape fixtures into golden
 * tests too, by adding an `expected` array to them -- this harness picks those up
 * the same way it already picks up `expected` from `.fixture.json` files) against the
 * canonical observation schema, plus two extra gates the JSON Schema cannot express
 * by itself:
 *   - leak scan: a canonical observation must never contain emails, absolute
 *     home paths, bearer tokens, keys, raw commit shas, etc.
 *   - tier invariant: metadata-only tiers (t0/t1) must not carry a content
 *     pointer (content_ref / message_ref must be null).
 *   - commit invariant: a `commit.observed` row's payload is a closed set of
 *     metadata fields with no free text in it. A commit is the first source that
 *     arrives carrying obvious PII -- an author name, an author address, and a
 *     message -- and a message is content in exactly the way a prompt is. The two
 *     gates above cannot catch one on their own: a message is arbitrary prose, and
 *     no regular expression recognizes prose. What is checkable is the shape, so
 *     that is what is checked -- every key allowed on the payload is listed, and
 *     every value on it must match a token or an enum member, which prose cannot.
 *
 * Run:  bun install && bun run validate
 */
import Ajv2020 from "ajv/dist/2020";
import addFormats from "ajv-formats";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const repoRoot = new URL("../../", import.meta.url).pathname;
const schema = JSON.parse(
  readFileSync(join(repoRoot, "schema/cclog.observation.v0.schema.json"), "utf8"),
);

const ajv = new Ajv2020({ allErrors: true, strict: false });
addFormats(ajv);
const validate = ajv.compile(schema);

/** Patterns that must never appear in a canonical observation. */
const LEAK_PATTERNS: [string, RegExp][] = [
  ["email", /[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/],
  ["abs_home_path", /(?:\/Users\/|\/home\/|\/root\/|[A-Za-z]:\\Users\\)/],
  ["bearer", /\bBearer\s+[A-Za-z0-9._-]{8,}/],
  ["jwt", /\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{5,}\./],
  ["aws_key", /\bAKIA[0-9A-Z]{16}\b/],
  ["private_key", /-----BEGIN [A-Z ]*PRIVATE KEY-----/],
  ["github_pat", /\bghp_[A-Za-z0-9]{20,}/],
  ["password_kv", /\bpass(word|wd)?["']?\s*[:=]/i],
  // A full git object name. The sha is the one genuinely stable id cclog ingests and
  // it is deliberately pseudonymized before it reaches a row (`cmt_…`), because it is
  // also a lookup key into a public repository's contents. A 64-hex digest (an object
  // store id) does not match: the word boundaries require exactly 40.
  ["raw_commit_sha", /\b[0-9a-f]{40}\b/],
];

function walkStrings(v: unknown, out: string[]): void {
  if (typeof v === "string") out.push(v);
  else if (Array.isArray(v)) for (const x of v) walkStrings(x, out);
  else if (v && typeof v === "object") for (const x of Object.values(v)) walkStrings(x, out);
}

function leakScan(obs: unknown): string[] {
  const strings: string[] = [];
  walkStrings(obs, strings);
  const hits: string[] = [];
  for (const s of strings)
    for (const [name, re] of LEAK_PATTERNS)
      if (re.test(s)) hits.push(`${name}: ${JSON.stringify(s).slice(0, 80)}`);
  return hits;
}

function tierInvariants(obs: any): string[] {
  const problems: string[] = [];
  const tier = obs?.cclogprivacyclass;
  if (tier === "t0_aggregate" || tier === "t1_structured") {
    for (const field of ["content_ref", "message_ref"]) {
      const v = obs?.data?.[field];
      if (v !== undefined && v !== null)
        problems.push(`${field} must be null at ${tier} (got ${JSON.stringify(v)})`);
    }
  }
  return problems;
}

/** Every key a `commit.observed` payload may carry, and what each may hold. */
const COMMIT_DATA_FIELDS: Record<string, RegExp> = {
  repository_ref: /^rep_[A-Za-z0-9]+$/,
  changed_paths_count: /^[0-9]+$/,
  insertions_bucket: /^(0|1-9|10-99|100-999|1000-9999|10000\+)$/,
  deletions_bucket: /^(0|1-9|10-99|100-999|1000-9999|10000\+)$/,
  message_ref: /^$/, // null only -- checked below, and by the tier invariant
  time_basis: /^(occurred_at|acquired_at|copied_at|received_at)$/,
};

/**
 * A commit row carries metadata about a commit and nothing of the commit itself.
 *
 * The closed key set is the gate a leak scan cannot be: `author`, `author_email`,
 * `message`, `subject`, `branch` or a list of changed paths would each fail here, and
 * a commit message added to any *allowed* key fails the value pattern for it. Absence
 * of `message_ref` fails too -- null is how a metadata-only row says the content
 * exists and is not here, and a missing field says nothing at all.
 */
function commitInvariants(obs: any): string[] {
  if (obs?.type !== "dev.cclog.commit.observed.v1") return [];
  const problems: string[] = [];
  const data = obs?.data ?? {};
  for (const [key, value] of Object.entries(data)) {
    const allowed = COMMIT_DATA_FIELDS[key];
    if (!allowed) {
      problems.push(`data.${key} is not a field a commit row may carry`);
      continue;
    }
    if (key === "message_ref") {
      if (value !== null) problems.push(`data.message_ref must be null (got ${JSON.stringify(value)})`);
      continue;
    }
    if (!allowed.test(String(value)))
      problems.push(`data.${key} = ${JSON.stringify(value)} is not a ${key}`);
  }
  if (!("message_ref" in data)) problems.push("data.message_ref must be present and null");
  if (!("repository_ref" in data)) problems.push("data.repository_ref must be present");
  // The sha reaches the row only as a pseudonym, and the subject is where it would
  // otherwise appear in full.
  if (!/^artifact\/commit\/cmt_[A-Za-z0-9]+$/.test(obs?.subject ?? ""))
    problems.push(`subject ${JSON.stringify(obs?.subject)} is not artifact/commit/<pseudonym>`);
  if (obs?.cclogworkspaceref != null)
    problems.push("a commit carries no workspace: `git log` cannot say which worktree it was made from");
  return problems;
}

const fixtureGlob = new Bun.Glob("adapters/**/fixtures/*.fixture.json");
const shapeGlob = new Bun.Glob("adapters/**/shapes/*.shape.json");
const files = [
  ...fixtureGlob.scanSync({ cwd: repoRoot }),
  ...shapeGlob.scanSync({ cwd: repoRoot }),
].sort();

let observations = 0;
let failures = 0;

for (const rel of files) {
  const fx = JSON.parse(readFileSync(join(repoRoot, rel), "utf8"));
  const expected: unknown[] = fx.expected ?? [];
  console.log(`\n• ${rel}  (${expected.length} obs)`);
  if (fx.description) console.log(`  ${fx.description}`);
  expected.forEach((obs: any, i: number) => {
    observations++;
    const errs: string[] = [];
    if (!validate(obs))
      for (const e of validate.errors ?? []) errs.push(`schema ${e.instancePath || "/"} ${e.message}`);
    errs.push(...leakScan(obs).map((h) => `leak ${h}`));
    errs.push(...tierInvariants(obs).map((p) => `invariant ${p}`));
    errs.push(...commitInvariants(obs).map((p) => `commit ${p}`));
    const label = `  [${i}] ${obs?.type ?? "?"}`;
    if (errs.length) {
      failures++;
      console.log(`${label}  ✗`);
      for (const e of errs) console.log(`      - ${e}`);
    } else {
      console.log(`${label}  ✓`);
    }
  });
}

console.log(`\n${"─".repeat(52)}`);
console.log(`fixtures: ${files.length}  observations: ${observations}  failures: ${failures}`);
process.exit(failures ? 1 : 0);
