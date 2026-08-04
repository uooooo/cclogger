# Live capture with Claude Code hooks

Claude Code's own documentation points at hooks rather than at the transcript
file, and it is right to.

```bash
cclogger hook-install
```

prints the settings JSON that registers `cclogger hook` for eight events. Paste
it into the `hooks` object of `~/.claude/settings.json` (or a project's
`.claude/settings.json`).

It **prints rather than writes**: that file is yours, its `hooks` object may
already hold entries this tool knows nothing about, and a merge that got that
wrong would silently break your editor. Pasting it twice is harmless — Claude
Code deduplicates hook handlers by command string.

Once registered, every hook event appends one line to
`~/.cclog/spool/hooks.jsonl`, and `cclogger import` drains it alongside the
archive. Nothing else changes.

## This does not replace `archive` and `import`

Hooks record from the moment you install them; Claude Code buffers nothing and
replays nothing, so no hook can reach a day that already happened. The
transcript path stays the only route to history, and `cclogger report`'s
coverage block prints the instant hook capture begins so a number is never read
as covering more than it does.

## What the hook path adds

- **A real tool duration.** `PostToolUse` carries the vendor's own
  `duration_ms`, documented as excluding "time spent in permission prompts and
  PreToolUse hooks". The transcript path has to close that interval by
  subtracting one record's timestamp from another's, which includes the wait at
  an approval prompt — the longest historical tool "duration" measured that way
  was 42.8 hours.
- **Subagent attribution.** `agent_id` and `agent_type` are present only when a
  hook fires inside a subagent, so the channel can say which tool calls a
  subagent made. The transcript path cannot do this reliably.
- **Failure, as distinct from silence.** `StopFailure` and `PostToolUseFailure`
  are separate events, so a turn or a tool that failed is recorded as failed
  rather than as `unknown`.

## Three things it cannot do

Stated in the tool's own output rather than left to be discovered:

- **`Stop` does not fire on an interrupt.** An API error fires `StopFailure`; a
  killed process fires nothing. A prompt with no matching completion is normal,
  and a count of completed turns is a floor, not a total.
- **Hooks on one event run in parallel**, and order across event types is not
  guaranteed, so nothing here depends on the spool's line order.
- **A capture that failed is recorded, not silently lost.** `cclogger hook`
  exits 0 on every internal error — Claude Code treats a hook error as one, and
  on `PreToolUse` and `UserPromptSubmit` a failing hook blocks the action, so a
  telemetry tool must never be able to interrupt your work. Every swallowed
  error leaves a line in the spool instead, which `import` turns into a
  `source.gap` marker carrying the reason.

## The spool holds metadata and never content

`UserPromptSubmit` carries `prompt_text`, `Stop` carries
`last_assistant_message`, and the tool events carry `tool_input` and
`tool_response`. The receiver copies a fixed allowlist of identity and metadata
fields out of each payload and drops everything else, so no file on disk ever
holds your prompts by way of this path — not even transiently. What it does keep
(`cwd`, and so your username) is owner-only, 0600 under a 0700 directory.

It is fast because it has to be: Claude Code runs command hooks synchronously
and waits. The receiver appends one line and exits, opening no database and
reading nothing else. Measured on a 250 KB payload, end to end including process
spawn: ~3 ms median.

## Rotation

The spool is never truncated, so it grows: about 200 bytes per event, and
`import` advances past what it has committed rather than deleting it. Rotating
it is not automated yet — for now, a spool you no longer want history from can
be deleted, and the next `import` starts over from what is there.
