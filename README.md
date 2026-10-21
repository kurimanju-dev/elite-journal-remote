# Elite Journal Relay

Copies Elite Dangerous journals to another computer. The receiver writes local files for companion apps. There's a desktop app and a CLI for macOS, Linux and Windows.

## Install and build

Get your OS/CPU's installer or CLI archive from [prereleases](https://github.com/kurimanju-dev/elite-journal-remote/releases). Downloads include `SHA256SUMS`. macOS bundles use ad-hoc signing and aren't notarized. Other packages are unsigned.

Use the same release or pairing key on both devices. Public builds share an extractable key. Create your own key on both ends for private access.

For the CLI, install Rust and run `cargo build --locked`. GUI builds also need Node.js, Python 3.11+ and the OS's Tauri prerequisites:

```sh
npm ci
python3 scripts/build.py
```

Build options: `--target TRIPLE`, `--cli-only`, `--debug`, `--matrix`. CI builds ARM64/x64 packages on native runners. Windows ARM needs MSVC ARM64 tools and LLVM. Linux needs WebKitGTK 4.1, GTK and packaging tools. Install with `apt install ./FILE.deb`, `dnf install ./FILE.rpm` or `pacman -U ./FILE.pkg.tar.zst`. Run `makepkg` as a regular user.

## Run it

On the game machine choose **Send** and the journal folder. On the other machine choose **Receive**, enter `auto` or the sender's address, and pick the companion's journal folder. Windows defaults to Saved Games. Linux detects Wine/Proton profiles; macOS also checks Highball/CrossOver bottles. Browse to a prefix if needed.

Click **Start**. One relay per window; stop before switching roles. Settings are saved per user. Closing the app stops it.

The receiver connects to the sender. Allow the sender through its firewall. For a host/VM pair, use shared networking; no port forwarding is needed:

- UTM: **Shared Network** (or **macOS Shared Network**), **Isolate Guest from Host** off.
- Parallels: **Devices → Network → Shared Network**.

Discovery stays local. For other routes, enter a reachable address. Plain TCP authenticates but does not encrypt journals: use a trusted network, an encrypted tunnel such as Tailscale, or TLS on both ends. TLS does not handle NAT traversal.

For TLS, create files in the sender's Advanced settings. Copy only `server-cert.pem` to the receiver and select it there. Keep `server-key.pem` on the sender. Use an explicit address and matching certificate name.

## CLI

```sh
elite-journal-relay serve --source '/game/journals'
elite-journal-relay receive --connect auto --wine-prefix '/path/to/prefix'
```

Run `--help` or `help COMMAND` for options and defaults. `prefixes` prints profiles as JSON; `doctor` shows folders and architecture. Logging uses `--verbose`, `--log-file PATH` or `RUST_LOG`; version is `-V`.

Both roles take `--key-file PATH`. Create one with `keygen --out PATH` (won't overwrite). For TLS use `tls-keygen --out-dir PATH [--name NAME]`; the directory must be new and the name defaults to `elite-relay`.

## Problems and limits

Can't connect: check the sender, bind address, route and firewall. Test port 48933 with `Test-NetConnection SERVER -Port 48933` or `nc -vz SERVER 48933`. Multicast won't cross routers; try the IP. Authentication errors mean checking builds/keys. For TLS, check trust and certificate names.

Missing Wine profile: initialize it, rescan or browse. Set the destination directly for redirected Saved Games folders. If a companion ignores updates, check its path and start it after sync. Overwrite gives native notifications; atomic replacement needs rename-aware apps and can be blocked by open files.

Source files stay read-only. Events send updates; scans repair missed changes. Windows also polls open handles. Incomplete lines wait until complete. Check that source files are changing and try verbose logs if stuck.

Deleted files stay on the receiver. Conflicts get a `.relay-conflict-*.bak` backup. Supports four clients and root-level files up to 16 MiB. Content overwritten while offline cannot be recovered. Companion apps decide when to read updates.

MIT license; see LICENSE.
