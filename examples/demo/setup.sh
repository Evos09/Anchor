#!/bin/sh
# Create a throwaway demo workspace with a symlink trap and an append-only
# changelog. Usage: ./setup.sh DIR
set -eu
d="$1"
mkdir -p "$d/src" "$d/fake_home/.ssh"
printf "print('hello')\n" > "$d/src/main.py"
printf "def helper():\n    return 42\n" > "$d/src/util.py"
printf "# Changelog\n\n## 2026-10-01\n- initial\n" > "$d/CHANGELOG.md"
printf "ssh-ed25519 AAAA... user@host\n" > "$d/fake_home/.ssh/authorized_keys"
ln -s "$(cd "$d" && pwd)/fake_home/.ssh" "$d/config_link"
mkdir -p "$d/ws"
mv "$d/src" "$d/CHANGELOG.md" "$d/ws/"
ln -s "$(cd "$d" && pwd)/fake_home/.ssh" "$d/ws/config_link"
rm "$d/config_link"
chmod -R a+rwX "$d"
echo "$d/ws"
