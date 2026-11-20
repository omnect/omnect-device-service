#!/usr/bin/env bash
# PreToolUse guard: a PR to this repo must state whether it breaks the UDS API
# that edge modules use. Under squash merge the PR title (or the merge
# subject) becomes the commit subject, and the release notes collect breaking
# changes from the "!" in it.
#
# Arguments: the GitHub repo (owner/name), then git pathspecs of the files
# that define the API (README.md, "Breaking changes").
#
# A command that sets a PR title or merge subject of this repo is denied
# until the question is answered. When the hook cannot check such a command,
# it asks the user. Its own errors must not block other commands, so only
# `set -u`. Runs on bash 3.2 (macOS) too.
set -u

NOT_BREAKING_MARKER="UDS_API_BREAKING=no"
# The tests set a shorter time.
GH_TIMEOUT_S=${UDS_API_GUARD_GH_TIMEOUT_S:-15}
FETCH_TIMEOUT_S=20
# Below the hook timeout in .claude/settings.json, so the hook decides
# before Claude Code stops it.
BUDGET_S=50
MAX_BODY_FILE_BYTES=200000
SELF_CHECK="check README.md, section \"Breaking changes\", yourself"
CHECK_YOURSELF="Check README.md, section \"Breaking changes\", yourself."
# gh as a word; also matches /usr/bin/gh.
GH_WORD='(^|[^[:alnum:]_.-])gh([^[:alnum:]_-]|$)'

ask_fixed() { # $1 = reason without characters that need JSON escaping
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"%s"}}\n' "$1"
    exit 0
}

input=$(cat)
# In the JSON text a newline or tab before gh is written as "\n" or "\t".
JSON_GH_WORD='\\[nt]gh([^[:alnum:]_-]|$)'
[[ "$input" =~ $GH_WORD || "$input" =~ $JSON_GH_WORD ]] || exit 0
command -v jq >/dev/null ||
    ask_fixed "uds-api-guard needs jq. If this command creates, retitles or merges a PR of this repo, check README.md (Breaking changes) yourself."
cmd=$(jq -r '.tool_input.command // empty' <<<"$input" 2>/dev/null)
[[ "$cmd" =~ $GH_WORD ]] || exit 0
[[ $# -ge 2 ]] || ask_fixed "uds-api-guard: .claude/settings.json must pass the repo and the API paths."

lower() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }

guarded=$(lower "$1")
shift
api_paths=("$@")
paths_text=""
for p in "${api_paths[@]}"; do paths_text+="${paths_text:+, }${p#:(top)}"; done

decision=""
reason=""
flag() { # $1 = deny|ask, $2 = reason; a deny wins over an ask
    [[ "$decision" == deny ]] && return 0
    if [[ "$1" == deny || -z "$decision" ]]; then
        decision=$1
        reason="$guarded: $2"
    fi
}
finish() {
    [[ -n "$decision" ]] && jq -n --arg d "$decision" --arg r "$reason" \
        '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:$d,permissionDecisionReason:$r}}'
    exit 0
}

if command -v timeout >/dev/null; then
    timeout_cmd=timeout
elif command -v gtimeout >/dev/null; then
    timeout_cmd=gtimeout
else
    timeout_cmd=""
fi
run_limited() { # $1 = seconds, rest = command; fails when the budget is used up
    local s=$1
    shift
    [[ $((SECONDS + s)) -le "$BUDGET_S" ]] || return 124
    if [[ -n "$timeout_cmd" ]]; then
        "$timeout_cmd" "$s" "$@"
    elif command -v perl >/dev/null; then
        # gh (Go) ignores SIGALRM, so the alarm kills the child instead.
        perl -e 'my $s = shift; my $pid = fork() // exit 127;
            if (!$pid) { exec @ARGV or exit 127 }
            $SIG{ALRM} = sub { kill "KILL", $pid; waitpid $pid, 0; exit 124 };
            alarm $s; waitpid $pid, 0; exit($? >> 8)' "$s" "$@"
    else
        "$@"
    fi
}

# Values from the command reach gh and git only in these forms: the hook
# runs before the user approves the command.
PR_NUM_RE='^[0-9]+$'
PR_URL_RE='^https?://(www\.)?github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)/pull/([0-9]+)([/#?].*)?$'
valid_branch() { [[ "$1" != -* ]] && git check-ref-format --branch "$1" >/dev/null 2>&1; }

# owner/name in lower case from -R, GH_REPO or a remote URL.
norm_repo() {
    local r=$1
    r=${r%/}
    r=${r%.git}
    r=${r##*:}
    [[ "$r" == */*/* ]] && r=${r#"${r%/*/*}"/}
    lower "$r"
}

# Name of the remote of $1 whose URL is the guarded repo. The URL is read as
# configured, before any insteadOf rewrite.
guarded_remote() {
    local key url
    while read -r key url; do
        key=${key#remote.}
        if [[ "$(norm_repo "$url")" == "$guarded" ]]; then
            printf '%s' "${key%.url}"
            return 0
        fi
    done < <(git -C "$1" config --get-regexp '^remote\..*\.url$' 2>/dev/null)
    return 1
}

# Folder of the command being checked, and the text of a cd target that
# could not be resolved.
cur_dir=""
cur_unknown=""
# Where the PR is checked: git dir and the remote of the guarded repo.
gd=""
remote=""
head=""
base_branch=""
pr_title=""

# Sets gd and remote. $1 = 1 when the repo was named in the command, so
# another local clone may be used.
locate() {
    if [[ -z "$cur_unknown" ]] && remote=$(guarded_remote "$cur_dir"); then
        gd=$cur_dir
        return 0
    fi
    if [[ "$1" == 1 && -n "${CLAUDE_PROJECT_DIR:-}" ]] && remote=$(guarded_remote "$CLAUDE_PROJECT_DIR"); then
        gd=$CLAUDE_PROJECT_DIR
        return 0
    fi
    return 1
}

# Target repo of a gh call: $1 = repo named in the call (-R, URL, API path)
# or "", $2 = GH_REPO of the call or "". Returns 0 for the guarded repo with
# gd and remote set, else 1.
target() {
    local named=${1:-$2}
    if [[ "$named" == *[\$\`]* ]]; then
        flag ask "the hook cannot read the repo \"$named\" (a shell expansion). Use the literal repo, or $SELF_CHECK."
        return 1
    fi
    # A repo named in the call is certain; GH_REPO may be changed by a form
    # the hook does not follow.
    if [[ -n "$1" && "$(norm_repo "$1")" != "$guarded" ]]; then
        return 1
    fi
    if [[ -n "$unsure" ]]; then
        flag ask "this command uses \"$unsure\", which the hook does not follow, so it cannot tell where a gh command runs. Split the command, or $SELF_CHECK."
        return 1
    fi
    if [[ -n "$named" ]]; then
        [[ "$(norm_repo "$named")" == "$guarded" ]] || return 1
        locate 1 && return 0
        flag ask "no local clone of $guarded found to check this command. Clone it, or $SELF_CHECK."
        return 1
    fi
    if [[ -n "$cur_unknown" ]]; then
        flag ask "this gh command runs in $cur_unknown, a folder the hook cannot resolve. Use -R or a literal path, or $SELF_CHECK."
        return 1
    fi
    locate 0
}

# The .git folder that holds the config, shared by all worktrees.
common_dir() {
    local d
    d=$(git -C "$1" rev-parse --git-common-dir 2>/dev/null) || return 1
    (cd "$1" && cd "$d" && pwd -P)
}

# Sets head, base_branch and pr_title for an existing PR. $1 = selector or
# "", $2 = GH_REPO of the call or "", $3 = what the selector names.
existing_pr() {
    local sel=$1 env=$2 what=${3:-PR} info num
    local -a cmd_env=(env -u GH_REPO)
    local query='"\(.number) \(.headRefOid) \(.baseRefName) \(.title)"'
    if [[ -n "$sel" ]]; then
        # Only the number goes to gh; the repo is the guarded one.
        [[ "$sel" =~ $PR_URL_RE ]] && sel=${BASH_REMATCH[3]}
        if ! [[ "$sel" =~ $PR_NUM_RE ]] && ! valid_branch "$sel"; then
            flag ask "could not read the PR selector \"$sel\". $CHECK_YOURSELF"
            return 1
        fi
        info=$(cd "$gd" && run_limited "$GH_TIMEOUT_S" gh pr view "$sel" -R "$guarded" \
            --json number,headRefOid,baseRefName,title -q "$query" 2>/dev/null)
    else
        # The PR of the current branch, found the way the command finds it.
        if [[ "$gd" != "$cur_dir" ]]; then
            flag ask "could not tell which PR this is. Name the PR, or $SELF_CHECK."
            return 1
        fi
        # target() found that GH_REPO means the guarded repo.
        [[ -n "$env" ]] && cmd_env=(env "GH_REPO=$guarded")
        info=$(cd "$gd" && run_limited "$GH_TIMEOUT_S" "${cmd_env[@]}" gh pr view \
            --json number,headRefOid,baseRefName,title -q "$query" 2>/dev/null)
    fi
    read -r num head base_branch pr_title <<<"$info"
    if ! [[ "${num:-}" =~ $PR_NUM_RE && "${head:-}" =~ ^[0-9a-f]{40}$ && -n "${base_branch:-}" ]]; then
        local label="the PR of the current branch"
        [[ -n "$sel" ]] && label="$what $sel"
        flag ask "could not read $label from GitHub. $CHECK_YOURSELF"
        return 1
    fi
    if ! git -C "$gd" cat-file -e "$head^{commit}" 2>/dev/null; then
        # A fetch runs the clone's config (e.g. core.sshCommand) before the
        # user approves the command: only in the project's own clone.
        if [[ -n "${CLAUDE_PROJECT_DIR:-}" && "$(common_dir "$gd")" == "$(common_dir "$CLAUDE_PROJECT_DIR")" ]]; then
            run_limited "$FETCH_TIMEOUT_S" git -C "$gd" fetch -q --no-tags -- "$remote" "refs/pull/$num/head" >/dev/null 2>&1
        fi
        if ! git -C "$gd" cat-file -e "$head^{commit}" 2>/dev/null; then
            flag ask "could not fetch the head of PR $num. Fetch it (git fetch $remote pull/$num/head), or $SELF_CHECK."
            return 1
        fi
    fi
}

# Sets head for a new PR: branch $1 (branch or owner:branch), else HEAD.
new_pr_head() {
    local b=${1#*:} ref
    if [[ -z "$1" ]]; then
        if [[ "$gd" != "$cur_dir" ]]; then
            flag ask "could not tell the head branch of this PR. Use --head, or $SELF_CHECK."
            return 1
        fi
        head=HEAD
        return 0
    fi
    if valid_branch "$b"; then
        ref=$(git -C "$gd" for-each-ref --count=1 --format='%(refname)' "refs/heads/$b" "refs/remotes/*/$b")
        if [[ -n "$ref" ]]; then
            head=$ref
            return 0
        fi
    fi
    flag ask "branch \"$1\" is not in the local clone. Fetch it, or $SELF_CHECK."
    return 1
}

# Default branch of the guarded repo as the local clone knows it, else main.
default_branch() {
    local h
    h=$(git -C "$gd" symbolic-ref -q --short "refs/remotes/$remote/HEAD" 2>/dev/null)
    if [[ -n "$h" ]]; then
        printf '%s' "${h#"$remote"/}"
    else
        printf main
    fi
}

version_of() { # $1 = commit: "major minor" from Cargo.toml
    git -C "$gd" show "$1:Cargo.toml" 2>/dev/null | sed -nE 's/^version *= *"([0-9]+)\.([0-9]+)\..*/\1 \2/p' | head -1
}

# The check itself. $1 = title or subject, $2 = 1 when the command carries
# the not-breaking marker. Needs gd, remote, head and base_branch.
check_title() {
    local title=$1 marker=$2 base_ref mb rc old new old_major old_minor new_major new_minor
    if ! valid_branch "$base_branch"; then
        flag ask "could not read the base branch \"$base_branch\". $CHECK_YOURSELF"
        return
    fi
    base_ref=$remote/$base_branch
    if ! git -C "$gd" rev-parse -q --verify "$base_ref^{commit}" >/dev/null; then
        flag ask "$base_ref is not in the local clone. Fetch it (git fetch $remote $base_branch), or $SELF_CHECK."
        return
    fi
    if ! mb=$(git -C "$gd" merge-base "$base_ref" "$head" 2>/dev/null); then
        flag ask "no merge base of $base_ref and the PR head. $CHECK_YOURSELF"
        return
    fi
    git -C "$gd" diff --quiet --no-ext-diff --no-textconv "$mb" "$head" -- "${api_paths[@]}"
    rc=$?
    [[ "$rc" -eq 0 ]] && return
    if [[ "$rc" -ne 1 ]]; then
        flag ask "git diff against $base_ref failed. $CHECK_YOURSELF"
        return
    fi

    if [[ "$title" =~ ^[a-z]+(\([^\)]*\))?!: ]]; then
        old=$(version_of "$base_ref")
        new=$(version_of "$head")
        if [[ -z "$old" || -z "$new" ]]; then
            flag ask "could not read the version in Cargo.toml. A breaking change needs at least a minor bump while on 0.x."
            return
        fi
        read -r old_major old_minor <<<"$old"
        read -r new_major new_minor <<<"$new"
        if [[ "$old_major" == 0 && "$new_major" == 0 && "$new_minor" -le "$old_minor" ]]; then
            flag deny "the title marks a breaking UDS API change, but Cargo.toml has no minor bump ($old_major.$old_minor → $new_major.$new_minor). A breaking change needs at least a minor bump while on 0.x."
        elif [[ "$old_major" != 0 && "$new_major" -le "$old_major" ]]; then
            flag deny "the title marks a breaking UDS API change, but Cargo.toml has no major bump ($old_major → $new_major)."
        fi
        return
    fi
    [[ "$marker" == 1 ]] && return
    flag deny "edge modules use its UDS API, and this PR changes its files ($paths_text). Read README.md, section \"Breaking changes\", and check the diff against $base_ref for each case listed there.
- Breaking: put \"!\" in the PR title or merge subject (e.g. \"feat!: ...\"), bump at least the minor version while on 0.x, and describe the old and new format in the PR description. The release notes pick it up from the title.
- Not breaking: run the command again with the prefix $NOT_BREAKING_MARKER directly before \"gh\" and tell the user why it is not breaking."
}

# Reads options the way gh (pflag) does. For each option in the map it sets
# opt_<key> (a repeated option keeps the last value) and opt_<key>_set; the
# other arguments go to pos.
# $1 = short options that take a value, $2 = long options that take a value,
# $3 = map "letter=key long=key ...", rest = arguments.
parse_opts() {
    local short=$1 long=" $2 " map=" $3 " a name val c i
    shift 3
    pos=()
    while [[ $# -gt 0 ]]; do
        a=$1
        shift
        case $a in
        --)
            pos+=("$@")
            return
            ;;
        --?*)
            name=${a#--}
            if [[ "$name" == *=* ]]; then
                val=${name#*=}
                name=${name%%=*}
            elif [[ "$long" == *" $name "* ]]; then
                val=${1-}
                [[ $# -gt 0 ]] && shift
            else
                val=true
            fi
            set_opt "$map" "$name" "$val"
            ;;
        -?*)
            # A cluster of short options: the first one that takes a value
            # takes the rest of the word, or the next word.
            for ((i = 1; i < ${#a}; i++)); do
                c=${a:i:1}
                [[ "$short" == *"$c"* ]] || continue
                val=${a:i+1}
                if [[ -z "$val" ]]; then
                    val=${1-}
                    [[ $# -gt 0 ]] && shift
                else
                    val=${val#=}
                fi
                set_opt "$map" "$c" "$val"
                break
            done
            ;;
        *) pos+=("$a") ;;
        esac
    done
}
set_opt() { # $1 = map, $2 = option name, $3 = value
    local k=${1#* "$2"=}
    [[ "$k" == "$1" ]] && return
    k=${k%% *}
    case $k in
    rawfield) fields+=("0$3") ;;
    typedfield) fields+=("1$3") ;;
    *) eval "opt_$k=\$3 opt_${k}_set=1" ;;
    esac
}
reset_opts() {
    opt_repo="" opt_title="" opt_title_set="" opt_head="" opt_base=""
    opt_dryrun="" opt_dryrun_set="" opt_method="" opt_input="" opt_host=""
    fields=()
}
# pflag reads these values of a boolean option as false.
is_false() { [[ " 0 f F false FALSE False " == *" $1 "* ]]; }

check_gh_pr() { # $1 = GH_REPO of the call, $2 = marker, rest = words after gh
    local env=$1 marker=$2 sub sel short long named title
    shift 2
    reset_opts
    # Group and subcommand: the first two words that are not an option.
    parse_opts "R" "repo" "R=repo repo=repo" "$@"
    [[ "${pos[0]:-}" == pr ]] || return
    sub=${pos[1]:-}
    case $sub in
    create | new)
        short="aBbFHlmprTtR"
        long="assignee attach base body body-file head label milestone project recover reviewer template title repo"
        ;;
    edit)
        short="BbFmtR"
        long="add-assignee add-label add-project add-reviewer attach base body body-file milestone remove-assignee remove-label remove-project remove-reviewer title repo"
        ;;
    merge)
        short="AbFtR"
        long="author-email body body-file match-head-commit subject repo"
        ;;
    revert)
        short="bFtR"
        long="body body-file title repo"
        ;;
    *) return ;;
    esac
    reset_opts
    parse_opts "$short" "$long" "R=repo repo=repo t=title title=title subject=title H=head head=head B=base base=base dry-run=dryrun" "$@"
    sel=${pos[2]:-}
    named=$opt_repo
    if [[ "$sel" =~ $PR_URL_RE ]]; then
        named=${BASH_REMATCH[2]}
    elif [[ "$sel" == *://* || "$sel" == *github.com* ]]; then
        flag ask "could not read the PR URL \"$sel\". $CHECK_YOURSELF"
        return
    fi
    case $sub in
    create | new)
        [[ -n "$opt_dryrun_set" ]] && ! is_false "$opt_dryrun" && return
        target "$opt_repo" "$env" || return
        new_pr_head "$opt_head" || return
        base_branch=$opt_base
        [[ -n "$base_branch" ]] || base_branch=$(default_branch)
        check_title "$opt_title" "$marker"
        return
        ;;
    revert)
        target "$named" "$env" || return
        [[ "$marker" == 1 ]] && return
        flag ask "gh pr revert creates a PR whose diff the hook cannot check. If the reverted PR changed the UDS API, the revert is a breaking change too. $CHECK_YOURSELF"
        return
        ;;
    esac
    # A merge without a subject keeps the PR title; an edit is checked when
    # it sets the title or the base.
    [[ -n "$opt_title_set" || ( "$sub" == edit && -n "$opt_base" ) ]] || return
    target "$named" "$env" || return
    existing_pr "$sel" "$env" || return
    title=$pr_title
    [[ -n "$opt_title_set" ]] && title=$opt_title
    [[ "$sub" == edit && -n "$opt_base" ]] && base_branch=$opt_base
    check_title "$title" "$marker"
}

# Path of a file named in the command, relative to its folder.
# Fails for a relative path when the folder is not known.
file_path() {
    if [[ "$1" == /* ]]; then
        printf '%s' "$1"
    elif [[ -z "$cur_unknown" && -z "$unsure" ]]; then
        printf '%s' "$cur_dir/$1"
    else
        return 1
    fi
}

# Value of a gh api field: "@file" is the file's content for -F/--field.
field_value() { # $1 = 1 for -F/--field, $2 = value
    local v=$2 f
    if [[ "$1" == 1 && "$v" == @* ]]; then
        [[ "$v" != @- ]] || return 1
        f=$(file_path "${v#@}") && [[ -f "$f" ]] || return 1
        v=$(head -c "$MAX_BODY_FILE_BYTES" "$f")
    fi
    printf '%s' "$v"
}

check_gh_api() { # $1 = GH_REPO of the call, $2 = marker, rest = words after gh
    local env=$1 marker=$2 a path method body typed
    local title="" title_set="" head_field="" base_field="" key val n kind owner_repo what=PR
    shift 2
    reset_opts
    # Fields are kept with a leading 1 for -F/--field, 0 for -f/--raw-field.
    parse_opts "XfFHqtp" "method raw-field field header jq template preview cache input hostname" \
        "X=method method=method f=rawfield raw-field=rawfield F=typedfield field=typedfield input=input hostname=host" "$@"
    [[ "${pos[0]:-}" == api ]] || return
    path=${pos[1]:-}
    body=$opt_input
    [[ -z "$opt_host" || "$(lower "$opt_host")" == github.com || "$opt_host" == *[\$\`]* ]] || return
    method=$(printf '%s' "$opt_method" | tr '[:lower:]' '[:upper:]')
    # gh sends a POST when fields or a body are given and no method is.
    if [[ -z "$method" && ( ${#fields[@]} -gt 0 || -n "$body" ) ]]; then
        method=POST
    fi
    [[ -n "$method" && "$method" != GET ]] || return

    # Endpoint without host, query and extra slashes.
    if [[ "$(lower "$path")" =~ ^https?://api\.github\.com(:443)?(/.*)?$ ]]; then
        path=${path:${#path}-${#BASH_REMATCH[2]}}
    fi
    path=${path%%\?*}
    path=$(sed -E 's#/+#/#g; s#^/##; s#/$##' <<<"$path")

    # Endpoints that can set a title, a merge subject or a base. Another
    # repo named literally is not ours.
    # gh fills in {owner}/{repo} and :owner/:repo from GH_REPO or the folder.
    if [[ "$path" =~ ^repos/([^/:\$\`{][^/\$\`{]*/[^/:\$\`{][^/\$\`{]*)/ && "$(norm_repo "${BASH_REMATCH[1]}")" != "$guarded" ]]; then
        return
    fi
    if ! [[ "$path" == graphql || "$path" =~ ^repositories/[^/]+/(pulls|issues)(/|$) ||
        "$path" == *[\$\`]* || "$opt_host" == *[\$\`]* ||
        "$path" =~ ^repos/[^/]+/[^/]+/(pulls|pulls/[0-9]+|pulls/[0-9]+/merge|issues/[0-9]+)$ ]]; then
        return
    fi

    # The fields that matter here; "@file" values are read from the file.
    local f_title="" f_title_set="" f_commit_title="" f_commit_title_set="" f_head="" f_base="" f_query=""
    if [[ -n "$body" ]]; then
        if ! body=$(file_path "$body") || [[ "$body" == */- || ! -f "$body" ]]; then
            flag ask "could not read the request body of this gh api call. $CHECK_YOURSELF"
            return
        fi
        f_title=$(jq -r '.title // empty' "$body" 2>/dev/null)
        [[ -n "$f_title" ]] && f_title_set=1
        f_commit_title=$(jq -r '.commit_title // empty' "$body" 2>/dev/null)
        [[ -n "$f_commit_title" ]] && f_commit_title_set=1
        f_head=$(jq -r '.head // empty' "$body" 2>/dev/null)
        f_base=$(jq -r '.base // empty' "$body" 2>/dev/null)
        # The raw text and the decoded query: JSON escapes can hide a name.
        f_query=$(head -c "$MAX_BODY_FILE_BYTES" "$body")$(jq -r '.query // empty' "$body" 2>/dev/null)
    fi
    for a in ${fields[@]+"${fields[@]}"}; do
        typed=${a:0:1}
        a=${a#?}
        key=${a%%=*}
        [[ "$key" != "$a" ]] || continue
        case $key in title | commit_title | head | base | query) ;; *) continue ;; esac
        if ! val=$(field_value "$typed" "${a#*=}"); then
            flag ask "could not read the field $key of this gh api call. $CHECK_YOURSELF"
            return
        fi
        case $key in
        title) f_title=$val f_title_set=1 ;;
        commit_title) f_commit_title=$val f_commit_title_set=1 ;;
        head) f_head=$val ;;
        base) f_base=$val ;;
        query) f_query+=$val ;;
        esac
    done

    if [[ "$path" == graphql ]]; then
        # A query taken from a shell variable cannot be read.
        # A query file can be written earlier in the same command, so the
        # command text counts too. GraphQL variables are $name, never $( ),
        # ${ } or backticks.
        if [[ "$f_query$cmd" =~ (createPullRequest|updatePullRequest|mergePullRequest|enablePullRequestAutoMerge|revertPullRequest)[^A-Za-z] ||
            ( -n "$f_query" && ( "$f_query" == [\$\`]* || "$f_query" != *"{"* || "$f_query" == *'$('* ||
            "$f_query" == *'${'* || "$f_query" == *'`'* ) ) ]]; then
            flag ask "this GraphQL mutation can set a PR title or merge subject, which the hook cannot check. $CHECK_YOURSELF"
        fi
        return
    fi
    if [[ -z "$f_title_set$f_commit_title_set$f_base" ]]; then
        # Without these fields nothing that the check covers changes, except
        # a new PR, which needs a title.
        [[ "$path" =~ ^repos/[^/]+/[^/]+/pulls$ ]] || return
    fi
    if [[ "$path" =~ ^repositories/ || "$opt_host" == *[\$\`]* ]]; then
        flag ask "the hook cannot tell which repo this gh api call writes to (a repository id or a host from a variable). Use repos/<owner>/<repo>/..., or $SELF_CHECK."
        return
    fi
    if [[ "$path" == *[\$\`]* ]]; then
        flag ask "the hook cannot read the endpoint $path (a shell expansion). Use the literal endpoint, or $SELF_CHECK."
        return
    fi
    if [[ "$path" =~ ^repos/([^/]+/[^/]+)/pulls$ ]]; then
        kind=create title=$f_title
    elif [[ "$path" =~ ^repos/([^/]+/[^/]+)/pulls/([0-9]+)$ ]]; then
        kind=edit n=${BASH_REMATCH[2]}
        [[ -n "$f_title_set$f_base" ]] || return
        title=$f_title title_set=$f_title_set base_field=$f_base
    elif [[ "$path" =~ ^repos/([^/]+/[^/]+)/pulls/([0-9]+)/merge$ ]]; then
        kind=merge n=${BASH_REMATCH[2]}
        [[ -n "$f_commit_title_set" ]] || return
        title=$f_commit_title title_set=1
    elif [[ "$path" =~ ^repos/([^/]+/[^/]+)/issues/([0-9]+)$ ]]; then
        # Also renames a PR; GitHub ignores a base here.
        kind=edit n=${BASH_REMATCH[2]} what="PR (or issue)"
        [[ -n "$f_title_set" ]] || return
        title=$f_title title_set=1
    else
        return
    fi
    owner_repo=${BASH_REMATCH[1]}
    head_field=$f_head
    [[ "$kind" == create ]] && base_field=$f_base
    [[ "$owner_repo" != *"{"* && "$owner_repo" != :* && "$owner_repo" != */:* ]] || owner_repo=""

    target "$owner_repo" "$env" || return
    if [[ "$kind" == create ]]; then
        new_pr_head "$head_field" || return
        base_branch=$base_field
        [[ -n "$base_branch" ]] || base_branch=$(default_branch)
    else
        existing_pr "$n" "$env" "$what" || return
        [[ -n "$title_set" ]] || title=$pr_title
        [[ "$kind" == edit && -n "$base_field" ]] && base_branch=$base_field
    fi
    check_title "$title" "$marker"
}

# Split the command into simple commands with cmdwords.py (a shell-like
# parser): quoted text stays one word, and a heredoc that no shell reads is
# no command.
words_file=$(mktemp) || ask_fixed "uds-api-guard: mktemp failed."
trap 'rm -f "$words_file"' EXIT
# The parser marks subshells with the byte 0x01; a command that holds that
# byte could fake a marker. bash 3.2 cannot match the byte in a regex, so tr
# counts it.
has_marker_byte() {
    local all rest
    all=$(printf '%s' "$cmd" | wc -c) && rest=$(printf '%s' "$cmd" | LC_ALL=C tr -d '\001' | wc -c) || return 0
    [[ -z "$all" || "$all" -ne "$rest" ]]
}
if has_marker_byte || ! command -v python3 >/dev/null ||
    ! python3 "${BASH_SOURCE[0]%/*}/cmdwords.py" <<<"$cmd" >"$words_file" 2>/dev/null; then
    if [[ "$cmd" =~ (^|[^[:alnum:]_-])(pr|api)([^[:alnum:]_-]|$) ]]; then
        flag ask "the hook could not parse this command (python3 missing or an unusual form). If it creates, retitles or merges a PR of $guarded, $SELF_CHECK."
    fi
    finish
fi

dir=$(jq -r '.cwd // empty' <<<"$input" 2>/dev/null)
[[ -d "$dir" ]] || dir=$PWD

# One entry of the parser output: sets words (without leading NAME=value),
# name (the command), prefix_repo (GH_REPO=... before it, or unset) and
# marker. Returns 1 at the end, 2 for a subshell marker (in name).
read_entry() {
    local n k w
    IFS= read -r -d '' n <&3 || return 1
    words=()
    for ((k = 0; k < n; k++)); do
        IFS= read -r -d '' w <&3 || break
        words+=("$w")
    done
    case ${words[0]:-} in $'\001(' | $'\001)')
        name=${words[0]}
        return 2
        ;;
    esac
    unset prefix_repo
    marker=0
    while [[ ${#words[@]} -gt 0 && "${words[0]}" =~ ^([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; do
        case ${BASH_REMATCH[1]} in
        GH_REPO) prefix_repo=${BASH_REMATCH[2]} ;;
        UDS_API_BREAKING) [[ "${BASH_REMATCH[2]}" == no ]] && marker=1 ;;
        esac
        words=("${words[@]:1}")
    done
    if [[ "${words[0]:-}" == builtin ]]; then
        words=("${words[@]:1}")
    fi
    name=${words[0]:-}
    name=${name##*/}
    return 0
}

# Commands that the parser splits off as their own entry (as cmdwords.py).
WRAPPERS=" timeout env command exec nice nohup stdbuf time sudo "
# A gh call in text that can set a title.
GH_TEXT_RE='(^|[^[:alnum:]_.-])gh[[:space:]]+(-[^[:space:]]*[[:space:]]+)*(pr[[:space:]]+(create|new|edit|merge|revert)|api)([^[:alnum:]_-]|$)'

# True when the words hold gh as a word, followed by a call that can set a
# title: pr create|new|edit|merge|revert or api. For xargs and parallel,
# which can add the rest, also gh or gh pr alone.
# $1 = command that runs gh, rest = its arguments.
title_gh_word() {
    local runner=$1 i j sub cmd_pos=-1
    shift
    local -a args=("$@")
    # The command of xargs or parallel: the first word after their options.
    if [[ " xargs parallel " == *" $runner "* ]]; then
        for ((i = 0; i < ${#args[@]}; i++)); do
            case ${args[i]} in
            -I | -n | -L | -P | -d | -E | -s | -a | -j) i=$((i + 1)) ;;
            -*) ;;
            *) cmd_pos=$i; break ;;
            esac
        done
    fi
    for ((i = 0; i < ${#args[@]}; i++)); do
        [[ "${args[i]##*/}" == gh ]] || continue
        j=$((i + 1))
        for sub in group subcommand; do
            while [[ "${args[j]:-}" == -* ]]; do
                case ${args[j]} in -R | --repo) j=$((j + 2)) ;; *) j=$((j + 1)) ;; esac
            done
            case $sub:${args[j]:-} in
            group: | subcommand:) [[ "$i" -eq "$cmd_pos" ]] && return 0 ;;
            group:api) return 0 ;;
            group:pr) j=$((j + 1)) && continue ;;
            subcommand:create | subcommand:new | subcommand:edit | subcommand:merge | subcommand:revert) return 0 ;;
            esac
            break
        done
    done
    return 1
}

# First pass: forms that change the folder or GH_REPO in a way the hook does
# not follow. A gh call that sets a title on this repo then asks.
unsure=""
wrapped_gh=""
exec 3<"$words_file"
while read_entry; rc=$?; [[ "$rc" -ne 1 ]]; do
    [[ "$rc" -eq 0 ]] || continue
    case $name in
    bash | sh | zsh | dash | ksh) [[ -n "${prefix_repo+x}" ]] && unsure="GH_REPO before $name" ;;
    env | sudo)
        # Options that change the folder or the environment, also in a
        # cluster or shortened (getopt_long), up to the gh word.
        for w in "${words[@]:1}"; do
            [[ "${w##*/}" == gh ]] && break
            case $name:$w in
            env:- | env:-[CuiS]* | env:-[!-]*[CuiS]* | env:--c* | env:--u* | env:--i* | env:--s* | sudo:-D* | sudo:-[!-]*D* | sudo:--ch*)
                unsure="$name $w"
                ;;
            esac
        done
        ;;
    esac
    # Code given as text: eval, env -S, watch, su -c, script -c.
    case $name in
    eval | env | watch | su | script)
        text=" ${words[*]:1}"
        [[ "$name" == env ]] && text=$(printf '%s ' "${words[@]:1}" | sed -nE 's/.*(-[^- ]*S|--s[a-z-]*=?)//p')
        [[ "$text" =~ $GH_TEXT_RE ]] &&
            flag ask "the hook cannot read the gh command that $name runs from text. If it sets a PR title or merge subject, $SELF_CHECK."
        ;;
    esac
    # gh as a word of another command (xargs, setsid, flock, find -exec,
    # ...), or behind a wrapper that the parser did not split off.
    if [[ -n "$wrapped_gh" ]]; then
        if [[ "$name" != gh && "$WRAPPERS" != *" $name "* ]]; then
            flag ask "the hook cannot tell which command \"$wrapped_gh\" runs. If it is a gh call that sets a PR title or merge subject, $SELF_CHECK."
        fi
        [[ "$name" == gh ]] && wrapped_gh=""
    fi
    if [[ "$name" != gh && " echo printf " != *" $name "* ]] && title_gh_word "$name" "${words[@]:1}"; then
        if [[ "$WRAPPERS" == *" $name "* ]]; then
            wrapped_gh=${words[*]}
        else
            flag ask "the hook cannot check the gh command that $name runs. If it sets a PR title or merge subject, $SELF_CHECK."
        fi
    fi
    case $name in
    set)
        # -a exports every assignment; -k puts NAME=value after the
        # command into its environment.
        [[ " ${words[*]:1}" =~ [[:space:]]-[[:alnum:]]*[ak] || "${words[*]}" == *allexport* || "${words[*]}" == *keyword* ]] &&
            unsure="set ${words[*]:1}"
        ;;
    export)
        case " ${words[*]:1} " in *" -n "*GH_REPO* | *" GH_REPO "*) unsure="export ${words[*]:1}" ;; esac
        ;;
    declare | typeset | local | readonly)
        [[ " ${words[*]:1}" == *" GH_REPO"* ]] && unsure="$name ${words[*]:1}"
        ;;
    esac
done
exec 3<&-
[[ -z "$wrapped_gh" ]] ||
    flag ask "the hook cannot tell which command \"$wrapped_gh\" runs. If it is a gh call that sets a PR title or merge subject, $SELF_CHECK."

# Second pass: follow cd and GH_REPO, check each gh call.
dir_unknown=""
env_repo=${GH_REPO:-}
env_repo_set=${GH_REPO+1}
# Subshells: cd and GH_REPO inside do not reach the commands after them.
stack_dir=() stack_unknown=() stack_repo=() stack_repo_set=()

resolve_dir() { # $1 = base, $2 = target; prints the folder
    CDPATH='' cd "$1" 2>/dev/null && CDPATH='' cd "$2" 2>/dev/null && pwd
}

exec 3<"$words_file"
while read_entry; rc=$?; [[ "$rc" -ne 1 ]]; do
    if [[ "$rc" -eq 2 ]]; then
        if [[ "$name" == $'\001(' ]]; then
            stack_dir+=("$dir") stack_unknown+=("$dir_unknown") stack_repo+=("$env_repo") stack_repo_set+=("$env_repo_set")
            continue
        fi
        top=$((${#stack_dir[@]} - 1))
        if [[ "$top" -ge 0 ]]; then
            dir=${stack_dir[top]} dir_unknown=${stack_unknown[top]}
            env_repo=${stack_repo[top]} env_repo_set=${stack_repo_set[top]}
            unset "stack_dir[$top]" "stack_unknown[$top]" "stack_repo[$top]" "stack_repo_set[$top]"
        else
            dir_unknown="a subshell the hook could not match"
        fi
        continue
    fi
    if [[ ${#words[@]} -eq 0 ]]; then
        # A plain assignment reaches gh only when GH_REPO is exported.
        [[ -n "${prefix_repo+x}" && -n "$env_repo_set" ]] && env_repo=$prefix_repo
        continue
    fi
    cur_dir=$dir cur_unknown=$dir_unknown
    case $name in
    cd | pushd)
        a="" end_opts=""
        for w in "${words[@]:1}"; do
            if [[ -z "$end_opts" ]]; then
                case $w in
                --) end_opts=1; continue ;;
                -[LPe@]*) continue ;;
                esac
            fi
            a=$w
            break
        done
        a=${a:-$HOME}
        a=${a/#\~/$HOME}
        if [[ "$a" == *[\$\`]* || "$a" == - ]]; then
            dir_unknown=$a
        elif [[ -n "$dir_unknown" && "$a" != /* ]]; then
            dir_unknown=$dir_unknown/$a
        elif d=$(resolve_dir "$dir" "$a"); then
            dir=$d dir_unknown=""
        else
            dir_unknown=$a
        fi
        ;;
    popd) dir_unknown=popd ;;
    export)
        for w in "${words[@]:1}"; do
            case $w in GH_REPO=*) env_repo=${w#*=} env_repo_set=1 ;; esac
        done
        ;;
    unset)
        for w in "${words[@]:1}"; do
            [[ "$w" == GH_REPO ]] && env_repo="" env_repo_set=""
        done
        ;;
    gh)
        call_repo=${prefix_repo-$env_repo}
        check_gh_pr "$call_repo" "$marker" "${words[@]:1}"
        check_gh_api "$call_repo" "$marker" "${words[@]:1}"
        ;;
    esac
done
exec 3<&-
finish
