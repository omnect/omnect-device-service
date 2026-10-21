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

# reports the identity of the files a rating is derived from
#
# NOTE:
#   both writers -- omnect_service_log.sh and the rating below -- publish by
#   rename, so a changed inode already tells new content from old; mtime and
#   size are reported as well to catch a writer that changes a file in place
function input_identity() {
    stat -c '%i %Y %s' "$@" 2>/dev/null | tr '\n' ' '
}

# reports what a rating was derived from and what it found, as one line of
# fields separated by a unit separator
function read_rating() {
    jq -r '[ ( ."rated-from" // "" ),
             ( .infos | map(select(.rating == "red")) | length | tostring ),
             ( .infos | map(select(.rating == "yellow")) | length | tostring )
           ] | join("\u001f")' "$1"
}

# rates the entries of a service exit log by the current rules and reports
# the resulting rating as exit status
#
# NOTE:
#   the rating is kept in a file beside the exit log, never in the log
#   itself: omnect_service_log.sh appends to that log from a systemd
#   StopPost hook while a check may run at the same time, and a rater
#   replacing the log would drop whatever was recorded in between -- which
#   is precisely the crash evidence the log exists for
function rate_servicelog() {
    local n="$1" servicelog="$2"
    local jquery rated tmprated fields source ratedfrom red yellow attempt

    rated="${servicelog}.rated"

    source=$(input_identity "$servicelog" "$CFGFILEPATH")
    ratedfrom=""
    if [ -r "$rated" ]; then
        fields=$(read_rating "$rated") \
            && IFS=$'\037' read -r ratedfrom red yellow <<<"$fields"
    fi

    # re-rate unless the rating at hand was derived from exactly the files
    # that are there now, because new entries or changed rules may change the
    # outcome
    #
    # NOTE:
    #   a rating records what it was made from, not when it was made: a rater
    #   that reads the log before an entry is appended may well put its
    #   result in place afterwards, and a timestamp would then make that
    #   result look like it covered the new entry, hiding a crash for good
    if [ "$ratedfrom" != "$source" ]; then
        # a file may be written while it is being read, so the result may
        # describe a state that never was; a few retries settle that, and
        # should they not, the rating is not put in place at all
        for attempt in 1 2 3; do
            # NOTE:
            #   the rules are read inside the retry, because an attempt that
            #   starts over does so with the identity of the configuration it
            #   found, and rules read before that change would then be
            #   stamped as if they were the ones in force -- every later
            #   check would believe a rating the current rules do not justify
            #
            # a service may bring its own ratings, otherwise the defaults
            # apply; either way an entry no rule matched is rated red rather
            # than being left unrated, which would make it count as green
            #
            # ratings of an earlier run are dropped first, because they were
            # derived from rules that may have been changed or removed since:
            # keeping them would let a rating survive that the current rules
            # do not justify anymore, and shield the entry from the red
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
            #   a concurrent run -- the systemd StopPost hook rates a single
            #   service while a manual run rates all of them -- must not see
            #   a half written rating, so it is assembled under a unique name
            #   and put in place by a rename, which is atomic
            tmprated=$(mktemp "${rated}.XXXXXX") \
                || { error "no place to rate \"$servicelog\" in"; return 2; }

            # mktemp creates the file private to its owner, but the rating
            # has to stay as accessible as the log it describes, or a reader
            # lacking the permission would take an unreadable rating for no
            # findings
            chmod "$(stat -c %a "$servicelog")" "$tmprated" \
                || { error "rating of \"$servicelog\" could not be made accessible"; rm -f "$tmprated"; return 2; }

            # NOTE:
            #   the rating is put in place only if jq succeeded, because a
            #   truncated or empty file would be counted as no findings
            if ! jq --arg src "$source" "$jquery"' | ."rated-from" = $src' \
                    "$servicelog" > "$tmprated"; then
                error "exit log \"$servicelog\" could not be rated"
                # drop an outdated rating so this log cannot pass as rated
                rm -f "$tmprated" "$rated"
                return 2
            fi

            fields=$(input_identity "$servicelog" "$CFGFILEPATH")
            [ "$fields" = "$source" ] && break

            rm -f "$tmprated"
            source="$fields"
            tmprated=""
        done

        [ -n "$tmprated" ] || { error "exit log \"$servicelog\" changed while it was rated"; return 2; }

        mv "$tmprated" "$rated"

        fields=$(read_rating "$rated") \
            || { error "rating of \"$servicelog\" could not be read"; return 2; }
        IFS=$'\037' read -r ratedfrom red yellow <<<"$fields"
    fi

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
