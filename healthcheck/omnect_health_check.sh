#!/bin/bash

HEALTHCHECK_DIR=/usr/lib/omnect/healthcheck

. ${HEALTHCHECK_DIR}/healthchecklib.sh

CFGFILE=omnect_health_checks.json
CFGFILEDIR=/etc/omnect/health_check
CFGFILEPATH="${CFGFILEDIR}/${CFGFILE}"

function verify_check_names() {
    local cfgfile="$1"
    shift
    local check known=() rawknown unknown_checks=()

    # nothing to verify, and no need to read the config file at all
    [ $# -gt 0 ] || return 0

    # a config file that cannot be read tells nothing about the check names,
    # so it is reported as what it is instead of as an unknown name
    # NOTE:
    #   the line breaks are replaced without a regular expression, because
    #   omnect OS builds jq without oniguruma and the regex builtins are not
    #   available there
    rawknown=$(jq -r 'def flat: tostring | split("\n") | join(" ") | split("\r") | join(" ");
                      [ .[] | (.name // empty), (.type // empty) ]
                      | unique | .[] | flat' "$cfgfile") \
	|| { error "config file \"$cfgfile\" could not be read"; return 2; }
    [ "$rawknown" ] && mapfile -t known <<<"$rawknown"

    for check; do
	in_list "$check" "${known[@]}" || unknown_checks+=("$check")
    done

    # one name per line, so a name containing a blank stays one name
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
    local nchecked nchecks check i name type extra_args
    local output retval entries entry rawentries

    nchecked=0
    # read all entries in a single pass to keep the number of jq invocations
    # independent of the number of configured checks
    #
    # NOTE:
    #   the unit separator joins the attributes of an entry because IFS
    #   collapses consecutive tabs or blanks, which would make an entry
    #   without a "type" attribute look like one that has it
    #
    # NOTE:
    #   an entry is framed by a line, so a line break a JSON string carries
    #   is replaced by a blank: it would otherwise end the entry early and
    #   turn the remainder of the attribute into an entry of its own
    #
    # NOTE:
    #   jq's exit status decides here: on a malformed config file it prints
    #   the entries it could read before giving up, and checking only those
    #   would rate a device whose remaining checks never ran
    rawentries=$(jq -r 'def flat: tostring | split("\n") | join(" ") | split("\r") | join(" "); .[] | [ (."type" // "" | flat), (."name" // "" | flat), (."extra-args" // "" | flat) ] | join("\u001f")' "$cfgfile") \
	|| { error "config file \"$cfgfile\" could not be read"; do_rate 2; return 2; }

    if [ "$rawentries" ]; then
        mapfile -t entries <<<"$rawentries"
    else
        entries=()
    fi
    nchecks=${#entries[@]}
    [ $nchecks -gt 0 ] \
	|| warn "no checks defined in config file \"$cfgfile\""
    
    i=0
    for entry in "${entries[@]}"; do
        i=$((i + 1))

        IFS=$'\037' read -r type name extra_args <<<"$entry"
        : ${name:=$type}

        # an entry nobody asked for must not rate this run, no matter whether
        # it could be run at all; an entry without a type may still carry the
        # name it was selected by
        if [ ${#selected_checks[@]} -gt 0 ] \
           && ! in_list "$name" "${selected_checks[@]}" \
           && ! in_list "$type" "${selected_checks[@]}"; then
            continue
        fi

        [ "$type" ] \
            || { error "entry $i in \"$cfgfile\" is missing \"type\" attribute"; do_rate 2; nchecked=$((nchecked + 1)); continue; }

        check="${HEALTHCHECK_DIR}/omnect_health__${type}.sh"
        if [ ! -x "$check" ]; then
            local rate=2
            error "check \"$check\" for type \"$type\" (entry $i in \"$cfgfile\") cannot be called"
            print_rating $rate ${type} "${check##*/}"
            do_rate $rate
            nchecked=$((nchecked + 1))
            continue
        fi

        output=$($check $cmd $extra_args)
        retval=$?

        if [ $output_when = 2 -o $output_when = 1 -a $retval != 0 ]; then
            echo "$output"
        fi
	[ "$retval" -le 2 ] || print_rating 2 "$type" "${check##*/}"
	do_rate $retval "${check##*/}"
        nchecked=$((nchecked + 1))

    done

    # a device nothing was checked on is not healthy, it is unknown
    [ $nchecked -gt 0 ] \
	|| { error "no check was run, health of this device is unknown"; do_rate 1; }
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

[ -r "${CFGFILEPATH}" ] || { fatal "configuration file \"${CFGFILEPATH}\" not found"; exit 1; }

# first argument must be either "check" or "get-infos" other arguments are
# optional and contain names of checks
unknown=$(verify_check_names "${CFGFILEPATH}" "$@")
case $? in
    0) ;;
    2) exit 2 ;;
    *)
	mapfile -t unknown_checks <<<"$unknown"
	printf -v unknown_list '%s, ' "${unknown_checks[@]}"
	error "unknown check name(s) [${unknown_list%, }]"
	exit 1
	;;
esac
case "$command" in
    check)
	do_checks "${CFGFILEPATH}" "$@"
	retval=$?
	;;
    get-infos)
	do_get_infos "${CFGFILEPATH}" "$@"
	;;
esac

get_overall_rating
ret=$?

[ "$command" = check ] && echo -e "\nOVERALL: $(print_rating)"

exit $ret
