# cclog conformance harness (Phase 0)

Validates every synthetic canonical observation under `adapters/**/fixtures/*.fixture.json`
against [`schema/cclog.observation.v0.schema.json`](../../schema/), plus two gates the JSON
Schema cannot express:

- **leak scan** — no email, absolute home path, bearer token, JWT, cloud/API key, private
  key, `password=`, or raw 40-hex commit sha may appear in a canonical observation.
- **tier invariant** — at `t0`/`t1`, `content_ref` / `message_ref` must be `null`.
- **commit invariant** — a `commit.observed` payload is a *closed* set of metadata fields
  whose values are tokens, buckets or counts. A commit message is prose and no regular
  expression recognizes prose, so the gate for the first source that arrives carrying PII
  is its shape: an `author`, an `author_email`, a `message` or a changed-path list has
  nowhere to go, and the sha reaches the row only as the `cmt_…` pseudonym in the
  subject.

## Run

```sh
bun install
bun run validate      # exits non-zero if any observation fails
```

## What it is / is not

- **Is**: a schema + privacy conformance gate over the fixtures, and the executable
  definition of "a well-formed canonical observation" for v0.
- **Is not**: the adapter transform (Phase 1, Rust) or the allocation/clock spikes. Those
  will consume the same fixtures as golden inputs.

Toolchain: bun + [ajv](https://ajv.js.org/) (2020-12). `node_modules/` is gitignored;
run `bun install` once. This TS harness is a Phase 0 spike; the Phase 1 core replaces it
with Rust golden tests over the same fixtures.
