#!/bin/bash

HEALTHCHECK_DIR=/usr/lib/omnect/healthcheck

. ${HEALTHCHECK_DIR}/healthchecklib.sh

CFGFILE=omnect_health_checks.json
CFGFILEDIR=/etc/omnect/health_check
CFGFILEPATH="${CFGFILEDIR}/${CFGFILE}"

function verify_check_names() {
    local cfgfile="$1"
    shift
    local check known_checknames known_checktypes unknown_checks retval

    retval=0

    # nothing to verify, and no need to read the config file at all
    [ $# -gt 0 ] || return 0

    known_checknames=$(jq -r '[ .[].name ] | unique | join(" ")' "$cfgfile")
    known_checktypes=$(jq -r '[ .[].type ] | unique | join(" ")' "$cfgfile")

    for check; do
	in_list "$check" "$known_checknames" && continue
	in_list "$check" "$known_checktypes" && continue

	retval=1
	unknown_checks="${unknown_checks:+${unknown_checks} }${check}"
    done

    echo "$unknown_checks"
    [ "$unknown_checks" ] && retval=1
    
    return $retval
}

function do__cmd() {
    local cmd="$1"
    local output_when="$2" # 0 = never, 1 = failed only, 2 = all
    local cfgfile="$3"
    local selected_checks="$4"
    local nchecked nchecks check i name type extra_args
    local output retval entries entry

    nchecked=0
    # one pass over the config file instead of three jq calls per entry;
    # the unit separator is used because IFS would collapse empty tab
    # separated fields, which would hide a missing "type" attribute
    mapfile -t entries < <(jq -r '.[] | [ (."type" // "" | tostring), (."name" // "" | tostring), (."extra-args" // "" | tostring) ] | join("\u001f")' "$cfgfile")
    nchecks=${#entries[@]}
    [ $nchecks -gt 0 ] \
	|| warn "no checks defined in config file \"$cfgfile\""
    
    i=0
    for entry in "${entries[@]}"; do
        i=$((i + 1))

        IFS=$'\037' read -r type name extra_args <<<"$entry"

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
        : ${name:=$type}
        if [ "$selected_checks" ] \
           && ! in_list "$name" "$selected_checks" \
           && ! in_list "$type" "$selected_checks"; then
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
    local selected_checks="$2"

    do__cmd check 2 "$cfgfile" "$2"
}

function do_get_infos() {
    local cfgfile="$1"
    local selected_checks="$2"

    do__cmd get-infos 1 "$cfgfile" "$2"
}

# let's set "check" as default command and allow parameter-less invocation
command="${1:-check}"
[ "$1" ] && shift
check_command_arg "$command"

[ -r "${CFGFILEPATH}" ] || { fatal "configuration file \"${CFGFILEPATH}\" not found"; exit 1; }

# first argument must be either "check" or "get-infos" other arguments are
# optional and contain names of checks
unknown=$(verify_check_names "${CFGFILEPATH}" "$@")
[ $? = 0 ] \
    || { error "unknown check name(s) [${unknown// /, }]"; exit 1; }
case "$command" in
    check)
	do_checks "${CFGFILEPATH}" "$*"
	retval=$?
	;;
    get-infos)
	do_get_infos "${CFGFILEPATH}" "$*"
	;;
esac

get_overall_rating
ret=$?

[ "$command" = check ] && echo -e "\nOVERALL: $(print_rating)"

exit $ret
