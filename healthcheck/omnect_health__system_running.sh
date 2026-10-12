#!/bin/bash
#

. ${BASH_SOURCE[0]%/*}/healthchecklib.sh || exit 2

function checkit() {
    do_rate_cmd systemctl -q is-system-running
}

function do_check() {
    local ret
    checkit
    ret=$?
    print_rating $ret system-running "$ME"
    return $ret
}

function do_get_infos() {
    local ret
    checkit
    ret=$?
    print_info_header "${ME}" "$ret"
    # a state other than "running" comes from failed units or from jobs that
    # are still pending
    [ $ret = 0 ] || { systemctl is-system-running; systemctl --failed; systemctl list-jobs; }
    return $ret
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
