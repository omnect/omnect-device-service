#!/bin/bash
#

. "$(dirname -- "${BASH_SOURCE[0]}")/healthchecklib.sh" || exit 2

# the journal entries systemd-coredump writes for a crashed process:
# SD_MESSAGE_COREDUMP for a crash it handled itself, and SD_MESSAGE_BACKTRACE
# for a backtrace passed in with --backtrace
COREDUMP_MESSAGE_ID=fc2e22bc6ee647b6b90729ab34a250b1
BACKTRACE_MESSAGE_ID=1f4e0a44a88649939aaea34fc6da8c95

# the core dumps of the current system run, what kept them from being looked
# up, and which parts of the journal could not be searched
coredumps=
coredump_errors=
coredump_skipped=

# puts the core dumps of the current system run into "coredumps"; returns 0
# when it could look for them, and 2 when it could not
#
# NOTE:
#   journalctl succeeds whether or not an entry matched, and fails only when
#   it could read no journal file at all, so the rating follows its exit
#   status
#
# NOTE:
#   the lookup is run twice, keeping the entries once and the diagnostics the
#   other time: a journal file that had to be skipped is reported on stderr,
#   and in a single stream it could not be told from an entry.
#
# NOTE:
#   only the lookup for the entries is quiet: without -q journalctl writes
#   "-- No entries --" where the entries are read, and with it it drops the
#   warnings about journal files the caller may not read
function find_coredumps() {
    local jargs rc=0

    # -b selects this boot, which is what the rating is about
    jargs=(-b "MESSAGE_ID=${COREDUMP_MESSAGE_ID}" + "MESSAGE_ID=${BACKTRACE_MESSAGE_ID}")

    coredumps=$(journalctl -q "${jargs[@]}" 2>/dev/null) || rc=$?
    coredump_skipped=$(journalctl "${jargs[@]}" 2>&1 1>/dev/null)
    coredump_errors=

    # a skipped journal file leaves a blind spot, but the other files were
    # searched, so it is kept apart from a lookup that yielded nothing
    if [ "${rc}" -ne 0 ]; then
        coredump_errors="${coredump_skipped:-the journal could not be read}"
        coredump_skipped=
        coredumps=
        return 2
    fi

    return 0
}

# rates the core dumps of the current system run and returns that rating
function rate_coredumps() {
    local rating=0

    if ! find_coredumps; then
        error "core dumps could not be looked up: ${coredump_errors}"
        rating=2
    elif [ -n "${coredumps}" ]; then
        rating=2
    elif [ -n "${coredump_skipped}" ]; then
        # the searchable part of the journal held no core dump, but the skipped
        # part may have, so there is evidence neither way
        error "core dumps could only be looked up in part of the journal: ${coredump_skipped}"
        rating=1
    fi

    return "${rating}"
}

function do_check() {
    local rating=0

    rate_coredumps || rating=$?

    print_rating "${rating}" "coredumps" "$ME"

    return "${rating}"
}

function do_get_infos() {
    local rating=0

    rate_coredumps || rating=$?

    print_info_header "${ME}" "${rating}"
    [ -z "${coredumps}" ] || echo "${coredumps}"
    [ -z "${coredump_errors}" ] || echo "${coredump_errors}"
    [ -z "${coredump_skipped}" ] || echo "${coredump_skipped}"

    return "${rating}"
}

command="${1:-check}"
[ "$1" ] && shift
check_command_arg "$command"

# first argument must be either "check" or "get-infos"
case "$command" in
    check)
        do_check "$@"
        retval=$?
        ;;
    get-infos)
        do_get_infos "$@"
        retval=$?
        ;;
esac

exit "$retval"
