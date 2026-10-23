#!/bin/bash
#
# Purpose of this script is to record service (re-)starts and terminations,
# regular as well as unintentional ones, in a generic way so that such
# occurrences can be examined by a health check routine.
#

HEALTHCHECK_DIR=/usr/lib/omnect/healthcheck

. ${HEALTHCHECK_DIR}/healthchecklib.sh

SERVICE_EXITLOGDIRNAME=omnect_health_log
SERVICE_EXITLOGDIR=/var/run/${SERVICE_EXITLOGDIRNAME}

STOP_POSTFIX=".exit-log"
STOP_INFOPOSTFIX=".exit-info"
STOP_1ST_INFOPOSTFIX=".1st.exit-info"
START_POSTFIX=".last-start"
START_1ST_POSTFIX=".1st-start"

function usage() {
    local do_exit="${1:--1}"
    
    cat <<-EOF
	Usage:

	   $ME start <service-name>
	   $ME stop  <service-name> <service-result> <exit-code> <exit-status>

	Generate a log entries in respective service log files ...
	 - for stop:
	    ${SERVICE_EXITLOGDIR}/<service-name>.$STOP_POSTFIX
	 - for start:
	    ${SERVICE_EXITLOGDIR}/<service-name>.$START_1ST_POSTFIX (first start only)
	    ${SERVICE_EXITLOGDIR}/<service-name>.$START_POSTFIX
EOF

    [ $do_exit -ge 0 ] && exit $do_exit
}

function output_logentry() {
    local outputfile="$1"
    local timestamp="$2"
    local service="$3"
    local result="$4"
    local exitcode="$5"
    local exitstatus="$6"
    
    # NOTE: a check may read this log at any moment, so every version of it
    #	    is assembled under a temporary name and moved into place in one
    #	    step. Writing to the log itself would expose it empty, then half
    #	    written, and a reader cannot tell either from a log of a service
    #	    that simply has nothing to report.
    local failed=0
    if [ -r "${outputfile}" ]; then
	# append new entry in existing file
	jq '.infos += [{ timestamp: "'"$timestamp"'"'"${result:+, result: \"$result\"}""${exitcode:+, exitcode: \"$exitcode\"}""${exitstatus:+, exitstatus: \"$exitstatus\"}"' }]' < "$outputfile" > "${outputfile}.tmp" || failed=1
    else
	# create new file
	jq -n '{ service: "'"$service"'", infos: [ { timestamp: "'"$timestamp"'"'"${result:+, result: \"$result\"}""${exitcode:+, exitcode: \"$exitcode\"}""${exitstatus:+, exitstatus: \"$exitstatus\"}"' } ] }' > "${outputfile}.tmp" || failed=1
    fi

    if [ $failed -ne 0 ]; then
	rm -f "${outputfile}.tmp"
	fatal "couldn't write a log entry for service \"$service\""
	return 1
    fi

    mv "${outputfile}.tmp" "${outputfile}"
}

function output_exitinfo() {
    local outputfile="$1"
    local timestamp="$2"
    local service="$3"
    local result="$4"
    local exitcode="$5"
    local exitstatus="$6"

    {
	cat <<EOF
========================================
Service:     $service
Result:      $result
Exitcode:    $exitcode
Exitstatus:  $exitstatus
Timestamp:   $timestamp
----------------------------------------
EOF
	journalctl -b0 --no-pager -l -u "$service"
    } > "${outputfile}.tmp"

    mv "${outputfile}.tmp" "${outputfile}"
}

case "$1" in
    start)
	EXPECTED_PARAMS=2
    ;;
    stop)
	EXPECTED_PARAMS=5
    ;;
    *)
	fatal "unrecognized operation \"$1\" given" && usage 1
    ;;
esac

[ $# -lt $EXPECTED_PARAMS ] && fatal "too few parameters ($# < $EXPECTED_PARAMS)" && usage 1
[ $# -gt $EXPECTED_PARAMS ] && warn "additional parameters ignored"

SERVICE="$2"

if [ ! -d "$SERVICE_EXITLOGDIR" ]; then
    mkdir "$SERVICE_EXITLOGDIR" \
	|| fatal "couldn't create directory \"$SERVICE_EXITLOGDIR\""
fi

SERVICE_EXITLOGFILE="$SERVICE_EXITLOGDIR/$SERVICE$STOP_POSTFIX"
SERVICE_EXITINFOFILE="$SERVICE_EXITLOGDIR/$SERVICE$STOP_INFOPOSTFIX"
SERVICE_EXITINFOFILE_1ST="$SERVICE_EXITLOGDIR/$SERVICE$STOP_1ST_INFOPOSTFIX"
SERVICE_STARTLOGFILE="$SERVICE_EXITLOGDIR/$SERVICE$START_POSTFIX"
SERVICE_STARTLOGFILE_1ST="$SERVICE_EXITLOGDIR/$SERVICE$START_1ST_POSTFIX"

timestamp=$(date +%Y-%m-%dT%H:%M:%SZ)

case "$1" in
    start)
	output_logentry "$SERVICE_STARTLOGFILE" "$timestamp" "$SERVICE"
	if [ ! -r "$SERVICE_STARTLOGFILE_1ST" ]; then
	    cp -a "$SERVICE_STARTLOGFILE" "$SERVICE_STARTLOGFILE_1ST"
	fi
    ;;
    stop)
	# FIXME: do we need to take care of log file rotation or removal?
	output_logentry "$SERVICE_EXITLOGFILE" "$timestamp" "$SERVICE" "$3" "$4" "$5"
	${HEALTHCHECK_DIR}/omnect_health__services.sh check "$SERVICE" > /dev/null
	output_exitinfo "$SERVICE_EXITINFOFILE" "$timestamp" "$SERVICE" "$3" "$4" "$5"
	if [ ! -r "$SERVICE_EXITINFOFILE_1ST" ]; then
	    cp -a "$SERVICE_EXITINFOFILE" "$SERVICE_EXITINFOFILE_1ST"
	fi
    ;;
esac
