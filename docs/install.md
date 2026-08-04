# Install, in detail

Apple Silicon, Intel Mac, or x86_64 Linux. **Windows is not supported** — the
archive relies on POSIX file permissions, so there is no Windows build.

## With Node or bun

You already have one of these — Claude Code ships on npm. No Rust needed.

```bash
npm install -g cclogger      # or: bun install -g cclogger
```

**This downloads a prebuilt binary during install**, from that version's GitHub
Release, rather than shipping one package per platform. So `npm install` needs
network at install time, and it will fail behind an offline mirror or a registry
proxy that does not also let GitHub through. It reads `HTTPS_PROXY` /
`https_proxy` if you have one set.

bun blocks a dependency's `postinstall` unless you trust it, so with
`bun install -g` the download happens on **first run** instead — same result,
one slow first command.

### Trying it without installing

```bash
bunx cclogger archive && bunx cclogger import && bunx cclogger report
# or: npx cclogger archive && npx cclogger import && npx cclogger report
```

Three commands rather than one because `report` reads a ledger, and on a machine
that has never run the first two there is no ledger to read — it would print
nothing, and that would not mean nothing happened.

Note that this is not a read-only peek: `archive` and `import` write to
`~/.cclog`.

**And it is a trial, not the intended mode.** `archive` only saves what exists
when it runs and wants to be on a schedule, and a scheduled job should call an
installed binary at an absolute path rather than re-resolve a package every run
— see [scheduling.md](scheduling.md).

## Without Node or Rust

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/uooooo/cclogger/releases/latest/download/cclogger-cli-installer.sh | sh
```

Installs to `$CARGO_HOME/bin`, or `~/.cargo/bin` if that is unset — the same
place `cargo install` would put it, created if you have no Rust.

If you would rather not pipe a script into a shell, the release page has the
plain `.tar.xz` for each platform; each holds one `cclogger` binary, and putting
it on your `PATH` is the whole install.

The assets there are named `cclogger-cli-*` because the Cargo package is
`cclogger-cli` while the binary and the product are `cclogger`
([ADR-0003](adr/0003-name.md)).

## From source

Rust 1.88+.

```bash
cargo install --git https://github.com/uooooo/cclogger cclogger-cli
```

### `command not found: cargo`

You have Rust but `~/.cargo/bin` is not on your `PATH` — `rustup` was probably
installed with `--no-modify-path`. For this shell:

```bash
source "$HOME/.cargo/env"
```

To make it permanent, add that line to `~/.zshrc` or `~/.bashrc`. And note that
the installed binary lands in `~/.cargo/bin` too, so `cclogger` will be missing
from your `PATH` for the same reason — scheduled jobs should call it by absolute
path (see [scheduling.md](scheduling.md)).

If you do not have Rust at all: <https://rustup.rs>.

## Which build do I have?

```bash
cclogger --version
```

The question a prebuilt binary raises and a source build does not. Releases are
cut by pushing a tag: [releasing.md](releasing.md).
