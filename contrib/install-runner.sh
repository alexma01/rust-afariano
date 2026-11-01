#!/bin/sh
# Installs the rust-afariano runner from the GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/frank10gm/rust-afariano/main/contrib/install-runner.sh | sh
#   VERSION=0.1.0 sh install-runner.sh        # a specific release instead of the latest
#
# Downloads rust-afariano-runner for this OS and CPU, checks it against the
# release's SHA256SUMS, and installs it to /usr/local/bin (when writable or run
# as root) or ~/.local/bin.
set -eu

REPO="frank10gm/rust-afariano"
NAME="rust-afariano-runner"

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) echo "unsupported OS: $(uname -s) (Linux and macOS only)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) echo "unsupported CPU: $(uname -m) (x86_64 and aarch64 only)" >&2; exit 1 ;;
esac
if [ "$os" = macos ] && [ "$arch" = x86_64 ]; then
  echo "no prebuilt runner for Intel Macs: build it with 'cargo build --release -p $NAME'" >&2
  exit 1
fi

if [ -n "${VERSION:-}" ]; then
  base="https://github.com/$REPO/releases/download/v${VERSION#v}"
else
  base="https://github.com/$REPO/releases/latest/download"
fi
asset="$NAME-$os-$arch"

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  echo "curl or wget is required" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  echo "sha256sum or shasum is required" >&2
  exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $asset"
fetch "$base/$asset" "$tmp/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
expected=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
actual=$(sha256 "$tmp/$asset")
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  echo "checksum mismatch for $asset (expected ${expected:-nothing}, got $actual)" >&2
  exit 1
fi
chmod 755 "$tmp/$asset"

if [ "$(id -u)" = 0 ] || [ -w /usr/local/bin ]; then
  dir=/usr/local/bin
else
  dir="$HOME/.local/bin"
  mkdir -p "$dir"
fi
mv "$tmp/$asset" "$dir/$NAME"
echo "Installed $dir/$NAME ($("$dir/$NAME" --version 2>/dev/null || echo unknown version))"

case ":$PATH:" in
  *":$dir:"*) cmd="$NAME" ;;
  *) cmd="$dir/$NAME"; echo "Note: $dir is not in PATH." ;;
esac
sudo_hint=""
if [ "$os" = linux ] && [ "$(id -u)" != 0 ]; then
  sudo_hint="sudo "
fi
cat <<EOF

Next, with the token shown when you created the runner (Runners → New runner):

  ${sudo_hint}$cmd register --url https://git.example.com --token rfr_… --executor docker
  ${sudo_hint}$cmd install

Use --executor shell to run jobs directly on this machine, and --privileged for
docker-in-docker services. On macOS run both commands as the user that runs
Docker Desktop, without sudo.
EOF
