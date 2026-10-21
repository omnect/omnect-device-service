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

. "$(dirname -- "${BASH_SOURCE[0]}")/healthchecklib.sh" || exit 2

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

# prints the requested services the given list of known ones does not hold
function unknown_services() {
    local service

    for service in "${check_services[@]}"; do
        in_list "$service" "$@" || echo "$service"
    done
}

# rates the entries of a service exit log by the current rules and reports
# the resulting rating as exit status
#
# NOTE:
#   the rating is worked out afresh on every run and never written down.
#   Keeping it would mean deciding when it stops being true, and the exit
#   log and the rules it is derived from are both changed by others while a
#   check runs: omnect_service_log.sh appends from a systemd StopPost hook,
#   and the rules are a configuration file anyone may edit.  A rating
#   believed a moment too long buries the crash recorded after it, which is
#   the one thing this log exists to surface, and no amount of comparing
#   timestamps or stamping identities makes that reliable.  Nothing outside
#   this function ever read such a rating anyway, so there is nothing to
#   save by keeping one.
function rate_servicelog() {
    local n="$1" servicelog="$2"
    local jquery counts red yellow

    # a service may bring its own ratings, otherwise the defaults apply;
    # either way an entry no rule matched is rated red rather than being left
    # unrated, which would make it count as green
    #
    # ratings an older version of this script left in the log are dropped
    # first, because they were derived from rules that may have been changed
    # or removed since: keeping them would let a rating survive that the
    # current rules do not justify anymore, and shield the entry from the red
    # fallback
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
    #   the entries are rated and counted in a single pass, and a log jq
    #   cannot read through is an error rather than a log without findings
    counts=$(jq -r "$jquery"' | [ ( .infos | map(select(.rating == "red")) | length ),
                                  ( .infos | map(select(.rating == "yellow")) | length )
                                ] | join(" ")' "$servicelog") \
        || { error "exit log \"$servicelog\" could not be rated"; return 2; }
    read -r red yellow <<<"$counts"

    [ "$red" -gt 0 ] && return 2
    [ "$yellow" -gt 0 ] && return 1
    return 0
}

function do_check() {
    local service rating servicelog services=() known unknown=() n

    known=$(configured_services) || { do_rate 2; return 2; }
    [ "$known" ] && mapfile -t services <<<"$known"

    for ((n=0; n < ${#services[@]}; n++)); do
        rating=0
        service="${services[$n]}"

        [ ${#check_services[@]} -eq 0 ] || in_list "$service" "${check_services[@]}" || continue

        servicelog="$LOGDIR/${service}.exit-log"
        if [ -r "$servicelog" ]; then
            rate_servicelog "$n" "$servicelog"
            rating=$?
        fi

        print_rating $rating "$service" "$ME"
        do_rate "$rating"
    done

    mapfile -t unknown < <(unknown_services "${services[@]}")
    for service in "${unknown[@]}"; do
        warn "no rules for service \"$service\" in \"$CFGFILEPATH\""
        print_rating 1 "$service" "$ME"
        do_rate 1
    done
    return $overall_rating
}

function do_get_infos() {
    local service rating servicelog services=() known unknown=() n

    known=$(configured_services) || { do_rate 2; return 2; }
    [ "$known" ] && mapfile -t services <<<"$known"

    for ((n=0; n < ${#services[@]}; n++)); do
        rating=0
        service="${services[$n]}"

        [ ${#check_services[@]} -eq 0 ] || in_list "$service" "${check_services[@]}" || continue

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

    mapfile -t unknown < <(unknown_services "${services[@]}")
    for service in "${unknown[@]}"; do
        warn "no rules for service \"$service\" in \"$CFGFILEPATH\""
        do_rate 1
        print_info_header "${ME}(${service})" 1
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
check_services=("$@")

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
