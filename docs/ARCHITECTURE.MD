# ectf HSF client: how it works

`ectf` is one binary for Linux and Windows. It replaces `remote-flash.sh`,
`remote-debug.sh` and `ectf-boards`. It is client-side only: the Pi
(`ectf.sh`, OpenOCD, the dashboard) is unchanged and is reached through the
system `ssh` and plain HTTP.

## Source layout

| File | Role |
|---|---|
| `main.rs` | CLI (clap), command dispatch, exit codes |
| `config.rs` | Settings resolution, config file, flash state file |
| `ssh.rs` | Runs system `ssh`, shell quoting, upload via `cat >` |
| `flash.rs` | ELF → .bin, image metadata, upload + flash |
| `debug.rs` | OpenOCD tunnel, GDB detection, Ctrl-C handling |
| `boards.rs` | Dashboard API: status / claim / release |
| `setup.rs` | Interactive setup, SSH key install, `doctor` |

## Settings

Priority: **CLI flag > environment variable > config file > default**.

| Setting | Env var | Config key | Default |
|---|---|---|---|
| Name | `ECTF_USER` | `user` | *none: required to claim/flash/debug* |
| SSH target | `ECTF_PI` | `pi` | `ectf@100.92.231.53` (TS) `ectf@10.37.1.253` (LAN) |
| Dashboard | `ECTF_DASHBOARD` | `dashboard` | `http://100.92.231.53:8080` |
| Board | `ECTF_BOARD` / `--board` | `board` | `board1` |
| Pi script | `ECTF_REMOTE_SCRIPT` | `remote_script` | `~/ectf2027/ectf.sh` |
| GDB | `ECTF_GDB` | `gdb` | auto-detected |
| ssh binary | `ECTF_SSH` | `ssh` | `ssh` from PATH |
| Lock override | `ECTF_FORCE=1` / `--force` | — | off |
| Image label | `ECTF_IMAGE_NAME` | — | ELF file name, 8 chars |
| Tunnel port | `ECTF_LOCAL_PORT` / `--local-port` | — | same as the Pi's port |

The name is required, with no fallback to `$USER`/`%USERNAME%`. That way a
claim and a flash always carry the same name. The scripts' fallbacks didn't
match each other, which made the Pi treat your own claim as someone else's.

## Files on disk

| What | Linux | Windows |
|---|---|---|
| Config | `~/.config/ectf/config.toml` | `%APPDATA%\ectf\config.toml` |
| Flash state | `~/.local/share/ectf/state.json` | `%LOCALAPPDATA%\ectf\state.json` |
| Converted image | `<elf>.bin` next to the ELF | same |
| Debug log (temp) | `$TMPDIR/ectf-debug-<pid>.log` | `%TEMP%\...` |

`ECTF_CONFIG` overrides the config path. The state file maps
board → {ELF path, SHA-256 of the .bin, time}, for this machine only.

## Command flows

### `ectf flash <elf>` (cargo runner)
Cargo runs `ectf flash <path-to-elf> [args after --]`. Extra args are ignored.
1. Require a name. Read the ELF and convert it to `.bin` (see below), then write `<elf>.bin`.
2. Build the metadata: label (`ECTF_IMAGE_NAME`, else the file name cleaned up
   to `[A-Za-z0-9._-]` and cut to 8 chars) and
   `"<git describe --always --dirty | nogit> sha256:<12 hex>"`.
3. Upload with `ssh pi "cat > /tmp/ectf-<user>-<board>-<pid>-<nanos>.bin"`,
   streaming the bytes over stdin. The unique name stops two people's
   uploads from overwriting each other.
4. Run on the Pi, with `-t` if stdin is a terminal:
   `ECTF_USER=.. ECTF_IMAGE=.. ECTF_IMAGE_NAME=.. [ECTF_FORCE=1] <script> <board> flash <bin>; rc=$?; rm -f <bin>; exit $rc`
5. On success, record the flash in the state file. Exit with ssh's exit code.

### `ectf debug [elf] [-- gdb args]`
1. ELF = the argument, else the last ELF flashed to this board from this machine.
2. Find GDB **before** touching the Pi: `ECTF_GDB`/config, else the first of
   `gdb-multiarch`, `arm-none-eabi-gdb`, `gdb` on PATH that accepts
   `set architecture armv6-m`.
3. Warn if the ELF's image hash differs from the last recorded flash.
4. Install the Ctrl-C/termination handler (see below).
5. `ssh pi "<script> <board> gdbport"` → remote port.
6. Spawn in its own process group:
   `ssh -tt -o BatchMode=yes -o ExitOnForwardFailure=yes -L <local>:127.0.0.1:<port> pi "ECTF_USER=.. [ECTF_FORCE=1] <script> <board> debug"`,
   with stdin null and stdout/stderr going to the temp log.
7. Check the log every 200 ms for `ectf: gdb server ready` (20 s timeout).
   Fail on a line starting with `error:`, on ssh exiting, or on Ctrl-C.
8. Run `gdb -q <elf> -ex "target extended-remote 127.0.0.1:<local>" <args>`.
9. A guard runs on every exit path: kill ssh. The remote pty hangs up, so
   OpenOCD exits and the board is released. Then delete the log.

### `ectf status | claim | release`
HTTP to the dashboard with a 10 s timeout. Error responses show the JSON
`error` field. `claim`/`release` default to the configured board.

### `ectf setup` / `ectf doctor`
`setup` asks for name, network and default board, then writes the config.
It then checks `ssh -o BatchMode=yes pi true`. If that fails, it creates
`~/.ssh/id_ed25519` if needed and appends the public key with
`ssh pi "umask 077; mkdir -p ~/.ssh && cat >> ~/.ssh/authorized_keys"`
(the password is prompted once). Then it checks GDB and the dashboard.
`doctor` runs the same checks without changing anything and exits 1 if
something required fails.

## What the client expects from the Pi

Changing any of these on the Pi needs a matching client change.

- SSH login with key auth as the configured `pi` user. `/tmp` must be writable.
- `<script> <board> gdbport` prints the GDB port number on stdout.
- `<script> <board> flash <file>` flashes. The exit code is passed back.
- `<script> <board> debug` starts OpenOCD on `127.0.0.1:<port>`, prints
  `ectf: gdb server ready` when ready, prints `error: ...` lines on failure,
  and exits when its pty hangs up.
- Env vars passed: `ECTF_USER`, `ECTF_IMAGE`, `ECTF_IMAGE_NAME`, `ECTF_FORCE`.
- Dashboard: `GET /api/status` → `{now, boards:[{name, claim{name,until},
  activity{action,user,since}, bootloader{version,secure}, port_present,
  gdb_port}], min_client?}`. `POST /api/claim {board,name,duration,note}` →
  `{claim:{name,until}}`. `POST /api/release {board,name,force}`. Errors
  return `{error}`.
- `min_client` (optional) makes `ectf status` warn when the client is older.

## ELF → .bin

The `.bin` has the same layout `rust-objcopy -O binary` produces for normal
cortex-m-rt builds. It takes every `PT_LOAD` segment with file data, places
it at its **physical** address (so `.data` lands at its flash load address,
not its RAM address), and fills gaps with zeros. The tool rejects non-ARM ELFs
and images over 1 MiB, which usually means a segment has a RAM load address.
To check against objcopy:
`rust-objcopy -O binary fw ref.bin && cmp ref.bin fw.bin`.

## Signals and process groups

- **The tunnel's ssh runs in a separate process group** (Unix
  `process_group(0)`, Windows `CREATE_NEW_PROCESS_GROUP`). Ctrl-C in the
  terminal then reaches GDB, which interrupts the target, but not the tunnel.
- **`ectf` itself never dies from Ctrl-C/SIGTERM/SIGHUP during `debug`**
  (`ctrlc` crate with `termination`). Before GDB starts, the signal cancels
  the wait. While GDB runs it is ignored. When GDB exits, the cleanup guard
  always runs, so OpenOCD is never left running.
- All ssh calls use `BatchMode=yes`, so a missing key fails quickly instead
  of hanging on a hidden password prompt (which would stop a
  background-process-group ssh on Unix). Only `setup` lets ssh prompt.

## Quoting and safety

Values sent to the Pi (name, image, label, paths) are single-quoted for the
POSIX shell. Board names must match `[A-Za-z0-9_-]+`. `remote_script` is
inserted unquoted so `~` expands on the Pi. It comes only from your own
config or env.

## Platform notes

- One codebase. The only OS-specific code is `detach()` in `debug.rs`.
- Windows uses the built-in OpenSSH client (`ssh.exe`, `ssh-keygen.exe`).
  Git Bash's ssh also works via `ECTF_SSH`.
- No bash, Python, coreutils, scp or WSL needed.

## Building and releasing

- `cargo build --release`. Commit `Cargo.lock`. Bump `version` when the
  Pi contract changes, and set `min_client` on the Pi if old clients would break.
- Windows MSVC builds need the Visual Studio Build Tools (rustup offers them).
- Check both OS builds from Linux: `cargo check --target x86_64-pc-windows-gnu`.

## Known limitations

- The "ELF differs from flashed image" check only knows about flashes from
  this machine, not other people's.
- With no ELF given, `debug` uses the last flash *to that board* from this
  machine, whichever project it came from.
- Flashing uses two SSH connections (upload, then flash), which costs about
  one extra connection setup.
