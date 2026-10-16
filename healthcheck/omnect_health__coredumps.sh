#!/bin/bash
#

. ${BASH_SOURCE[0]%/*}/healthchecklib.sh || exit 2

# prints core dumps of the current system run to stdout
function find_coredumps() {
    local up now since sinces

    # at first find out time span we want to have core dumps for as we inly want
    # to consider those that happened during this system run
    now=$(date +%s)
    up=$(set -- $(cat /proc/uptime); echo ${1%.*})
    sinces=$((now - up))
    since="$(date -d '@'"$sinces" +"%Y-%m-%d %H:%M:%S")"

    coredumpctl -q --since "$since"
}

function do_check() {
    local cores rating=0

    cores=$(find_coredumps)
    [ -z "${cores}" ] || rating=2

    print_rating ${rating} "coredumps" "$ME"
    do_rate ${rating}

    return ${rating}
}

function do_get_infos() {
    local cores rating=0

    cores=$(find_coredumps)
    [ -z "${cores}" ] || rating=2
    do_rate ${rating}

    print_info_header "${ME}" "${rating}"
    [ -z "${cores}" ] || echo "${cores}"

    return ${rating}
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

exit $retval
