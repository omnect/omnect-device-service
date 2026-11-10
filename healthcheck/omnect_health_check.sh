#!/bin/bash

HEALTHCHECK_DIR=/usr/lib/omnect/healthcheck

. "${HEALTHCHECK_DIR}/healthchecklib.sh"

CFGFILE=omnect_health_checks.json
CFGFILEDIR=/etc/omnect/health_check
CFGFILEPATH="${CFGFILEDIR}/${CFGFILE}"

# the configured entries, read once: a second read could see a file that was
# replaced in between, and rate a run by entries the checks never saw
entries=()

# reads every configured entry into a line of its own, with its attributes
# joined by the unit separator
#
# NOTE:
#   the unit separator joins the attributes because IFS collapses consecutive
#   tabs or blanks, which would make an entry without a "type" attribute look
#   like one that has it
#
# NOTE:
#   an entry is framed by a line, so a line break a JSON string carries is
#   replaced by a blank: it would otherwise end the entry early and turn the
#   remainder of the attribute into an entry of its own
#
# NOTE:
#   the line breaks are replaced without a regular expression, because omnect
#   OS builds jq without oniguruma and the regex builtins are not available
#   there
#
# NOTE:
#   jq's exit status decides here: on a malformed config file it prints the
#   entries it could read before giving up, and checking only those would rate
#   a device whose remaining checks never ran
#
# NOTE:
#   the document is read through "input", so that an empty file - no JSON
#   document at all, and a common result of a power loss during a write -
#   is an error
function read_entries() {
    local cfgfile="$1" rawentries

    rawentries=$(jq -n -r 'input | def flat: tostring | split("\n") | join(" ") | split("\r") | join(" "); .[] | [ (."type" // "" | flat), (."name" // "" | flat), (."extra-args" // "" | flat) ] | join("\u001f")' "$cfgfile") \
        || return 2

    [ "$rawentries" ] && mapfile -t entries <<<"$rawentries"

    return 0
}

# reports every name the caller asked for that none of the entries carries,
# one name per line; returns 1 when it found any
function verify_check_names() {
    local check entry type name known=()

    [ $# -gt 0 ] || return 0

    for entry in "${entries[@]}"; do
        IFS=$'\037' read -r type name _ <<<"$entry"
        [ -z "$type" ] || known+=("$type")
        [ -z "$name" ] || known+=("$name")
    done

    local unknown_checks=()
    for check; do
        in_list "$check" "${known[@]}" || unknown_checks+=("$check")
    done

    [ ${#unknown_checks[@]} -gt 0 ] || return 0
    printf '%s\n' "${unknown_checks[@]}"

    return 1
}

function do__cmd() {
    local cmd="$1"
    local output_when="$2" # 0 = never, 1 = failed only, 2 = all
    local cfgfile="$3"
    shift 3
    local selected_checks=("$@")
    local nchecked check i name type extra_args
    local output retval entry

    nchecked=0

    i=0
    for entry in "${entries[@]}"; do
        i=$((i + 1))

        IFS=$'\037' read -r type name extra_args <<<"$entry"
        : "${name:=$type}"

        # only entries the caller asked for are run and rated; an entry
        # without a type can still be selected by its name
        if [ ${#selected_checks[@]} -gt 0 ] \
           && ! in_list "$name" "${selected_checks[@]}" \
           && ! in_list "$type" "${selected_checks[@]}"; then
            continue
        fi

        # an entry without a type is rated red and named by its name or by
        # its position
        [ "$type" ] \
            || { error "entry $i in \"$cfgfile\" is missing \"type\" attribute"; print_rating "$RATING_RED" "${name:-entry $i}" "${cfgfile##*/}"; do_rate "$RATING_RED"; nchecked=$((nchecked + 1)); continue; }

        check="${HEALTHCHECK_DIR}/omnect_health__${type}.sh"
        if [ ! -x "$check" ]; then
            local rate="$RATING_RED"
            error "check \"$check\" for type \"$type\" (entry $i in \"$cfgfile\") cannot be called"
            print_rating "$rate" "$name" "${check##*/}"
            do_rate "$rate"
            nchecked=$((nchecked + 1))
            continue
        fi

        output=$("$check" "$cmd" $extra_args)
        retval=$?

        if [ -n "$output" ] \
           && [ "$output_when" = 2 -o "$output_when" = 1 -a "$retval" != 0 ]; then
            echo "$output"
        fi

        # a failing check that printed nothing is named here, so every
        # non-green rating has a line of its own
        if [ "$retval" -gt "$RATING_RED" ]; then
            print_rating "$RATING_RED" "$name" "${check##*/}"
        elif [ "$retval" -ne 0 -a -z "$output" ]; then
            print_rating "$retval" "$name" "${check##*/}"
        fi
        do_rate "$retval" "${check##*/}"
        nchecked=$((nchecked + 1))

    done

    # a device nothing was checked on is not healthy, it is unknown
    [ "$nchecked" -gt 0 ] \
        || { error "no check was run, health of this device is unknown"; print_rating "$RATING_YELLOW" "${cfgfile##*/}" "${ME}"; do_rate "$RATING_YELLOW"; }
}

function do_checks() {
    local cfgfile="$1"
    shift

    do__cmd check 2 "$cfgfile" "$@"
}

function do_get_infos() {
    local cfgfile="$1"
    shift

    do__cmd get-infos 1 "$cfgfile" "$@"
}

# let's set "check" as default command and allow parameter-less invocation
command="${1:-check}"
[ "$1" ] && shift
check_command_arg "$command"

# a missing config file is as fatal as an unreadable one, and is rated the
# same way: no check could run, so the rating has to say so where it is read
if [ ! -r "${CFGFILEPATH}" ]; then
    fatal "configuration file \"${CFGFILEPATH}\" not found"
    print_rating "$RATING_RED" "${CFGFILE}" "${ME}"
    do_rate "$RATING_RED"
elif read_entries "${CFGFILEPATH}"; then
    # the arguments following the command are optional and name the checks to
    # be run
    if unknown=$(verify_check_names "$@"); then
        case "$command" in
            check)
                do_checks "${CFGFILEPATH}" "$@"
                ;;
            get-infos)
                do_get_infos "${CFGFILEPATH}" "$@"
                ;;
        esac
    else
        mapfile -t unknown_checks <<<"$unknown"
        printf -v unknown_list '%s, ' "${unknown_checks[@]}"
        error "unknown check name(s) [${unknown_list%, }]"
        # the caller named checks this device does not have, so nothing was
        # checked and the health of the device is unknown rather than bad
        print_rating "$RATING_YELLOW" "${CFGFILE}" "${ME}"
        do_rate "$RATING_YELLOW"
    fi
else
    # without the entries there is nothing to run, so the run is rated red
    # and the config file is named as the cause
    error "config file \"${CFGFILEPATH}\" could not be read"
    print_rating "$RATING_RED" "${CFGFILE}" "${ME}"
    do_rate "$RATING_RED"
fi

get_overall_rating
ret=$?

[ "$command" = check ] && echo -e "\nOVERALL: $(print_rating)"

exit "$ret"
