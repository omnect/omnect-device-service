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
#   the list is passed as separate arguments rather than as one string,
#   because a check or service name may contain a blank, which splitting a
#   string would turn into two names
function in_list() {
    local needle="$1" item
    shift

    for item; do
        [ "$item" = "$needle" ] && return 0
    done

    return 1
}

strrating=("GREEN" "YELLOW" "RED")
overall_rating=0

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
        X0)
            : # everything fine here
        ;;
        X1)
            [ $overall_rating -ge 1 ] || overall_rating=1
            ;;
        X2)
            [ $overall_rating -ge 2 ] || overall_rating=2
            ;;
        *)
            fatal "Unknown rating \"$rating\" ${hint:-${ME}} - treated as 2 == RED"
            overall_rating=2
            ;;
    esac
}

function do_rate_cmd() {
    local ret

    "$@"
    ret=$?
    do_rate $ret "$1"
    return $ret
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
    return $overall_rating
}
