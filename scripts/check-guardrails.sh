#!/usr/bin/env bash
# Project-specific architecture checks for nntp-proxy.
#
# This intentionally checks added lines rather than the full historical tree:
# the repository already contains some older exceptions, and this gate exists
# to keep new work from spreading those patterns.

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

base_ref="${GUARDRAILS_BASE_REF:-}"
if [ -z "$base_ref" ]; then
    if [ -n "${GITHUB_BASE_REF:-}" ] && git rev-parse --verify "origin/${GITHUB_BASE_REF}" >/dev/null 2>&1; then
        base_ref="origin/${GITHUB_BASE_REF}"
    elif git rev-parse --verify origin/main >/dev/null 2>&1; then
        base_ref="origin/main"
    elif git rev-parse --verify main >/dev/null 2>&1; then
        base_ref="main"
    elif [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "error: could not resolve guardrail base ref in GitHub Actions" >&2
        exit 2
    else
        base_ref=""
    fi
fi

diff_file="$(mktemp)"
trap 'rm -f "$diff_file"' EXIT

if [ -n "$base_ref" ]; then
    git diff --unified=0 "$base_ref"...HEAD -- src benches >>"$diff_file"
fi

git diff --cached --unified=0 -- src benches >>"$diff_file"
git diff --unified=0 -- src benches >>"$diff_file"

added_lines() {
    awk '
        /^\+\+\+ / { path = $2; next }
        /^\+/ && path != "" { print path "\t" substr($0, 2) }
    ' "$diff_file"
}

added_lines_outside_framer() {
    added_lines | awk '$1 != "b/src/session/multiline_framing.rs" { sub(/^[^\t]*\t/, ""); print }'
}

added_lines_outside_framer_and_cache() {
    added_lines | awk '$1 != "b/src/session/multiline_framing.rs" && $1 !~ /^b\/src\/cache\// { sub(/^[^\t]*\t/, ""); print }'
}

added_buffer_pool_lines() {
    added_lines | awk '$1 == "b/src/pool/buffer.rs" { sub(/^[^\t]*\t/, ""); print }'
}

failures=0

check_added() {
    local title="$1"
    local pattern="$2"
    local tmp

    tmp="$(mktemp)"
    if added_lines | rg --pcre2 "$pattern" >"$tmp"; then
        echo "error: $title"
        sed 's/^/  /' "$tmp"
        failures=$((failures + 1))
    fi
    rm -f "$tmp"
}

check_added_from() {
    local source="$1"
    local title="$2"
    local pattern="$3"
    local tmp

    tmp="$(mktemp)"
    if "$source" | rg --pcre2 "$pattern" >"$tmp"; then
        echo "error: $title"
        sed 's/^/  /' "$tmp"
        failures=$((failures + 1))
    fi
    rm -f "$tmp"
}

check_added_from added_lines_outside_framer \
    "new multiline response boundary logic must stay inside src/session/multiline_framing.rs" \
    'ends_with\s*\(|starts_with\s*\(|windows\s*\(|line\s*==\s*b?"\.\\r\\n"|terminator offset|packed response|let\s+\w+\s*=\s*&\w+\s*=\s*&\w+\s*\[\.\.'

check_added \
    "new production response status checks should use StatusCode parsing or parsed status fields" \
    'starts_with\s*\(\s*(b)?"[1-5][0-9][0-9]|==\s*(b)?"430|response_code\s*==\s*430|code\s*==\s*430'

check_added \
    "new production command classification should use src/command/classifier.rs helpers" \
    'starts_with\s*\(\s*(b)?"(ARTICLE|BODY|HEAD|STAT|XOVER|OVER|HDR|LIST|GROUP)|split_ascii_whitespace\s*\('

check_added \
    "new too_many_arguments allowances should be avoided outside grandfathered internals" \
    '#\[allow\(clippy::too_many_arguments\)\]'

check_added_from added_buffer_pool_lines \
    "new scratch socket-read buffers should not be grown with extend_from_slice in hot paths" \
    'extend_from_slice\s*\('

if [ "$failures" -ne 0 ]; then
    echo
    echo "guardrail check failed with $failures finding group(s)"
    exit 1
fi

echo "guardrail check passed"
