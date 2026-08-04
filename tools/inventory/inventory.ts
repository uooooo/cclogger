/**
 * Structural inventory of local AI-tool logs.
 *
 * Emits key names, counts, and enumerated discriminator values only. Never emits a
 * field *value* from a content-bearing field, so its output is safe to commit.
 */
import { Glob } from "bun";
import { existsSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

export type RecordKind = {
  kind: string;
  count: number;
  /** Field holding a per-record stable id, or null when the kind has none. */
  stableIdField: string | null;
  /** Sorted union of top-level key names seen for this kind. Names only, no values. */
  keys: string[];
};

export type Survey = {
  vendor: string;
  files: number;
  bytes: number;
  earliest: string | null;
  latest: string | null;
  recordKinds: RecordKind[];
  /** (shape signature -> file count) for format-variant dispatch. */
  fingerprints: { signature: string; files: number }[];
};

const ID_CANDIDATES = ["uuid", "id", "call_id", "event_id"];

function scanLines(dir: string): { file: string; lines: string[] }[] {
  const out: { file: string; lines: string[] }[] = [];
  // A vendor that was never installed simply has nothing to scan.
  if (!existsSync(dir)) return out;
  for (const rel of new Glob("**/*.jsonl").scanSync({ cwd: dir })) {
    const file = join(dir, rel);
    const text = readFileSync(file, "utf8");
    out.push({ file, lines: text.split("\n").filter((l) => l.trim().length > 0) });
  }
  return out;
}

function fold(
  vendor: string,
  scanned: { file: string; lines: string[] }[],
  kindOf: (rec: any) => string,
  signatureOf: (recs: any[]) => string,
): Survey {
  const kinds = new Map<string, { count: number; keys: Set<string>; id: string | null; idSeen: boolean }>();
  const fps = new Map<string, number>();
  let bytes = 0;
  let earliest: string | null = null;
  let latest: string | null = null;

  for (const { file, lines } of scanned) {
    bytes += statSync(file).size;
    const recs: any[] = [];
    for (const line of lines) {
      let rec: any;
      try {
        rec = JSON.parse(line);
      } catch {
        continue;
      }
      recs.push(rec);
      const kind = kindOf(rec);
      const entry = kinds.get(kind) ?? { count: 0, keys: new Set<string>(), id: null, idSeen: false };
      entry.count += 1;
      for (const k of Object.keys(rec)) entry.keys.add(k);
      const payload = rec.payload && typeof rec.payload === "object" ? rec.payload : rec;
      const found = ID_CANDIDATES.find((c) => typeof payload[c] === "string");
      // A kind has a stable id only if *every* record of that kind carries one.
      if (!entry.idSeen) {
        entry.id = found ?? null;
        entry.idSeen = true;
      } else if (entry.id !== (found ?? null)) {
        entry.id = null;
      }
      kinds.set(kind, entry);

      const ts = typeof rec.timestamp === "string" ? rec.timestamp : null;
      if (ts) {
        if (!earliest || ts < earliest) earliest = ts;
        if (!latest || ts > latest) latest = ts;
      }
    }
    const sig = signatureOf(recs);
    fps.set(sig, (fps.get(sig) ?? 0) + 1);
  }

  return {
    vendor,
    files: scanned.length,
    bytes,
    earliest,
    latest,
    recordKinds: [...kinds.entries()]
      .map(([kind, v]) => ({ kind, count: v.count, stableIdField: v.id, keys: [...v.keys].sort() }))
      .sort((a, b) => b.count - a.count),
    fingerprints: [...fps.entries()]
      .map(([signature, files]) => ({ signature, files }))
      .sort((a, b) => b.files - a.files),
  };
}

export function surveyClaude(projectsDir: string): Survey {
  return fold(
    "claude-code",
    scanLines(projectsDir),
    (rec) => String(rec.type ?? "unknown"),
    (recs) => {
      const versions = new Set(recs.map((r) => String(r.version ?? "?")));
      return `version=${[...versions].sort().join(",")}`;
    },
  );
}

export function surveyCodex(dirs: string[]): Survey {
  const scanned = dirs.flatMap((d) => scanLines(d));
  return fold(
    "codex",
    scanned,
    (rec) => {
      const t = String(rec.type ?? "unknown");
      const p = rec.payload && typeof rec.payload === "object" && rec.payload.type;
      return p ? `${t}:${p}` : t;
    },
    (recs) => {
      const meta = recs.find((r) => r.type === "session_meta");
      const keys = meta?.payload ? Object.keys(meta.payload).sort().join(",") : "none";
      const cli = meta?.payload?.cli_version ?? "?";
      return `cli=${cli};meta_keys=${keys}`;
    },
  );
}

export function renderMarkdown(...surveys: Survey[]): string {
  const out: string[] = ["# Source inventory", ""];
  out.push("key 名・件数・discriminator 値のみ。source の値は含まない。", "");
  for (const s of surveys) {
    out.push(`## ${s.vendor}`, "");
    out.push(`- files: ${s.files}`);
    out.push(`- bytes: ${s.bytes}`);
    out.push(`- range: ${s.earliest ?? "-"} .. ${s.latest ?? "-"}`, "");
    out.push("| record kind | count | stable id | top-level keys |");
    out.push("|---|---:|---|---|");
    for (const k of s.recordKinds) {
      out.push(`| \`${k.kind}\` | ${k.count} | ${k.stableIdField ? `\`${k.stableIdField}\`` : "**none**"} | ${k.keys.map((x) => `\`${x}\``).join(" ")} |`);
    }
    out.push("", "| format fingerprint | files |", "|---|---:|");
    for (const f of s.fingerprints) out.push(`| \`${f.signature}\` | ${f.files} |`);
    out.push("");
  }
  return out.join("\n");
}

if (import.meta.main) {
  const home = process.env.HOME ?? "";
  const rootArg = process.argv.indexOf("--root");
  const root = rootArg >= 0 ? process.argv[rootArg + 1] : home;
  const md = renderMarkdown(
    surveyClaude(join(root, ".claude", "projects")),
    surveyCodex([join(root, ".codex", "sessions"), join(root, ".codex", "archived_sessions")]),
  );
  process.stdout.write(md);
}
