#
# Gather all common helper functions in this file
#

ME="${0##*/}"

function print() {
    echo -e "$@"
}

function msg() {
    print "${ME}: ${@}"
}

function warn() {
    echo -e "${ME}: WARNING: ${@}" >&2
}

function error() {
    echo -e "${ME}: ERROR: ${@}" >&2
}

function fatal() {
    echo -e "${ME}: FATAL ERROR: ${@}" >&2
}

# true if $1 equals one of the remaining arguments
#
# NOTE:
#   a check name may contain a blank, so the list comes as separate
#   arguments
function in_list() {
    local needle="$1" item
    shift

    for item; do
        [ "$item" = "$needle" ] && return 0
    done

    return 1
}

# put in front of a jq program that reads a config file with "jq -n": the
# file has to hold exactly one JSON document, so an empty file, a second
# document and content after the first one are errors
# shellcheck disable=SC2034  # used by the scripts that source this file
JQ_ONE_DOCUMENT='[inputs] | if length != 1 then error("the file must hold exactly one JSON document") else .[0] end'

# a rating is also the exit status of a check, so its values are fixed
RATING_GREEN=0
RATING_YELLOW=1
RATING_RED=2

strrating=("GREEN" "YELLOW" "RED")
overall_rating="$RATING_GREEN"

function print_rating() {
    local rating="${1:-${overall_rating}}"
    local info="$2"
    local file="$3"
    local ratingstr str
    local extrainfo="${file:+  (${file})}"

    printf -v ratingstr "%-9s" "[${strrating[${rating}]}]"
    printf -v str " %s %-25s  " "${ratingstr}" "$info"

    print "${str}${extrainfo}"
}

function do_rate() {
    local rating="$1"
    local hint="$2"

    case "X$rating" in
        "X$RATING_GREEN")
            : # everything fine here
        ;;
        "X$RATING_YELLOW")
            [ "$overall_rating" -ge "$RATING_YELLOW" ] || overall_rating="$RATING_YELLOW"
            ;;
        "X$RATING_RED")
            [ "$overall_rating" -ge "$RATING_RED" ] || overall_rating="$RATING_RED"
            ;;
        *)
            fatal "Unknown rating \"$rating\" ${hint:-${ME}} - treated as ${RATING_RED} == ${strrating[$RATING_RED]}"
            overall_rating="$RATING_RED"
            ;;
    esac
}

# prints a rating and adds it to the overall rating, so that the rating a
# reader sees is the one that was counted
function print_and_rate() {
    local rating="$1"
    shift

    print_rating "$rating" "$@"
    do_rate "$rating"
}

function do_rate_cmd() {
    local ret

    "$@"
    ret=$?
    do_rate "$ret" "$1"
    return "$ret"
}

function check_command_arg() {
    local cmd="$1"

    # first argument must be either "check" or "get-infos"
    case "X$1" in
        Xcheck | Xget-infos)
        ;;
        X)
            fatal "no command given"
            exit 1
            ;;
        *)
            fatal "unrecognized command \"$1\""
            exit 1
            ;;
    esac
}

function print_info_header() {
    local title="${1:-$ME}"
    local rating="${2:-$overall_rating}"
    echo "---[ ${title}:${strrating[$rating]} ]---"
}

function get_overall_rating() {
    return "$overall_rating"
}
