#!/usr/bin/env bash
# PreToolUse reminder: a CI job checks that a PR which changes the UDS API
# files says whether it breaks the API. Remind Claude of the rule before a
# command that may create, rename, merge or push to a PR. Never blocks a
# command, so a loose match is enough.
set -u

pr_cmd_re='\bgh\b.*\b(pr\b.*\b(create|new|edit|merge)|api\b.*\bpulls)\b|\bgit\b.*\bpush\b'
grep -qE "$pr_cmd_re" || exit 0

cat <<'JSON'
{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"If this command creates, renames, merges or pushes to a PR that changes the UDS API files, follow \"Breaking UDS API changes\" in CLAUDE.md first: a breaking change needs ! in the PR title, a change that is not breaking needs a line UDS_API_BREAKING=no in the PR description, and a merge keeps the PR title. CI fails otherwise."}}
JSON
