#!/bin/sh
# Build a throwaway swapdex store for the README recordings.
#
# The demos must never show a real login, so they are recorded against this
# fixture rather than the machine's own accounts: every name, email and token
# here is invented, and SWAPDEX_ROOT keeps swapdex inside $1 for the whole run.
#
# Usage: seed-demo.sh <root>
set -eu

root=${1:?usage: seed-demo.sh <root>}
rm -rf "$root"
store="$root/.local/share/swapdex"
mkdir -p "$store/accounts"

# Claude's saved snapshot for one profile: the identity `ls` reads, and a token
# far enough from expiry that no "stale" marker appears in the recording.
profile() {
  name=$1 email=$2 tier=$3 uuid=$4
  d="$store/accounts/$name/claude-code"
  mkdir -p "$d"
  exp=$(( ($(date +%s) + 86400) * 1000 ))
  printf '{"claudeAiOauth":{"accessToken":"demo-%s","refreshToken":"demo-r","subscriptionType":"%s","expiresAt":%s}}' \
    "$name" "$tier" "$exp" > "$d/credentials"
  printf '{"accountUuid":"%s","emailAddress":"%s"}' "$uuid" "$email" > "$d/oauth_account"
  chmod 600 "$d/credentials" "$d/oauth_account"
}

profile personal you@personal.dev pro u-personal
profile work     you@work.com     max u-work

# The live login the recording starts from, so `status` has something true to
# say and `use` is a real change rather than a no-op.
mkdir -p "$root/.claude"
exp=$(( ($(date +%s) + 86400) * 1000 ))
printf '{"claudeAiOauth":{"accessToken":"demo-work","refreshToken":"demo-r","subscriptionType":"max","expiresAt":%s}}' \
  "$exp" > "$root/.claude/.credentials.json"
# The identity lives beside the home, not inside .claude - `swapdex status`
# reads $HOME/.claude.json, and seeding the wrong one left the recording
# saying "Claude account" where the email belongs.
printf '{"oauthAccount":{"accountUuid":"u-work","emailAddress":"you@work.com"}}' \
  > "$root/.claude.json"
chmod 600 "$root/.claude/.credentials.json" "$root/.claude.json"

# Codex, seeded the same way, so the recording shows the two tools swapdex
# actually switches rather than three "not logged in" lines.
jwt() {
  b64() { printf '%s' "$1" | base64 -w0 | tr '+/' '-_' | tr -d '='; }
  printf '%s.%s.sig' "$(b64 '{"alg":"none"}')" "$(b64 "{\"email\":\"$1\"}")"
}
codex_profile() {
  name=$1 email=$2 acct=$3
  d="$store/accounts/$name/codex"
  mkdir -p "$d"
  printf '{"auth_mode":"chatgpt","last_refresh":"%s","tokens":{"access_token":"demo-%s","account_id":"%s","refresh_token":"demo-r","id_token":"%s"}}' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$name" "$acct" "$(jwt "$email")" > "$d/auth"
  chmod 600 "$d/auth"
}
codex_profile personal you@personal.dev acct-personal
codex_profile work     you@work.com     acct-work

mkdir -p "$root/.codex"
printf '{"auth_mode":"chatgpt","last_refresh":"%s","tokens":{"access_token":"demo-work","account_id":"acct-work","refresh_token":"demo-r","id_token":"%s"}}' \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(jwt you@work.com)" > "$root/.codex/auth.json"
chmod 600 "$root/.codex/auth.json"

echo "seeded $root"
