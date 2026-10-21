#!/usr/bin/env python3
"""Explicit opt-in: install a per-user launchd agent. Does not start it automatically."""
import argparse
from pathlib import Path
import plistlib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--key-file", type=Path, required=True)
parser.add_argument("--listen", default="127.0.0.1:48933")
parser.add_argument("--source", type=Path)
args = parser.parse_args()
binary = args.binary.expanduser().resolve(strict=True)
key_file = args.key_file.expanduser().resolve(strict=True)
label = "local.elite-journal-relay"
agents = Path.home() / "Library/LaunchAgents"
logs = Path.home() / "Library/Logs/EliteJournalRelay"
agents.mkdir(parents=True, exist_ok=True)
logs.mkdir(parents=True, exist_ok=True)
target = agents / (label + ".plist")
if target.exists():
    raise SystemExit(f"{target} exists; remove/unload it before reinstalling")
command = [str(binary), "serve", "--listen", args.listen, "--key-file", str(key_file)]
if args.source:
    command += ["--source", str(args.source.expanduser().resolve(strict=True))]
config = {"Label": label, "ProgramArguments": command, "RunAtLoad": True, "KeepAlive": True,
          "ThrottleInterval": 5, "StandardOutPath": str(logs / "server.log"), "StandardErrorPath": str(logs / "server-error.log")}
with target.open("xb") as file:
    plistlib.dump(config, file)
print(f"Created {target}")
print(f'Load: launchctl bootstrap gui/$(id -u) "{target}"')
print(f'Remove: launchctl bootout gui/$(id -u) "{target}"; then remove the plist.')
