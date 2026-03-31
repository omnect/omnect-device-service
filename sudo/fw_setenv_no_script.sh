#!/bin/bash -e
# Wrapper for fw_setenv – blocks script-file mode.
set -uo pipefail

FW_SETENV=/usr/bin/fw_setenv

usage() {
    echo "Usage: $0 <key> <value>" >&2
    echo "       Script-file mode is not permitted." >&2
    exit 1
}

die() {
    echo "ERROR: $*" >&2
    exit 1
}

[[ $# -ne 2 ]] && usage

KEY="$1"
VALUE="$2"

# Block -s / --script anywhere in the value (as a word)
for word in $VALUE; do
    if [[ "$word" == "-s" || "$word" == "--script" || "$word" == --script=* ]]; then
        die "Script-file mode is not allowed (flag: $word)"
    fi
done

exec "$FW_SETENV" "$KEY" "$VALUE"
