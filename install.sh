#!/bin/sh
# swapdex installer: fetch the prebuilt binary for this platform from the latest
# GitHub release, verify its checksum, and install it. Unix only (Linux, WSL,
# macOS) - swapdex manages 0600 credential files.
#
#   curl -fsSL https://raw.githubusercontent.com/youdie006/swapdex/main/install.sh | sh
#   INSTALL_DIR=/usr/local/bin curl -fsSL .../install.sh | sh
#
# Prefer `cargo install swapdex`, `brew install youdie006/tap/swapdex`, or
# `npm i -g @youdie006/swapdex` if you use those.

set -eu

REPO="youdie006/swapdex"
BINDIR="${INSTALL_DIR:-$HOME/.local/bin}"

fail() {
  echo "swapdex: $1" >&2
  echo "swapdex: install another way instead -> cargo install swapdex" >&2
  exit 1
}

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux) suffix="unknown-linux-musl" ;;
  Darwin) suffix="apple-darwin" ;;
  *) fail "unsupported OS '$os' (Linux, WSL, and macOS only)" ;;
esac
case "$arch" in
  x86_64 | amd64) cpu="x86_64" ;;
  aarch64 | arm64) cpu="aarch64" ;;
  *) fail "unsupported architecture '$arch'" ;;
esac
target="${cpu}-${suffix}"
base="https://github.com/${REPO}/releases/latest/download"

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "swapdex: downloading ${target} ..." >&2
curl -fsSL "${base}/swapdex-${target}.tar.gz" -o "${tmp}/swapdex.tar.gz" || fail "download failed"
curl -fsSL "${base}/swapdex-${target}.sha256" -o "${tmp}/swapdex.sha256" 2>/dev/null || true

# Verify the checksum when both the sums file and a hasher are available.
if [ -s "${tmp}/swapdex.sha256" ]; then
  want=$(awk '{print $1}' "${tmp}/swapdex.sha256")
  if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "${tmp}/swapdex.tar.gz" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then
    got=$(shasum -a 256 "${tmp}/swapdex.tar.gz" | awk '{print $1}')
  else
    got=""
  fi
  if [ -n "$got" ] && [ "$want" != "$got" ]; then
    fail "checksum mismatch (expected $want, got $got)"
  fi
fi

tar -xzf "${tmp}/swapdex.tar.gz" -C "${tmp}" || fail "extraction failed"
[ -f "${tmp}/swapdex" ] || fail "the archive did not contain the binary"

mkdir -p "$BINDIR"
if install -m 0755 "${tmp}/swapdex" "${BINDIR}/swapdex" 2>/dev/null; then
  :
else
  cp "${tmp}/swapdex" "${BINDIR}/swapdex" && chmod 0755 "${BINDIR}/swapdex"
fi

echo "swapdex: installed to ${BINDIR}/swapdex" >&2
case ":${PATH}:" in
  *":${BINDIR}:"*) ;;
  *) echo "swapdex: add it to your PATH -> export PATH=\"${BINDIR}:\$PATH\"" >&2 ;;
esac
"${BINDIR}/swapdex" --version 2>/dev/null || true
