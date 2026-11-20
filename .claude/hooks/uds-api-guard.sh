#!/usr/bin/env bash
# PreToolUse guard: a PR in this repo must state whether it breaks the UDS API
# that edge modules use. Under squash merge the PR title becomes the commit
# subject, and the release notes collect breaking changes from the "!" in it.
# Any internal error lets the command run, so only `set -u`.
set -u

NOT_BREAKING_MARKER="UDS_API_BREAKING=no"
# Files that define the UDS API (README.md, "Breaking changes").
API_PATHS=(":(top)src" ":(top)systemd/*.socket")
GH_TIMEOUT_S=15

repo_name() {
    local url
    url=$(git -C "$1" remote get-url upstream 2>/dev/null || git -C "$1" remote get-url origin 2>/dev/null) || return 1
    basename "${url%.git}"
}

unquote() {
    local v=$1
    v=${v#[\"\']}
    v=${v%[\"\']}
    printf '%s' "$v"
}

input=$(cat)
cmd=$(jq -r '.tool_input.command // empty' <<<"$input" 2>/dev/null) || exit 0
# A gh call at the start of a command, optionally after env assignments.
gh_re='(^|[;&|(`[:cntrl:]])[[:space:]]*(([A-Za-z_][A-Za-z0-9_]*=[^[:space:];&|]*[[:space:]]+)*)gh[[:space:]]+pr[[:space:]]+(create|new|edit)([[:space:]]|$)'
[[ "$cmd" =~ $gh_re ]] || exit 0
env_prefix=${BASH_REMATCH[2]}
sub=${BASH_REMATCH[4]}
before=${cmd%%"${BASH_REMATCH[0]}"*}
rest=${cmd#*"${BASH_REMATCH[0]}"}
val_re='("[^"]*"|'"'"'[^'"'"']*'"'"'|[^[:space:];&|]+)'

if [[ "$rest" =~ (^|[[:space:]])(--title|-t)[=[:space:]]+$val_re ]]; then
    title=$(unquote "${BASH_REMATCH[3]}")
elif [[ "$sub" == edit ]]; then
    exit 0
else
    title=""
fi

dir=$(jq -r '.cwd // empty' <<<"$input" 2>/dev/null)
[[ -d "$dir" ]] || dir=$PWD
if [[ "$before" =~ (^|[;&|[:space:]])cd[[:space:]]+$val_re ]]; then
    d=$(unquote "${BASH_REMATCH[2]}")
    d=${d/#\~/$HOME}
    [[ "$d" == /* ]] || d=$dir/$d
    [[ -d "$d" ]] && dir=$d
fi

repo_flag=()
# The hook runs on every Bash call of the session; a PR for another repo is not ours.
project_repo=$(repo_name "${CLAUDE_PROJECT_DIR:-}") || exit 0
if [[ "$rest" =~ (^|[[:space:]])(-R|--repo)[=[:space:]]+$val_re ]]; then
    repo_arg=$(unquote "${BASH_REMATCH[3]}")
    repo_flag=(--repo "$repo_arg")
    repo=${repo_arg%.git}
    repo=${repo##*/}
else
    repo=$(repo_name "$dir") || exit 0
fi
[[ "$repo" == "$project_repo" ]] || exit 0

decide() { # $1 = deny|ask, $2 = reason
    jq -n --arg d "$1" --arg r "$2" \
        '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:$d,permissionDecisionReason:$r}}'
    exit 0
}

head=HEAD
base_branch=main
# "gh pr edit <pr>" can name a PR of another branch than the checked out one.
if [[ "$sub" == edit && "$rest" =~ ^[[:space:]]*([^-[:space:]][^[:space:];&|]*) ]]; then
    pr_arg=$(unquote "${BASH_REMATCH[1]}")
    pr=$(cd "$dir" && timeout "$GH_TIMEOUT_S" gh pr view "$pr_arg" "${repo_flag[@]}" \
        --json headRefOid,baseRefName -q '.headRefOid + " " + .baseRefName' 2>/dev/null) || exit 0
    read -r head base_branch <<<"$pr"
    [[ -n "$head" && -n "$base_branch" ]] || exit 0
    git -C "$dir" cat-file -e "$head^{commit}" 2>/dev/null || exit 0
fi
if [[ "$rest" =~ (^|[[:space:]])(-B|--base)[=[:space:]]+$val_re ]]; then
    base_branch=$(unquote "${BASH_REMATCH[3]}")
fi

base_ref=""
for r in upstream origin; do
    git -C "$dir" rev-parse -q --verify "$r/$base_branch" >/dev/null && { base_ref=$r/$base_branch; break; }
done
[[ -n "$base_ref" ]] || exit 0
base=$(git -C "$dir" merge-base "$base_ref" "$head" 2>/dev/null) || exit 0
git -C "$dir" diff --quiet --no-ext-diff --no-textconv "$base" "$head" -- "${API_PATHS[@]}"
[[ "$?" -eq 1 ]] || exit 0

if [[ "$title" =~ ^[a-z]+(\([^\)]*\))?!: ]]; then
    old=$(git -C "$dir" show "$base_ref:Cargo.toml" 2>/dev/null | sed -nE 's/^version *= *"([0-9]+)\.([0-9]+)\..*/\1 \2/p' | head -1)
    new=$(git -C "$dir" show "$head:Cargo.toml" 2>/dev/null | sed -nE 's/^version *= *"([0-9]+)\.([0-9]+)\..*/\1 \2/p' | head -1)
    [[ -n "$old" && -n "$new" ]] || exit 0
    read -r old_major old_minor <<<"$old"
    read -r new_major new_minor <<<"$new"
    if [[ "$old_major" == 0 && "$new_major" == 0 ]] && ((new_minor <= old_minor)); then
        decide deny "$repo: the PR title marks a breaking UDS API change, but Cargo.toml has no minor bump ($old_major.$old_minor → $new_major.$new_minor). A breaking change needs at least a minor bump while on 0.x."
    fi
    if [[ "$old_major" != 0 ]] && ((new_major <= old_major)); then
        decide deny "$repo: the PR title marks a breaking UDS API change, but Cargo.toml has no major bump ($old_major → $new_major)."
    fi
    exit 0
fi

[[ " $env_prefix" =~ [[:space:]]"$NOT_BREAKING_MARKER"[[:space:]] ]] && exit 0

decide deny "$repo: edge modules use its UDS API, and this PR changes its files (src/ or the socket unit). Read README.md, section \"Breaking changes\", and check the diff against $base_ref for each case listed there.
- Breaking: put \"!\" in the PR title (e.g. \"feat!: ...\"), bump at least the minor version while on 0.x, and describe the old and new format in the PR description. The release notes pick it up from the title.
- Not breaking: run the command again with the prefix $NOT_BREAKING_MARKER directly before \"gh\" and tell the user why it is not breaking."
