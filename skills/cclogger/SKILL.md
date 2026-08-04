---
name: cclogger
description: Reports what the user worked on and for how long, from their local cclogger work ledger built from Claude Code and Codex transcripts. Use for questions about the user's own recent work -- what they did today, yesterday, this week, or this month; how long they spent on a given project or repository; when they were working; and whether work on different things overlapped.
when_to_use: Fires on things like "what did I work on today", "what did I do yesterday", "how much time did I spend on X this week", "when was I working", "did my sessions overlap", or any direct request for a cclogger report or log.
argument-hint: "[today|yesterday|this week|this month|YYYY-MM-DD|<project>]"
---

# cclogger: what the user worked on, and for how long

`cclogger` reconstructs work history from Claude Code and Codex transcripts into a local
ledger, then reports hours per workstream (`report`) or the shape of a day (`log`). It is
explicit that it is not a timesheet and prints its own caveats -- carry those through rather
than rounding them away into something more confident-sounding.

## 0. Find the binary

Try `command -v cclogger`. If that fails, both `cargo install` and this project's shell
installer put it in `$HOME/.cargo/bin`, which may not be on `PATH` -- a documented gotcha of
this project (see its README's "command not found: cargo" note). Check
`test -x "$HOME/.cargo/bin/cclogger"`; if it's there, use that absolute path for every call
below, and mention once that `PATH` is missing it (fix: `source "$HOME/.cargo/env"`, or add
that line to the shell rc file).

If it's nowhere: say plainly that cclogger isn't installed, give the real install command --

```
npm install -g cclogger      # or: bun install -g cclogger
```

-- note that it supports macOS and Linux only (Apple Silicon, Intel Mac, x86_64 Linux; no
Windows build exists), and that installing downloads a prebuilt binary and so needs network.
Then stop there. Do not answer the question with anything else in its place.

<!-- Remove this paragraph with the first tagged release; see docs/releasing.md. -->
**Not published yet:** until the first release is tagged there is nothing on npm, so give
`cargo install --git https://github.com/uooooo/cclogger cclogger-cli` instead, and say it needs
Rust 1.88+.

## 1. Bring the ledger current, and read what it says

Before answering, run:

```
cclogger archive
cclogger import
```

Both are cheap on unchanged data and safe to re-run. `archive` only saves what exists in
`~/.claude`/`~/.codex` *right now*; `import` turns the archive into observations and prints
its own coverage (locators scanned/processed, observations created, gaps, any copied-history
count). Read that output rather than discarding it once the exit code is checked:

- If `archive` fails outright (nonzero exit, `archive failed: ...`), say so and stop --
  don't report against a ledger that may not reflect a fresh archive.
- Nonzero exit from `import`, or a nonzero `locators unreadable` count: say so; the ledger
  is current except for those specific files.
- `cursors reset` or a nonzero `gaps` count is worth one line, not a reason to stop.
- A `git` section with a `not read` list means the ledger knows work happened in those
  repositories and this run could not collect their commits (moved, deleted, or no
  `user.email` configured there). Worth one line: a commit count without it reads as
  complete.
- If `import` reports "no ledger ... run cclogger archive first", that only happens under
  `--dry-run`, which you are not passing -- if you see it anyway, something upstream is
  unusual; relay the message verbatim rather than guessing past it.

Do not read `~/.claude/projects/` or `~/.codex/` transcripts directly to answer this, even
"just to double check." Going around `cclogger` defeats the reason the ledger exists -- it
holds timestamps, event types and tool families, never prompt text (a deliberate boundary
the cclogger project keeps as a deliberate boundary) -- and risks pulling
raw session content from unrelated projects into this conversation.

## 2. Pick the command and the window

`report` answers *how much*; `log` answers *when* and *what overlapped*. Both take the same
day/period flags.

| Question shape                       | Command                                       |
| ------------------------------------- | ---------------------------------------------- |
| what did I work on today              | `report` (today is the default)               |
| what did I work on yesterday          | `report --day <yesterday's date>`             |
| this week / this month                | `report --week` / `report --month YYYY-MM`    |
| a named day                           | `report --day YYYY-MM-DD`                     |
| a range of days                       | `report --from YYYY-MM-DD --to YYYY-MM-DD`    |
| how long on `<project>`               | `report` over the window above, then that project's row |
| when was I working / what overlapped  | `log` (add `--explain` if asked how it's computed) |

Compute relative dates yourself rather than guessing one:

```
date -v-1d +%F 2>/dev/null || date -d yesterday +%F     # yesterday
date +%Y-%m                                              # this month
```

If the window is genuinely ambiguous (no period named, and "today" isn't clearly implied
either), default to today and say plainly that's what you did.

**Timezone.** `report`/`log` bucket "today" at this machine's own UTC offset, read fresh on
every run -- so it already agrees with the `date` calls above, and needs no flag. Do *not*
compute an offset with `date +%z` and pass it along: that round-trips a half-hour zone
(`+0530`, `+0545`, `+0845`) through whole hours and would quietly move the day boundary for
anyone in one. Pass `--tz-offset-hours` only when the user asks about a *different* offset
than the machine's -- whole hours (`9`, `-5`) or `±HH:MM` (`5:30`, `-3:30`). Valid range is
UTC-12..+14; anything else is refused, not clamped. If the offset can't be read at all the
command stops rather than guessing one; relay that refusal rather than working around it.

For "how long on `<project>`", match the name loosely against the repository lines the
report prints (e.g. `github.com/acme/platform` for "acme" or for "platform"). If nothing in
that window matches, say there's no observed activity for it there rather than guessing --
offer to widen the window (`--week`, `--month`) instead of assuming which one was meant.

## 3. Answer like the tool does

Prefer showing the tool's own relevant lines verbatim (in a code block) alongside a short
plain-English recap, rather than only paraphrasing -- that keeps the caveats attached to the
numbers instead of dropping out of a summary.

- Quote figures as printed (`2h36m`), never re-rounded into "about N hours."
- `report` always prints two attention numbers together: the day/period union, and the
  per-repository naive sum -- because per-project totals overlap and don't add up to the
  total. Keep both, labelled, whenever you cite an overall figure; don't cite only one.
- State once, near the top of your answer, what this measures: AI-observed attention, an
  estimate anchored on prompts, not total work time -- it cannot see anything done away
  from this machine, or without an AI session. Reuse that wording (it's `report`'s and
  `log --explain`'s own `basis` line) rather than paraphrasing it into something firmer.
- If asked what the work *was* (content, not just timing or duration), say the ledger can't
  answer that -- it holds no prompt text, only timestamps, event types, tool families and
  repository identity. Don't infer a topic from a repo name or a tool-call pattern.
- A workstream or day can show real activity but zero attention (e.g. the agent ran with no
  human prompt) -- `0m` against a nonzero prompt/observation count. That's "nothing to
  allocate," not "nothing happened." Keep the two apart the way the tool's output does.
- A period average prints `n/a`, never `0m`, when no day in it holds any observation --
  that's "no coverage," not "zero work." A single day or period entirely outside what's
  been collected prints its own `note` line saying so; relay that rather than silently
  reporting `0m` as if it settled the question.
- If there's no `~/.cclog/config/workstreams.toml`, `report`/`log` already say so inline and
  put every repository under `unassigned` -- not an error, just ungrouped. Mention it only
  when it's relevant to what was asked (e.g. everything landing in one bucket).

## 4. One thing to disclose, not hide

Running `cclogger` from inside this session is itself observed: the Bash calls this skill
just made land in *this* session's own transcript, and a later `archive`/`import` will fold
them into this repository's ledger. It's small, but if the user is asking specifically about
today, or about this session, say so rather than let them find it themselves in tomorrow's
numbers.

## Reference: real output shape

The two lines every `report` always prints together, taken from this project's own real-day
example in its README (repository and workstream names replaced there, timings real) --
reproduced here only so the shape is recognizable; never reuse these numbers for an actual
answer:

```
attention        4h16m  day-wide union, overlaps counted once -- the day's total
                 5h13m  per-repository naive sum -- counts overlapping windows twice; not a total
```

For flags this doesn't cover, `cclogger report --help` and `cclogger log --help` print the
full set -- no need to guess them from here.
