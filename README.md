# cclogger

**A local-first work ledger for AI-assisted coding.** It reconstructs what you
worked on, and for how long, from the transcripts Claude Code and Codex already
write — both vendors in one timeline.

Nothing leaves your machine. There is no account and no upload.

## Why

Other tools around Claude Code count tokens and cost. None of them tell you
where the time went — which projects, for how long, and where two of them were
running at once. A per-project total erases that last part: on one real day
here, 7 of 18 work blocks spanned more than one repository.

It also keeps what would otherwise be deleted. Claude Code removes session files
older than [`cleanupPeriodDays`](https://code.claude.com/docs/en/settings.md) at
startup — 30 days by default. If that is the only thing you came here for, raise
that setting instead; it is one line and you do not need this tool for it.

## What it is not

- Not a productivity score. No keystrokes, no screenshots, no presence tracking.
- Not a timesheet. It measures what an AI session can observe, which is a
  fraction of the work.
- Not a viewer for vendor JSONL.

## Install

Apple Silicon, Intel Mac, or x86_64 Linux. **Windows is not supported** — the
archive relies on POSIX file permissions.

<!-- Remove with the first tagged release. See docs/releasing.md. -->
> **Nothing is published yet.** The release pipeline is committed but no version
> has been tagged, so there is nothing on npm and no binary to download. Use the
> from-source option for now.

```bash
npm install -g cclogger                                     # or: bun install -g cclogger
cargo install --git https://github.com/uooooo/cclogger cclogger-cli    # Rust 1.88+
```

[docs/install.md](docs/install.md) covers the standalone installer, proxies, and
`command not found: cargo`.

## Use

```bash
cclogger archive     # copy vendor transcripts into a local content-addressed archive
cclogger import      # turn the archive, the hook spool and git into observations
cclogger report      # today: hours per workstream
cclogger report --week    # also --month 2026-07, --from/--to
cclogger log         # today: the shape of the day
cclogger hook-install     # print the settings that turn on live hook capture
```

`archive` is the one to automate. It only saves what exists when it runs;
sessions the vendor deleted in the meantime are gone — see
[docs/scheduling.md](docs/scheduling.md).

### `cclogger report`

```
2026-07-26  UTC+09:00

basis         AI-observed attention (estimated; not total work time). Work done away
              from this machine, or without an AI session, leaves no trace in it.
parameters    attention window [-1m, +5m] around each human prompt; agent gap threshold 5m

workstream                                    attention (est.)  prompts    share
client-a                                                 2h36m       46    60.7%
  github.com/client-a/platform                           2h36m       46
personal                                                   30m       11    11.8%
  github.com/me/research                                   19m        7
  github.com/me/coursework                                 11m        4
personal/tooling                                           17m        8     6.4%
unassigned                                                 54m       21    21.0%
  github.com/client-b/protocol                             54m       21

attention        4h16m  day-wide union, overlaps counted once -- the day's total
                 5h13m  per-repository naive sum -- counts overlapping windows twice; not a total

agent runtime    4h37m  day-wide union of the cross-session timeline (5m gap threshold)
```

*A real day; names replaced, timings unchanged.*

Grouping lives in `~/.cclog/config/workstreams.toml`:

```toml
[[rule]]
match = "github.com/me/*"
workstream = "personal"
```

### `cclogger log`

A table cannot say whether `client-a 2h36m` and `personal/tooling 17m` happened
at once or one after the other. This can.

```
                     14  15  16  17  18  19  20  21  22  23
client-a/platform      ▁▁▁▁▁▁▁ ▁▁█▁▁ ▁▆▂▁▁  ▁▁▁  ▁▁▁▁▁▁▁▁▁     46 prompts  claude-code+codex
                        ▲▲▲▲▲  ▲▲ ▲  ▲▲▲▲▲  ▲ ▲  ▲▲▲▲▲▲▲▲
client-b/protocol                       ▁▁▁▁▁▁▇▁ ▁             21 prompts  claude-code
me/research                 ▁▁ ▂▁▁                              7 prompts  claude-code
(all)                  ▁▁▁▁▁▁▁▁▃▂█▁▁▁▁▆▂▁▁▁▁▁▁▇▁ ▁▁▁▁▁▁▁▁▁     86 prompts

15:26-16:14     48m elapsed     29m attention (est.)    12 prompts  claude-code
  spans 3 repositories -- one stretch of time, not one per repository
    client-a/platform           17m attention    58.3%     8 prompts     515 observations
    me/tooling                   6m attention    20.6%     1 prompts     153 observations
    tools  shell 70 · read 58 · edit 42 · other 7 · mcp 4
```

`--from 13:00 --to 18:00` narrows the whole query, not just the drawn axis.
`--explain` prints the full basis for the day.

**It does not tell you what the work was about.** The ledger holds timestamps,
event types and tool families, and no prompt text at all.

Live capture with Claude Code hooks: [docs/hooks.md](docs/hooks.md).

## Reading the numbers

The tool prints its own basis on every run. In short:

- **Attention** is a `[-1m, +5m]` window around each human prompt, unioned. Read
  a long response without typing and that time is not counted.
- **Agent runtime** is clustering of one cross-session timeline, not a sum of
  tool durations. On a real corpus, 82.5% of within-session gaps of 5 minutes or
  more had another session active inside them — that is switching projects, not
  stopping.
- **Response time** is your gap, not the model's: an assistant's completion to
  the prompt that follows it. Reported as quartiles, never as a sum or a mean.
- **Commits are evidence, not a clock.** A commit is an instant, so it is in no
  total and does not mark a day as worked.
- **Coverage is stated.** A record the importer cannot map becomes an explicit
  gap with a reason, excluded from every clock and counted in the report.

## How it works

```
~/.claude, ~/.codex  ──▶  cclogger archive  ──▶  content-addressed archive (0600 under 0700)
                                                      │
Claude Code hooks    ──▶  cclogger hook  ──▶  spool  ─┤
                                                      │
your git repositories ───────── git log ─────────────▶┤
                                                      ▼
                                              cclogger import  ──▶  SQLite ledger
                                                                        │
                                                          ┌─────────────┴─────────────┐
                                                          ▼                           ▼
                                                    cclogger report                cclogger log
```

`import` reads the archive and the spool, never the live vendor directories — by
the time you ask about a day, the vendor may have deleted it.

- [`schema/`](schema/) — the canonical observation, CloudEvents-compatible
- [`adapters/`](adapters/) — synthetic fixtures, one per design decision
- [`tools/conformance/`](tools/conformance/) — validates every fixture against
  the schema, plus a leak scan and a metadata-only invariant
- [`skills/cclogger/`](skills/cclogger/) — ask from inside a Claude Code session

## Status

Working and used daily by its author. The APIs and the ledger schema are not
stable yet.

Not yet built: live capture for Codex, a daemon to run `archive` unattended, and
anything involving more than one person.

## License

Apache-2.0. See [LICENSE](LICENSE).
