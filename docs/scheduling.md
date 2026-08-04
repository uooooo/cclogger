# Running `cclogger archive` on a schedule

`cclogger archive` saves what exists **when it runs**. A session created and deleted between two runs is gone. There is no daemon yet; a scheduled run is what closes the gap.

**How wide that gap can safely be depends on a setting you control.** Claude Code deletes transcripts after `cleanupPeriodDays`, which defaults to 30 and has a minimum of 1 ([settings](https://code.claude.com/docs/en/settings.md)). At the default, once a day leaves a wide margin. If you have raised it, deletion is not your reason to schedule this at all — keeping the ledger current is. If you have lowered it, raise the frequency to match, and read the cost note at the end first.

Codex keeps its own sessions; it does not delete them on a timer.

## macOS (launchd)

**1. Install the binary somewhere stable.**

launchd cannot run `cargo run`, which needs the source tree.

```bash
cargo install --path crates/cclogger-cli
```

The shell installer in the [README](../README.md#without-node-or-rust) lands in the same place, so the plist below is right either way. If `rustup` was installed with `--no-modify-path`, `~/.cargo/bin` is not on the `PATH` that launchd gives a job — which is why the plist uses the absolute path rather than the bare command name.

**If you installed from npm**, the `cclogger` on your `PATH` is a Node wrapper that spawns the real binary out of `node_modules`. It works from a job, but it costs a Node process per run and it moves whenever npm's global prefix does. `command -v cclogger` tells you which one you have; prefer one of the two above for anything scheduled.

**2. Write the agent and load it.**

No plist is committed here: it would have to contain an absolute path with your username in it. Generate it so `$HOME` expands on your machine.

```bash
mkdir -p "$HOME/.cclog" "$HOME/Library/LaunchAgents"
cat > "$HOME/Library/LaunchAgents/dev.cclogger.archive.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.cclogger.archive</string>
    <key>ProgramArguments</key>
    <array>
        <string>$HOME/.cargo/bin/cclogger</string>
        <string>archive</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>9</integer>
        <key>Minute</key>
        <integer>15</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>$HOME/.cclog/archive.log</string>
    <key>StandardErrorPath</key>
    <string>$HOME/.cclog/archive.err.log</string>
</dict>
</plist>
PLIST

launchctl bootstrap gui/$(id -u) "$HOME/Library/LaunchAgents/dev.cclogger.archive.plist"
```

`StartCalendarInterval` is a time of day, not a fixed interval. If the machine is asleep at that time, launchd runs the job as soon as it wakes — unlike an interval job, it does not simply skip.

**3. Check it, and stop it.**

```bash
launchctl list | grep cclogger                            # loaded? PID and last exit code
launchctl kickstart -k gui/$(id -u)/dev.cclogger.archive  # run once now
cat "$HOME/.cclog/archive.log"                          # scanned / archived counts
cat "$HOME/.cclog/archive.err.log"                      # should be empty

launchctl bootout gui/$(id -u)/dev.cclogger.archive        # unload
rm "$HOME/Library/LaunchAgents/dev.cclogger.archive.plist"
```

**Always log stdout and stderr somewhere.** launchd does not tell you when a job fails; the log file is the only trace.

## Linux (systemd user timer)

```bash
mkdir -p ~/.config/systemd/user

cat > ~/.config/systemd/user/cclogger-archive.service <<'UNIT'
[Unit]
Description=Archive AI session transcripts before the vendor deletes them

[Service]
Type=oneshot
ExecStart=%h/.cargo/bin/cclogger archive
UNIT

cat > ~/.config/systemd/user/cclogger-archive.timer <<'UNIT'
[Unit]
Description=Daily cclogger archive

[Timer]
OnCalendar=*-*-* 09:15:00
Persistent=true

[Install]
WantedBy=timers.target
UNIT

systemctl --user daemon-reload
systemctl --user enable --now cclogger-archive.timer
```

`Persistent=true` is the equivalent of launchd's catch-up behaviour: a run missed while the machine was off happens at the next boot.

```bash
systemctl --user list-timers cclogger-archive.timer
journalctl --user -u cclogger-archive.service -n 20
```

Note that a user timer only runs while the user has a session, unless lingering is enabled (`loginctl enable-linger $USER`).

## What a run costs

`cclogger archive` reads every file it scans and hashes it with SHA-256 every time. Unchanged files are not rewritten, but they are still read and hashed. "Cheap" means no writes, not no work — the scan covers the whole corpus, which reaches several gigabytes quickly.

A measured run over a 3.3 GB corpus: `scanned 1585 / archived 13 / unchanged 1572`.

Take that into account before raising the frequency. Daily is already comfortably inside the retention window.
