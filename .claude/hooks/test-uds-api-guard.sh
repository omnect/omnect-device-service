#!/usr/bin/env bash
# Runs uds-api-guard.sh, as .claude/settings.json calls it, with sample
# commands against scratch git repos. gh is a stub, so the tests need no
# network or login.
set -euo pipefail

hooks=$(cd "$(dirname "$0")" && pwd)
claude_dir=$(dirname "$hooks")
hook_cmd=$(jq -r '.hooks.PreToolUse[].hooks[] | select(.command | contains("uds-api-guard.sh")) | .command' "$claude_dir/settings.json")
CLAUDE_PROJECT_DIR="" eval "set -- $hook_cmd"
repo=$2
name=${repo#*/}
api_dir=${3#:(top)}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export GIT_CONFIG_GLOBAL=$tmp/gitconfig GIT_CONFIG_NOSYSTEM=1
git config --global user.name t
git config --global user.email t@t
git config --global init.defaultBranch main
git config --global commit.gpgsign false
git config --global advice.detachedHead false
# The remotes carry GitHub URLs, as in a real clone; git reads local repos.
git config --global "url.$tmp/omnect.git.insteadOf" "https://github.com/$repo.git"
git config --global "url.$tmp/fork.git.insteadOf" "https://github.com/someone/$name.git"
unset GH_REPO

commit() { # $1 = repo, $2 = version, $3 = file to change
    mkdir -p "$1/$(dirname "$3")"
    printf '[package]\nversion = "%s"\n' "$2" >"$1/Cargo.toml"
    echo "$RANDOM" >>"$1/$3"
    git -C "$1" add -A
    git -C "$1" commit -qm "change $3"
}

# The guarded repo: main at 0.5.0, a stale fork with main at 0.1.0.
git init -q "$tmp/seed"
commit "$tmp/seed" 0.1.0 README.md
git -C "$tmp/seed" branch fork-main
commit "$tmp/seed" 0.5.0 "$api_dir/api.rs"
git clone -q --bare "$tmp/seed" "$tmp/omnect.git"
git init -q --bare "$tmp/fork.git"
git -C "$tmp/seed" push -q "$tmp/fork.git" fork-main:main

work=$tmp/work
git clone -q "https://github.com/$repo.git" "$work"
git -C "$work" remote rename origin upstream
ln -s "$claude_dir" "$work/.claude"
echo /.claude >>"$work/.git/info/exclude"
git -C "$work" switch -qc api
commit "$work" 0.5.1 "$api_dir/api.rs"
git -C "$work" switch -qc api-minor
commit "$work" 0.6.0 "$api_dir/api.rs"
git -C "$work" switch -qc docs upstream/main
commit "$work" 0.5.1 README.md
# A release branch that already holds the API change of "api".
git -C "$work" push -q upstream api:release api:refs/pull/5/head api:refs/pull/8/head docs:refs/pull/6/head
git -C "$work" fetch -q upstream
git -C "$work" switch -q api
# PRs 10 to 13: their heads are only on the remote.
git clone -q "$tmp/omnect.git" "$tmp/pusher"
for pr in 10 11 12 13; do
    commit "$tmp/pusher" 0.5.1 "$api_dir/api.rs"
    git -C "$tmp/pusher" push -q origin "HEAD:refs/pull/$pr/head"
done
# Another clone of the guarded repo, outside the project.
git clone -q --no-local "https://github.com/$repo.git" "$tmp/clone2"

# Fork layout: origin is the stale fork, the guarded repo has another name.
forked=$tmp/forked
git clone -q "https://github.com/someone/$name.git" "$forked"
git -C "$forked" remote add omnect "https://github.com/$repo.git"
git -C "$forked" fetch -q omnect
git -C "$forked" switch -qc api omnect/main
commit "$forked" 0.5.1 "$api_dir/api.rs"

other=$tmp/other
git init -q "$other"
git -C "$other" remote add origin https://github.com/someone/other.git

mkdir "$tmp/bin"
cat >"$tmp/bin/gh" <<'STUB'
#!/usr/bin/env bash
# gh pr view [<pr>] [-R repo] --json ... -q ...: number, head, base and title
# of a PR in the guarded repo.
[[ "$1 $2" == "pr view" ]] || exit 1
if [[ -n "${STUB_HANG:-}" ]]; then
    # Like gh, which ignores SIGALRM.
    trap '' ALRM
    exec sleep 30
fi
shift 2
pr="" repo=${GH_REPO:-}
while [[ $# -gt 0 ]]; do
    case $1 in
    -R) repo=$2; shift 2 ;;
    --json | -q) shift 2 ;;
    *) pr=$1; shift ;;
    esac
done
[[ -z "$repo" || "$repo" == "$STUB_REPO" ]] || exit 1
[[ -n "$pr" ]] || pr=${STUB_CURRENT_PR:-}
[[ -n "$pr" ]] || exit 1
pr=${pr##*/}
sha=$(git -C "$STUB_REMOTE" rev-parse -q --verify "refs/pull/$pr/head") || sha=${STUB_MISSING_SHA:-}
[[ -n "$sha" ]] || exit 1
echo "$pr $sha ${STUB_BASE:-main} ${STUB_TITLE:-fix: old}"
STUB
chmod +x "$tmp/bin/gh"
export PATH="$tmp/bin:$PATH" STUB_REMOTE=$tmp/omnect.git STUB_REPO=$repo CLAUDE_PROJECT_DIR=$work

printf '{"title":"fix: x"}' >"$work/title.json"
printf '%s' '{"query":"mutation { update\u0050ullRequest(input: {pullRequestId: \"x\", title: \"fix: x\"}) { clientMutationId } }"}' >"$work/q.json"
git -C "$work" worktree add -q --detach "$tmp/wt" api-minor

fail=0
count=0
# $1 = expected (deny|ask|none), $2 = command, $3 = cwd
check() {
    local got
    count=$((count + 1))
    got=$(jq -n --arg c "$2" --arg d "${3:-$work}" '{tool_name:"Bash",tool_input:{command:$c},cwd:$d}' |
        bash -c "$hook_cmd" | jq -r '.hookSpecificOutput.permissionDecision // empty') || got=error
    got=${got:-none}
    if [[ "$got" != "$1" ]]; then
        echo "FAIL: expected $1, got $got: $2 (in ${3:-$work})"
        fail=1
    fi
}

R=$repo
# Command forms: global flags, env values with quotes, groups, full paths,
# wrappers, chains, attached short values, more than one cd.
check deny "gh pr create --title 'fix: x'"
check deny "gh -R $R pr create --title 'fix: x'"
check deny "gh pr -R $R create --title 'fix: x'"
check deny "gh --repo=$R pr create --title 'fix: x'"
check deny "FOO=\"a b\" gh pr create --title 'fix: x'"
check deny "{ gh pr create --title 'fix: x'; }"
check deny "/usr/bin/gh pr create --title 'fix: x'"
check deny "timeout 30 gh pr create --title 'fix: x'"
check deny "if true; then gh pr create --title 'fix: x'; fi"
check deny "bash -c 'gh pr create --title \"fix: x\"'"
check deny "echo \$(gh pr create -t 'fix: x')"
check deny "gh pr edit 6 --add-label foo; gh pr create --title 'fix: x'"
check deny "UDS_API_BREAKING=no gh pr create --title 'fix: x' && gh pr edit 5 -t 'fix: x'"
check deny "gh pr edit 5 -t'fix: x'"
check deny "gh pr edit 5 -t='fix: x'"
check deny "gh pr create -R$R -t 'fix: x'"
check deny "gh pr create -dt 'fix: x'"
check deny "gh pr edit --title 'fix: x' 5"
check deny "gh pr edit https://github.com/$R/pull/5 --title 'fix: x'"
check deny "cd /tmp && cd $work && gh pr create --title 'fix: x'" /tmp
check none "cd $work && cd $other && gh pr create --title 'fix: x'"
check deny "(cd $other) && gh pr create --title 'fix: x'"
check none "(cd $work && gh pr create --title 'fix: x' -R someone/other)"
check ask "cd \"\$X\" && gh pr create --title 'fix: x'"
check none "gh pr create -R someone/other --title 'fix: x'"
check none "gh pr create -Rsomeone/other --title 'fix: x'"
check none "gh pr create --title 'fix: x'" "$other"
# Text in a body or a heredoc is no flag and no command.
check none "gh pr edit 5 --body '--title fix: x'"
check none "gh pr edit 5 -b '-t fix: x'"
check none "cat <<'EOF'
gh pr create --title 'fix: x'
EOF"
check none "echo gh pr create --title 'fix: x'"
# --head and --base.
check deny "gh pr create --head api --title 'fix: x'" "$work"
check none "gh pr create --head docs --title 'fix: x'"
check none "gh pr create --head someone:docs --title 'fix: x'"
check ask "gh pr create --head nosuch --title 'fix: x'"
check none "gh pr create -Brelease -t 'fix: x'"
check none "gh pr create --base=release -t 'fix: x'"
check ask "gh pr create -B nosuch -t 'fix: x'"

# Answers that pass.
check none "UDS_API_BREAKING=no gh pr create --title 'fix: x'"
check none "env UDS_API_BREAKING=no gh pr create --title 'fix: x'"
git -C "$work" switch -q api-minor
check none "gh pr create --title 'feat!: x'"
check none "gh pr create --title 'feat(api)!: x'"
git -C "$work" switch -q api
check deny "gh pr create --title 'feat!: x'"
git -C "$work" switch -q docs
check none "gh pr create --title 'fix: x'"
git -C "$work" switch -q api

# Commands that do not set a title pass.
check none "git status"
check none "gh pr list"
check none "gh pr view 5"
check none "gh pr edit 5 --add-label foo"
check none "gh pr merge 5 --squash"
check none "gh pr create --dry-run --title 'fix: x'"
check none "gh api repos/$R/pulls/5"
check none "gh api -X PATCH repos/$R/pulls/5 -f body=x"
check none "gh api -X PUT repos/$R/pulls/5/merge -f merge_method=squash"
check none "gh api repos/$R/issues -f title='fix: x'"

# gh pr merge --subject, gh api on .../pulls, GraphQL.
check deny "gh pr merge 5 --squash --subject 'fix: x'"
check deny "gh pr merge 5 -st 'fix: x'"
check none "gh pr merge 6 -s -t 'fix: x'"
check deny "gh api -X PATCH repos/$R/pulls/5 -f title='fix: x'"
check deny "gh api --method=patch /repos/$R/pulls/5 -ftitle='fix: x'"
check deny "gh api -X PATCH https://api.github.com/repos/$R/pulls/5 --raw-field=title='fix: x'"
check deny "gh api -X PATCH 'repos/{owner}/{repo}/pulls/5' -F title='fix: x'"
check deny "gh api repos/$R/pulls -f title='fix: x' -f head=api -f base=main"
check deny "gh api repos/$R/pulls -f title='feat!: x' -f head=api -f base=main"
check none "UDS_API_BREAKING=no gh api repos/$R/pulls -f title='fix: x' -f head=api -f base=main"
check none "gh api repos/$R/pulls -f title='fix: x' -f head=docs -f base=main"
check deny "gh api -X PUT repos/$R/pulls/5/merge -f commit_title='fix: x'"
check deny "gh api -X PATCH repos/$R/pulls/5 --input title.json"
check deny "gh api -X PATCH repos/$R/pulls/5 -F title=@title.json"
check ask "gh api -X PATCH repos/$R/pulls/5 --input -"
check none "gh api -X PATCH repos/someone/other/pulls/5 -f title='fix: x'"
check ask "gh api graphql -f query='mutation { updatePullRequest(input: {title: \"fix: x\"}) { clientMutationId } }'"
check ask "gh api -X PATCH repositories/1/pulls/5 -f title='fix: x'"

# Target repo and base.
check deny "gh pr edit 8 --title 'fix: x'"
STUB_MISSING_SHA=1111111111111111111111111111111111111111 check ask "gh pr edit 9 --title 'fix: x'"
STUB_CURRENT_PR=5 check deny "gh pr edit --title 'fix: x'"
STUB_CURRENT_PR=5 STUB_BASE=release check none "gh pr edit --title 'fix: x'"
check deny "gh pr create --title 'feat!: x'" "$forked"
check deny "gh pr create --title 'fix: x'" "$forked"
check deny "GH_REPO=$R gh pr edit 5 --title 'fix: x'" "$other"
check none "GH_REPO=someone/other gh pr create --title 'fix: x'"
check deny "export GH_REPO=$R; cd $other && gh pr edit 5 -t 'fix: x'"
check none "export GH_REPO=someone/other; gh pr create -t 'fix: x'"
check deny "export GH_REPO=someone/other; unset GH_REPO; gh pr create -t 'fix: x'"
check deny "(export GH_REPO=someone/other); gh pr create -t 'fix: x'"
GH_REPO=someone/other check none "gh pr create -t 'fix: x'"

# Without python3 or jq the hook asks for gh commands and lets others run.
# Forms that the first parser version missed.
check deny "git status
gh pr create --title 'fix: x'"
check deny "git status;	gh pr create --title 'fix: x'"
check deny "bash <<'EOF'
gh pr create --title 'fix: x'
EOF"
check deny "gh pr create --dry-run --title 'fix: x' && gh pr create --title 'fix: x'"
check deny "gh pr create --dry-run=0 --title 'fix: x'"
check deny "cd -- $work && gh pr create --title 'fix: x'" /tmp
check ask "cd \"\$X\" && cd ../other && gh pr create --title 'fix: x'"
check deny "gh api -X PATCH repos/$R/pulls/5 -f=title='fix: x'"
check deny "gh api -iX PATCH repos/$R/pulls/5 -f title='fix: x'"
# Forms that change the folder or GH_REPO in a way the hook does not follow:
# a gh call that sets a title asks.
check ask "env -C $work gh pr create --title 'fix: x'" "$other"
check ask "env -C $work bash -c 'gh pr create --title \"fix: x\"'" "$other"
check ask "env -C $other bash x.sh; gh pr create --title 'fix: x'"
check ask "cat <<'EOF' | env -C $work bash
gh pr create --title 'fix: x'
EOF" "$other"
check ask "GH_REPO=$R; export GH_REPO; gh pr edit 5 --title 'fix: x'" "$other"
check ask "export -n GH_REPO=someone/other; gh pr create --title 'fix: x'"
check ask "declare -x GH_REPO=$R; gh pr edit 5 --title 'fix: x'" "$other"
check ask "GH_REPO=$R bash -c 'gh pr edit 5 --title \"fix: x\"'" "$other"
GH_REPO=someone/other check ask "env -u GH_REPO gh pr create --title 'fix: x'"
check ask "env - gh pr create --title 'fix: x'"
check ask "env -S \"gh pr create --title 'fix: x'\""
check none "env -C $work git status"
check none "env -C $work gh pr create -R someone/other --title 'fix: x'"
# A repo or endpoint from a shell expansion cannot be read.
check ask "gh pr create -R \"\$REPO\" --title 'fix: x'"
check ask "GH_REPO=\$R gh pr edit 5 --title 'fix: x'"
check ask "gh api -X PATCH repos/\$OWNER/\$NAME/pulls/5 -f title='fix: x'"
check ask "gh api -X PATCH repos/$R/pulls/\$N -f title='fix: x'"
check ask "gh api -X PATCH \"\$EP\" -f title='fix: x'"
check ask "gh api graphql -f query=\"\$Q\""
check none "gh api -X POST repos/$R/pulls/\$N/comments/\$ID/replies -f body=x"
check none "gh api -X POST repos/someone/other/pulls/\$N -f title=x"
# More ways to set a title: the issues endpoint, auto-merge, revert.
check deny "gh api -X PATCH repos/$R/issues/5 -f title='fix: x'"
STUB_BASE=release check none "gh api -X PATCH repos/$R/issues/5 -f base=main"
check ask "gh api -X PATCH repos/$R/issues/42 -f title='Bug: x'"
check ask "gh api graphql -f query='mutation { enablePullRequestAutoMerge(input: {pullRequestId: \"x\", commitHeadline: \"fix: x\"}) { clientMutationId } }'"
check ask "gh pr revert 5 --title 'fix: x'"
check none "UDS_API_BREAKING=no gh pr revert 5"
check none "gh pr revert 5 -R someone/other"
# A new base is checked with the current title.
STUB_BASE=release check deny "gh pr edit 5 --base main"
STUB_BASE=release check none "gh pr edit 5 --base release"
STUB_BASE=release check deny "gh api -X PATCH repos/$R/pulls/5 -f title='fix: x' -f base=main"
STUB_BASE=release check none "gh api -X PATCH repos/$R/pulls/5 -f title='fix: x'"
STUB_BASE=release STUB_TITLE='feat!: old' check deny "gh api -X PATCH repos/$R/pulls/5 -f base=main"
# Round 3 of the self-review.
check ask "gh api -X PATCH repositories/1/pulls/5 --input -"
check ask "cat > q.graphql <<'X'
mutation { updatePullRequest(input: {pullRequestId: \"x\", title: \"fix: x\"}) { clientMutationId } }
X
gh api graphql -F query=@q.graphql"
check ask "gh api graphql -f query=\"mutation { \$(cat m.graphql) }\""
check none "gh api repos/someone/other/pulls/5/reviews --input -"
check none "gh api repos/someone/other/issues -F title=@nosuch.txt"
check none "gh api repos/$R/pulls/5/comments --input nosuch.json"
check ask "env -iC $work gh pr create --title 'fix: x'" "$other"
check ask "env -vC $work gh pr create --title 'fix: x'" "$other"
check ask "env --ch=$work gh pr create --title 'fix: x'" "$other"
check ask "sudo -D $work gh pr create --title 'fix: x'" "$other"
check deny "gh pr edit https://github.com/$R/pull/5/files --title 'fix: x'" "$other"
check deny "gh pr edit 'https://github.com/$R/pull/5#issuecomment-1' --title 'fix: x'" "$other"
check ask "eval \"gh pr create --title 'fix: x'\""
check ask "echo 5 | xargs -I{} gh pr edit {} --title 'fix: x'"
check none "git ls-files | xargs grep -l gh"
check ask "set -a; GH_REPO=$R; gh pr edit 5 --title 'fix: x'" "$other"
check deny "builtin cd $work && gh pr create --title 'fix: x'" "$other"
check ask "gh api --hostname \"\$H\" -X PATCH repos/$R/pulls/5 -f title='fix: x'"

# Round 4 of the self-review.
check ask "gh api repositories/1/pulls -f title='fix: x' -f head=api -f base=main"
check deny "gh api -X PATCH repos/:owner/:repo/pulls/5 -f title='fix: x'"
check deny "gh api repos/:owner/:repo/pulls -f title='fix: x' -f head=api -f base=main"
check none "gh pr checks 5 | sed \$'s/\\x1b\\\\[[0-9;]*m//g'"
check deny "echo \$'\\x1b[1m'; gh p''r create --title 'fix: x'"
check deny "gh pr edit 12 --title 'fix: x'" "$work/$api_dir"
check deny "gh pr edit 13 --title 'fix: x'" "$tmp/wt"
check none "gh pr list --json number -q '.[].number' | xargs -n1 gh pr view"
check none "git ls-files | xargs grep -l 'gh pr create'"
check none "find .claude -name '*.sh' -exec grep -n 'gh api' {} +"
check none "which gh; ps aux | grep gh; timeout 30 gh pr view 5"
check ask "echo pr create --title fix | xargs gh"
check ask "setsid gh pr create --title 'fix: x'"
check ask "flock /tmp/l gh pr create --title 'fix: x'"
check ask "watch 'gh pr edit 5 --title x'"
check deny "gh pr edit 'https://github.com/$R/pull/5?notification_referrer_id=1' --title 'fix: x'" "$other"
check deny "gh pr edit http://github.com/$R/pull/5 --title 'fix: x'" "$other"
check deny "gh pr edit https://www.github.com/$R/pull/5 --title 'fix: x'" "$other"
check ask "gh pr edit https://example.com/$R/pull/5 --title 'fix: x'" "$other"
check ask "env -S'gh pr create --title fix:x'"
check ask "env -vS'gh pr create --title fix:x'"
check ask "sudo -u root -D $work gh pr create --title 'fix: x'" "$other"
check ask "sudo -g wheel --chdir=$work gh pr create --title 'fix: x'" "$other"
check ask "sudo -Eu root gh pr create --title 'fix: x'"
check ask "timeout -vs KILL 10 gh pr create --title 'fix: x'"
check deny "sudo -E -u root gh pr create --title 'fix: x'"
check ask "cd \"\$X\" && gh api -X PATCH repos/$R/pulls/5 --input title.json"
check ask "gh api graphql --input q.json"
check ask "set -k; gh pr edit 5 --title 'fix: x' GH_REPO=$R" "$other"

# A PR head that is only on the remote is fetched, but only in the
# project's own clone: a fetch runs the clone's git config.
check deny "gh pr edit 10 --title 'fix: x'"
check ask "gh pr edit 11 --title 'fix: x'" "$tmp/clone2"
# A command that can write the parser's subshell marker byte asks.
# The parser keeps $'\\x01' as text, so bash and the hook agree here.
check deny "\$'\\x01('; cd $work; \$'\\x01)'; gh pr create --title 'fix: x'" "$other"
check none "echo \$'a\\nb'"
check ask "echo $(printf 'x\001y'); gh pr create --title 'fix: x'" "$other"

# Without timeout (macOS) the gh calls are limited with perl.
mkdir "$tmp/nopy" "$tmp/nojq" "$tmp/notimeout"
for tool in bash cat env git gh head jq mktemp perl python3 rm sed sleep tr timeout wc; do
    path=$(command -v "$tool") || continue
    [[ "$tool" == python3 ]] || ln -s "$path" "$tmp/nopy/$tool"
    [[ "$tool" == jq ]] || ln -s "$path" "$tmp/nojq/$tool"
    [[ "$tool" == timeout ]] || ln -s "$path" "$tmp/notimeout/$tool"
done
PATH=$tmp/notimeout check deny "gh pr edit 5 --title 'fix: x'"
PATH=$tmp/notimeout STUB_CURRENT_PR=5 STUB_BASE=release check none "gh pr edit --title 'fix: x'"
start=$SECONDS
STUB_HANG=1 UDS_API_GUARD_GH_TIMEOUT_S=2 PATH=$tmp/notimeout check ask "gh pr edit 5 --title 'fix: x'"
if [[ $((SECONDS - start)) -ge 10 ]]; then
    echo "FAIL: a hanging gh was not stopped after 2 s"
    fail=1
fi
PATH=$tmp/nopy check ask "gh pr create --title 'fix: x'"
PATH=$tmp/nopy check none "git status"
check_nojq() { # $1 = expected, $2 = command as JSON text, $3 = description
    local got
    count=$((count + 1))
    got=$(printf '{"tool_name":"Bash","tool_input":{"command":"%s","description":"%s"},"cwd":"%s"}' "$2" "${3:-}" "$work" |
        PATH=$tmp/nojq bash -c "$hook_cmd" | jq -r '.hookSpecificOutput.permissionDecision // empty') || got=error
    got=${got:-none}
    [[ "$got" == "$1" ]] || { echo "FAIL: without jq expected $1, got $got: $2"; fail=1; }
}
check_nojq ask "gh pr create"
check_nojq none "git status"
check_nojq none "git status" "Show status through git"
check_nojq ask "git status\\ngh pr create"

[[ "$fail" == 0 ]] && echo "all $count checks passed"
exit "$fail"
