#!/usr/bin/env bash
# PreToolUse guard: a PR in this repo must state whether it breaks the UDS API
# that edge modules use. Under squash merge the PR title becomes the commit
# subject, and the release notes collect breaking changes from the "!" in it.
# Any internal error lets the command run.
set -u

NOT_BREAKING_MARKER="UDS_API_BREAKING=no"

repo_name() {
    local url
    url=$(git -C "$1" remote get-url upstream 2>/dev/null || git -C "$1" remote get-url origin 2>/dev/null) || return 1
    basename "${url%.git}"
}

input=$(cat)
cmd=$(jq -r '.tool_input.command // empty' <<<"$input" 2>/dev/null) || exit 0
[[ $cmd =~ gh[[:space:]]+pr[[:space:]]+(create|edit) ]] || exit 0
sub=${BASH_REMATCH[1]}
title_re='(--title|-t)[=[:space:]]+("([^"]*)"|'"'"'([^'"'"']*)'"'"')'
if [[ $cmd =~ $title_re ]]; then
    title=${BASH_REMATCH[3]}${BASH_REMATCH[4]}
elif [ "$sub" = edit ]; then
    exit 0
else
    title=""
fi

dir=$(jq -r '.cwd // empty' <<<"$input" 2>/dev/null)
[ -d "$dir" ] || dir=$PWD
if [[ $cmd =~ (^|[;&|[:space:]])cd[[:space:]]+([^[:space:];&|]+) ]]; then
    d=${BASH_REMATCH[2]/#\~/$HOME}
    [[ $d = /* ]] || d=$dir/$d
    [ -d "$d" ] && dir=$d
fi

# The hook runs on every Bash call of the session; a PR for another repo is not ours.
project_repo=$(repo_name "${CLAUDE_PROJECT_DIR:-}") || exit 0
if [[ $cmd =~ (-R|--repo)[=[:space:]]+([^[:space:]]+) ]]; then
    repo=${BASH_REMATCH[2]##*/}
else
    repo=$(repo_name "$dir") || exit 0
fi
[[ $repo == "$project_repo" ]] || exit 0

decide() { # $1 = deny|ask, $2 = reason
    jq -n --arg d "$1" --arg r "$2" \
        '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:$d,permissionDecisionReason:$r}}'
    exit 0
}

base_branch=main
[[ $cmd =~ (-B|--base)[=[:space:]]+([^[:space:]]+) ]] && base_branch=${BASH_REMATCH[2]}
base_ref=""
for r in upstream origin; do
    git -C "$dir" rev-parse -q --verify "$r/$base_branch" >/dev/null && { base_ref=$r/$base_branch; break; }
done
[ -n "$base_ref" ] || exit 0
base=$(git -C "$dir" merge-base "$base_ref" HEAD 2>/dev/null) || exit 0
git -C "$dir" diff --quiet "$base" HEAD -- src || touches_src=1
[ -n "${touches_src:-}" ] || exit 0

if [[ $title =~ ^[a-z]+(\([^\)]*\))?!: ]]; then
    old=$(git -C "$dir" show "$base:Cargo.toml" 2>/dev/null | sed -nE 's/^version *= *"([0-9]+)\.([0-9]+)\..*/\1 \2/p' | head -1)
    new=$(git -C "$dir" show "HEAD:Cargo.toml" 2>/dev/null | sed -nE 's/^version *= *"([0-9]+)\.([0-9]+)\..*/\1 \2/p' | head -1)
    [ -n "$old" ] && [ -n "$new" ] || exit 0
    read -r old_major old_minor <<<"$old"
    read -r new_major new_minor <<<"$new"
    if [ "$old_major" = 0 ] && [ "$new_major" = 0 ] && [ "$new_minor" -le "$old_minor" ]; then
        decide deny "$repo: the PR title marks a breaking UDS API change, but Cargo.toml has no minor bump ($old_major.$old_minor → $new_major.$new_minor). A breaking change needs at least a minor bump while on 0.x."
    fi
    if [ "$old_major" != 0 ] && [ "$new_major" -le "$old_major" ]; then
        decide deny "$repo: the PR title marks a breaking UDS API change, but Cargo.toml has no major bump ($old_major → $new_major)."
    fi
    exit 0
fi

[[ $cmd == *"$NOT_BREAKING_MARKER"* ]] && exit 0

decide deny "$repo: edge modules use its UDS API, and this PR changes src/. Check the diff against $base_ref for a breaking change of that API: a changed or removed route or method, a changed request or response body, a renamed or removed field, publish channel or status key, or a changed field type or shape (a new optional field is not breaking).
- Breaking: put \"!\" in the PR title (e.g. \"feat!: ...\"), bump at least the minor version while on 0.x, and describe the old and new format in the PR description. The release notes pick it up from the title.
- Not breaking: run the command again with the prefix $NOT_BREAKING_MARKER and tell the user why it is not breaking."
