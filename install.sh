#!/bin/sh
# Installs the latest prebuilt Fizz for this computer.
#
#   curl -fsSL https://raw.githubusercontent.com/eehkay/fizz/main/install.sh | sh
#
# It downloads one program, checks it against its published checksum, and
# puts it in ~/.local/bin (or the folder named in FIZZ_INSTALL_DIR). It
# needs no administrator rights and installs nothing else.

set -eu

repo="eehkay/fizz"
dest="${FIZZ_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64 | Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  *)
    echo "There is no prebuilt Fizz for $(uname -s) on $(uname -m)." >&2
    echo "It can still be built from source: https://github.com/$repo#from-source" >&2
    exit 1
    ;;
esac

name="fizz-$target.tar.gz"
url="https://github.com/$repo/releases/latest/download/$name"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "Downloading Fizz for $target…"
curl -fsSL "$url" -o "$work/$name"
curl -fsSL "$url.sha256" -o "$work/$name.sha256"

expected="$(cut -d' ' -f1 "$work/$name.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$work/$name" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$work/$name" | cut -d' ' -f1)"
fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  echo "The download did not match its checksum, so nothing was installed." >&2
  exit 1
fi

tar -xzf "$work/$name" -C "$work"
mkdir -p "$dest"
# Move into place in one step, so a running Fizz is never half-replaced.
cp "$work/fizz" "$dest/.fizz.new"
chmod 755 "$dest/.fizz.new"
mv "$dest/.fizz.new" "$dest/fizz"

echo "Installed $("$dest/fizz" --version) to $dest/fizz"
case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "Note: $dest is not on your PATH. Add it, or run $dest/fizz directly." ;;
esac
echo
echo "Next: run 'fizz setup' to connect it to your BlueBubbles server,"
echo "or 'fizz --demo' to look around first."
