# ectf: HSF client

Flash, debug, and claim the team's MSPM0 boards on the HSF server (RPi server in the room), from Linux or Windows.

## Install (once per client machine)

You need:
- **Rust** (rustup) and `rustup target add thumbv6m-none-eabi`
  - Windows: also the Visual Studio Build Tools (rustup offers them)
- **OpenSSH client**: preinstalled on most Linux; Windows: Settings > System > Optional features > OpenSSH Client
- **GDB** (only for debugging): Debian/Ubuntu `gdb-multiarch` · Arch `arm-none-eabi-gdb` · Fedora `gdb` · Windows: Arm GNU Toolchain
- **Tailscale** (or be on the room LAN)

```sh
cd ~          # NOT inside a firmware project (its .cargo/config.toml would break the install)
cargo install --locked --git https://github.com/NoahW227/HSF-client.git ectf-remote
ectf setup    # name, network, default board, SSH key
ectf doctor   # check everything works
```

Update: run the same `cargo install` command with `--force`.

## Set up (once per project)

Add to the project's `.cargo/config.toml` (keep your existing rustflags):

```toml
[target.'cfg(all(target_arch = "arm", target_os = "none"))']
runner = ["ectf", "flash"]

[build]
target = "thumbv6m-none-eabi"
```

## Commands

```sh
ectf                          # board status
ectf claim board2 --for 2h    # claim a board for a long session
cargo run --release           # build + flash (uses your default board)
ectf debug                    # GDB on the image you just flashed
ectf debug -- -ex 'hbreak main'
ectf release board2
```

Pick a board for one command: `ectf -b board3 debug`, or `ECTF_BOARD=board3 cargo run --release`.

Debugging: quitting GDB releases the board. Ctrl-C interrupts the target.
There are only 4 hardware breakpoints. `load` is disabled, so flash with `cargo run`.

## Settings

`ectf setup` writes your config. Environment variables override it:

| Variable | Use |
|---|---|
| `ECTF_BOARD` | board for this shell |
| `ECTF_USER` | your name |
| `ECTF_PI` / `ECTF_DASHBOARD` | e.g. `ectf@10.37.1.253` / `http://10.37.1.253:8080` on the LAN |
| `ECTF_GDB` | path to GDB if it isn't found automatically |
| `ECTF_FORCE=1` | override the board lock (or `--force`) |

## Troubleshooting

- **Anything failing**: run `ectf doctor`.
- **SSH fails but the password works**: if your key has a passphrase, run `ssh-add` (Windows: start the "OpenSSH Authentication Agent" service first).
- **"local port N is busy"**: `ectf debug --local-port 3334`.
- **Symbols don't match the board**: someone else flashed it, or you rebuilt without flashing. Run `cargo run --release` again.
