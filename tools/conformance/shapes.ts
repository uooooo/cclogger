/**
 * Shape fixtures record the *structure* of real vendor records without their content.
 * This scan enforces that: no emails or absolute home paths.
 */
import { Glob } from "bun";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const repoRoot = join(import.meta.dir, "..", "..");
const EMAIL = /[\w.+-]+@[\w-]+\.[\w.]+/;
const HOME_PATH = /\/(Users|home)\/(?!x\b|alice\b|dev\b)[\w.-]+/;
const REQUIRED = ["vendor", "variant", "description", "records"];

export function scanShapes(): string[] {
  const errs: string[] = [];
  const files = [...new Glob("adapters/*/shapes/*.shape.json").scanSync({ cwd: repoRoot })].sort();
  if (files.length === 0) errs.push("no shape fixtures found");
  for (const rel of files) {
    const raw = readFileSync(join(repoRoot, rel), "utf8");
    let doc: any;
    try {
      doc = JSON.parse(raw);
    } catch (e) {
      errs.push(`${rel}: invalid json (${e})`);
      continue;
    }
    for (const k of REQUIRED) {
      if (!(k in doc)) errs.push(`${rel}: missing "${k}"`);
    }
    if (!Array.isArray(doc.records) || doc.records.length === 0) {
      errs.push(`${rel}: "records" must be a non-empty array`);
    }
    const m = raw.match(EMAIL);
    if (m) errs.push(`${rel}: email-like value ${m[0]}`);
    const p = raw.match(HOME_PATH);
    if (p) errs.push(`${rel}: real-looking home path ${p[0]}`);
  }
  return errs;
}

if (import.meta.main) {
  const errs = scanShapes();
  for (const e of errs) console.error(`FAIL ${e}`);
  console.log(errs.length === 0 ? "shape fixtures: ok" : `shape fixtures: ${errs.length} problem(s)`);
  process.exit(errs.length === 0 ? 0 : 1);
}
