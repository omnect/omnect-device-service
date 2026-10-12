#!/bin/bash
#

. ${BASH_SOURCE[0]%/*}/healthchecklib.sh || exit 2

# prints crash-looping services to stdout; returns 1 if they cannot be determined
function find_crash_loops() {
    local unit props active sub found="" units

    units=$(systemctl list-units --all --type=service --no-legend --plain | awk '{print $1}')
    if [ -z "${units}" ]; then
        return 1
    fi

    for unit in ${units}; do
        props=$(systemctl show "${unit}" -p ActiveState,SubState 2>/dev/null) || continue
        active=$(echo "${props}" | sed -n 's/^ActiveState=//p')
        sub=$(echo "${props}" | sed -n 's/^SubState=//p')

        # only a live loop is rated red: NRestarts has no time window and keeps
        # counting occasional restarts, and a unit that gave up restarting shows
        # up as a failed unit in the system-running check
        if [ "${active}" = "activating" ]; then
            case "${sub}" in
                # 'auto-restart-queued' is the same pending restart with the
                # restart job already queued
                auto-restart|auto-restart-queued)
                    found="${found} ${unit}(${sub})"
                    ;;
            esac
        fi
    done

    echo "${found}"
}

function do_check() {
    local found rating=0

    found=$(find_crash_loops) || rating=2
    [ -z "${found}" ] || rating=2
    print_rating ${rating} crash_loop "$ME"
    do_rate ${rating}
    return ${rating}
}

function do_get_infos() {
    local found rating=0 error=""

    found=$(find_crash_loops) || { rating=2; error="failed to list services"; }
    [ -z "${found}" ] || rating=2
    print_info_header "${ME}" "${rating}"
    [ -z "${error}" ] || echo "${error}"
    [ -z "${found}" ] || echo "crash-looping services:${found}"
    return ${rating}
}

command="${1:-check}"
[ "$1" ] && shift
check_command_arg "$command"

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

exit $retval
