#!/bin/bash
#
# Purpose of this script is to analyze service log files and rate their
# content, and hence the status of the respective service, as running normal,
# bearing warnings or failing at all.
#
# As it is completely service specific which entries lead wo what rating this
# kind of information needs to be passed along with the service log files to
# be processed.
#
# An additional requirement is to have multiple files with specifications of
# log entry

# ${BASH_SOURCE[0]} has no directory part when called by bare name
[[ ${BASH_SOURCE[0]} == */* ]] && HC_DIR=${BASH_SOURCE[0]%/*} || HC_DIR=.
. "${HC_DIR}/healthchecklib.sh" || exit 2

: ${CFGDIR:=/etc/omnect/health_check}
: ${CFGFILE:=omnect_service_log_analysis.json}
CFGFILEPATH="${CFGDIR}/${CFGFILE}"

: ${LOGDIR:=/run/omnect_health_log}
: ${ANALYSISDIR:=/run/omnect_health_log}

# prints the services the configuration file knows, one per line
function configured_services() {
    jq -r '.services[].service' "$CFGFILEPATH" \
        || { error "configuration file \"$CFGFILEPATH\" could not be read"; return 2; }
}

# rates the entries of a service exit log by the current rules and reports
# the resulting rating as exit status
function rate_servicelog() {
    local n="$1" servicelog="$2"
    local jquery rated counts red yellow

    # re-rate in case of ...
    #   a) new entries, or
    #   b) newer configuration file which might change the (overall)
    #      outcome
    # NOTE:
    #   (re-)rating will normally happen after generation of new entries
    #   initiated by omnect_service_log.sh via systemd StopPost hook
    if [    ! -r "${servicelog}.rated" \
         -o "$CFGFILEPATH" -nt "${servicelog}" \
         -o "${servicelog}" -nt "${servicelog}.rated" \
       ]; then
        # a service may bring its own ratings, otherwise the defaults apply;
        # either way an entry no rule matched is rated red rather than being
        # left unrated, which would make it count as green
        #
        # ratings of an earlier run are dropped first, because they were
        # derived from rules that may have been changed or removed since:
        # keeping them would let a rating survive that the current rules do
        # not justify anymore, and shield the entry from the red fallback
        jquery=$(jq -r --argjson n "$n" '
            [ "del(.infos[].rating)" ]
            + [ ( if (.services[$n] | has("ratings"))
                  then .services[$n].ratings[]?
                  else ."default-ratings"[]?
                  end
                ) | "(.infos[] | select(" + .condition + ")) += { \"rating\": \"" + .rating + "\"}"
              ]
            + [ "(.infos[] | select(has(\"rating\") | not)) += { \"rating\": \"red\" }" ]
            | join(" | ")' "$CFGFILEPATH") \
            || { error "rating rules for \"$servicelog\" could not be read"; return 2; }

        # NOTE:
        #   the rating is written to a file of its own, because
        #   "${servicelog}.rated" only records when the log was last rated:
        #   a concurrent run -- the systemd StopPost hook rates a single
        #   service while a manual run rates all of them -- would otherwise
        #   move a half written file over the exit log
        rated=$(mktemp "${servicelog}.XXXXXX") \
            || { error "no place to rate \"$servicelog\" in"; return 2; }

        # NOTE:
        #   the rated log replaces the original one only if jq succeeded,
        #   because otherwise the sole record of what happened to the service
        #   would be overwritten by a truncated or empty file
        if jq "$jquery" "$servicelog" > "$rated"; then
            mv "$rated" "$servicelog"
            # create empty anew to remember when we last rated all entries
            : > "${servicelog}.rated"
        else
            error "exit log \"$servicelog\" could not be rated"
            # drop the marker so the next run attempts to rate again
            rm -f "$rated" "${servicelog}.rated"
            return 2
        fi
    fi

    counts=$(jq -r '[ ( .infos | map(select(.rating == "red")) | length ),
                      ( .infos | map(select(.rating == "yellow")) | length )
                    ] | join(" ")' "$servicelog") \
        || { error "exit log \"$servicelog\" could not be read"; return 2; }
    read -r red yellow <<<"$counts"

    [ "$red" -gt 0 ] && return 2
    [ "$yellow" -gt 0 ] && return 1
    return 0
}

function do_check() {
    local service rating servicelog services=() known n

    known=$(configured_services) || { do_rate 2; return 2; }
    [ "$known" ] && mapfile -t services <<<"$known"

    for ((n=0; n < ${#services[@]}; n++)); do
        rating=0
        service="${services[$n]}"

        [ -z "$check_services" ] || in_list "$service" "$check_services" || continue

        servicelog="$LOGDIR/${service}.exit-log"
        if [ -r "$servicelog" ]; then
            rate_servicelog "$n" "$servicelog"
            rating=$?
        fi

        print_rating $rating "$service" "$ME"
        do_rate "$rating"
    done
    return $overall_rating
}

function do_get_infos() {
    local service rating servicelog services=() known n

    known=$(configured_services) || { do_rate 2; return 2; }
    [ "$known" ] && mapfile -t services <<<"$known"

    for ((n=0; n < ${#services[@]}; n++)); do
        rating=0
        service="${services[$n]}"

        [ -z "$check_services" ] || in_list "$service" "$check_services" || continue

        servicelog="$LOGDIR/${service}.exit-log"
        if [ -r "$servicelog" ]; then
            rate_servicelog "$n" "$servicelog"
            rating=$?
        fi

        do_rate "$rating"
        print_info_header "${ME}(${service})" "$rating"
        if [ "$rating" != 0 -a -r /var/run/omnect_health_log/${service}.exit-info ]; then
                cat /var/run/omnect_health_log/${service}.exit-info
        fi
    done
    get_overall_rating
}

if [ ! -r "$CFGFILEPATH" ]; then
    warn "no configuration file (${CFGFILEPATH}) found, don't know what to do."
    exit 0
fi

command="${1:-check}"
[ "$1" ] && shift
check_command_arg "$command"

# allow to select tests via command line
check_services="$*"

# first argument must be either "check" or "get-infos"
case "$command" in
    check)
        do_check "$@"
        overall_rating=$?
        ;;
    get-infos)
        do_get_infos "$@"
        overall_rating=$?
        ;;
esac

# print_rating "" "omnect_service_log_analyze" "$ME"

exit $overall_rating
