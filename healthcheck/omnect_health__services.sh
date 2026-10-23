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
# the rating is worked out on every call, because the log and the rules can
# both change while a check runs
function rate_servicelog() {
    local service="$1" servicelog="$2"
    local jquery rating

    # the rules are looked up by the service's name, because the
    # configuration may be replaced while a check runs
    #
    # a service may bring its own ratings, otherwise the defaults apply;
    # either way an entry no rule matched is rated red rather than being left
    # unrated, which would make it count as green
    jquery=$(jq -r --arg service "$service" '
        ( first(.services[]? | select(.service == $service))
          // error("no rules for service \($service)") ) as $rules
        | [ ( if ($rules | has("ratings"))
              then $rules.ratings[]?
              else ."default-ratings"[]?
              end
            ) | "(.infos[] | select(" + .condition + ")) += { \"rating\": \"" + .rating + "\"}"
          ]
        + [ "(.infos[] | select(has(\"rating\") | not)) += { \"rating\": \"red\" }" ]
        | join(" | ")' "$CFGFILEPATH") \
        || { error "rating rules for service \"$service\" could not be read"; return 2; }

    # NOTE:
    #   the entries are rated and counted in a single pass, and the rating is
    #   worked out where the entries are, so that the only thing left to do
    #   here is to recognise it; "-e" makes jq say so when a log yielded no
    #   result at all, which an empty log does
    #
    # NOTE:
    #   everything that is not plainly green or yellow counts as red: a rule
    #   may name a rating this script does not know, or none at all, which
    #   leaves the entry with an empty one, and such an entry has been
    #   matched by a rule precisely because it is worth reporting
    rating=$(jq -er "$jquery"' | [ ( .infos | map(select(.rating != "green" and .rating != "yellow")) | length ),
                                   ( .infos | map(select(.rating == "yellow")) | length ) ]
                               | if   .[0] > 0 then 2
                                 elif .[1] > 0 then 1
                                 else 0
                                 end' "$servicelog") \
        || { error "exit log \"$servicelog\" could not be rated"; return 2; }

    # a log that did not yield one of the three ratings has not been rated at
    # all, and an unrated log must not pass as a healthy one
    case "$rating" in
        0|1|2) return "$rating" ;;
    esac
    error "exit log \"$servicelog\" was rated \"$rating\""
    return 2
}

# prints what "check" has to say about one service
function report_check() {
    local service="$1" rating="$2"

    print_rating "$rating" "$service" "$ME"
}

# prints what "get-infos" has to say about one service
function report_infos() {
    local service="$1" rating="$2"
    local exitinfo="$LOGDIR/${service}.exit-info"

    print_info_header "${ME}(${service})" "$rating"
    [ "$rating" != 0 -a -r "$exitinfo" ] && cat "$exitinfo"

    return 0
}

# reports the given rating for every service the caller asked for, for the
# cases in which the configuration does not allow rating them one by one
#
# NOTE:
#   a rating nobody is named in is of no use to the reader: the dispatcher
#   prints what a check wrote and counts it as run, so returning a rating
#   without a line of its own leaves an empty line behind and an overall
#   rating that names no culprit, with the reason on stderr at best
function report_all() {
    local report="$1" rating="$2" service

    if [ ${#check_services[@]} -eq 0 ]; then
        "$report" "$ME" "$rating"
        return 0
    fi

    for service in "${check_services[@]}"; do
        "$report" "$service" "$rating"
    done
}

# rates every configured service the caller asked for, plus every service the
# caller asked for that the configuration does not know, and reports each one
# through the given report function
#
# NOTE:
#   both commands share this loop rather than keeping one of their own,
#   because they differ only in how a service is reported: as long as they
#   were written out twice, a fix to one of them kept missing the other
function do__cmd() {
    local report="$1"
    local service rating servicelog services=() known unknown=()

    # NOTE:
    #   a missing configuration is reported here rather than before the
    #   command is known, because what it means depends on what was asked
    #   for: the services named on the command line are then of unknown
    #   health, which is yellow, not green
    if [ ! -r "$CFGFILEPATH" ]; then
        warn "no configuration file (${CFGFILEPATH}) found, don't know what to do."
        do_rate 1
        report_all "$report" 1
        return 1
    fi

    known=$(configured_services) || { do_rate 2; report_all "$report" 2; return 2; }
    [ "$known" ] && mapfile -t services <<<"$known"

    for service in "${services[@]}"; do
        rating=0

        [ ${#check_services[@]} -eq 0 ] || in_list "$service" "${check_services[@]}" || continue

        servicelog="$LOGDIR/${service}.exit-log"
        if [ -r "$servicelog" ]; then
            rate_servicelog "$service" "$servicelog"
            rating=$?
        fi

        do_rate "$rating"
        "$report" "$service" "$rating"
    done

    mapfile -t unknown < <(unknown_services "${services[@]}")
    for service in "${unknown[@]}"; do
        warn "no rules for service \"$service\" in \"$CFGFILEPATH\""
        do_rate 1
        "$report" "$service" 1
    done

    get_overall_rating
}

function do_check() {
    do__cmd report_check
}

function do_get_infos() {
    do__cmd report_infos
}

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
