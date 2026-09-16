#!/bin/sh
# swapdex installer: fetch the prebuilt binary for this platform from the latest
# GitHub release, verify its checksum, and install it. Unix only (Linux, WSL,
# macOS) - swapdex manages 0600 credential files.
#
#   curl -fsSL https://raw.githubusercontent.com/youdie006/swapdex/main/install.sh | sh
#   curl -fsSL .../install.sh | INSTALL_DIR=/usr/local/bin sh
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
command -v awk >/dev/null 2>&1 || fail "awk is required"
command -v sed >/dev/null 2>&1 || fail "sed is required"
command -v mktemp >/dev/null 2>&1 || fail "mktemp is required"
if command -v sha256sum >/dev/null 2>&1; then
  hasher=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  hasher=shasum
else
  fail "sha256sum or shasum is required to verify the download"
fi

tmp=$(mktemp -d) || fail "could not create a temporary directory"
stage=""
cleanup() {
  rm -rf "$tmp"
  [ -z "$stage" ] || rm -f "$stage"
}
trap cleanup 0
trap 'exit 1' 1 2 15

echo "swapdex: downloading ${target} ..." >&2
curl -fsSL "${base}/swapdex-${target}.tar.gz" -o "${tmp}/swapdex.tar.gz" || fail "download failed"
# The release always publishes the checksum; a missing one means a bad download
# or a tampered mirror, so fail rather than silently skip verification.
curl -fsSL "${base}/swapdex-${target}.sha256" -o "${tmp}/swapdex.sha256" || fail "checksum file download failed"

[ -s "${tmp}/swapdex.sha256" ] || fail "checksum file was empty or missing"
want=$(awk 'NR == 1 { print tolower($1); exit }' "${tmp}/swapdex.sha256") \
  || fail "could not read the checksum file"
[ "${#want}" -eq 64 ] || fail "checksum file did not contain a valid SHA-256"
case "$want" in
  *[!0-9a-f]*) fail "checksum file did not contain a valid SHA-256" ;;
esac

if [ "$hasher" = sha256sum ]; then
  sum_output=$(sha256sum "${tmp}/swapdex.tar.gz") \
    || fail "sha256sum could not verify the download"
else
  sum_output=$(shasum -a 256 "${tmp}/swapdex.tar.gz") \
    || fail "shasum could not verify the download"
fi
got=$(printf '%s\n' "$sum_output" | awk 'NR == 1 { print tolower($1); exit }') \
  || fail "could not read the calculated checksum"
[ "${#got}" -eq 64 ] || fail "$hasher returned an invalid SHA-256"
case "$got" in
  *[!0-9a-f]*) fail "$hasher returned an invalid SHA-256" ;;
esac
[ "$want" = "$got" ] || fail "checksum mismatch (expected $want, got $got)"

tar -xzf "${tmp}/swapdex.tar.gz" -C "${tmp}" || fail "extraction failed"
[ -f "${tmp}/swapdex" ] || fail "the archive did not contain the binary"

mkdir -p "$BINDIR" || fail "could not create the install directory"
destination="${BINDIR}/swapdex"
[ ! -d "$destination" ] \
  || fail "install destination '$destination' is a directory"
# Stage on the destination filesystem. A successful rename below is then one
# atomic replacement, so a bad candidate or interrupted copy leaves any prior
# working binary untouched.
stage=$(mktemp "${BINDIR}/.swapdex.XXXXXX") \
  || fail "could not create a staging file in the install directory"
if command -v install >/dev/null 2>&1 \
  && install -m 0755 "${tmp}/swapdex" "$stage" 2>/dev/null; then
  :
else
  cp "${tmp}/swapdex" "$stage" || fail "could not stage the downloaded binary"
  chmod 0755 "$stage" || fail "could not make the staged binary executable"
fi
version=$("$stage" --version 2>/dev/null) \
  || fail "downloaded binary could not run --version; the prior install was kept"
[ -n "$version" ] \
  || fail "downloaded binary returned an empty --version; the prior install was kept"
mv -f "$stage" "$destination" || fail "could not replace the installed binary"
stage=""

echo "swapdex: installed to $destination" >&2
case ":${PATH}:" in
  *":${BINDIR}:"*) ;;
  *)
    quoted_bindir=$(printf '%s' "$BINDIR" | sed "s/'/'\\\\''/g")
    printf '%s\n' \
      "swapdex: add it to your PATH -> export PATH='${quoted_bindir}':\"\$PATH\"" >&2
    ;;
esac
printf '%s\n' "$version"
