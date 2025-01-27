#!/bin/bash

# usage omnect_pmsg_to_reboot_reason.sh <console-file> <pmsg-file> <dmsg-file>
#
# we now need to analyze the situation:
#  - console-ramoops-0
#    -> will always exist
#  - dmesg-ramoops-0
#    -> definitely means that a crash happened
#    -> but this might have been during an intentional reboot
#  - pmsg-ramoops-0
#    -> exists only upon regular reboots or reboot attempts
#
# that means, we need to prioritize what to report:
# 1. a crash is a crash, whatever the circumstances
#    -> existence of dmesg file wins!
#    -> extra-info can be provided for pmsg info
# 2. intentional reboots
#   -> existence of pmsg file tells us more
#   -> analyze pmsg file and deduce reboot reason
# 3. blackouts/brown-outs/power-cycles
#   -> empty /sys/fs/pstore directory because nothing survives in plain
#      RAM w/o power
# 4. unexpected/unrecognized reboot
#    -> existence of console file without dmesg and pmsg files
#    -> circumstances unclear
#       (maybe reset by PMIC or reset button w/o involvement of watchdog, or
#        watchdog reset not recorded in dmesg buffer?)
#    -> needs more investigation
#
# NOTE:
#   it is possible that above assumptions don't hold true for some reboot
#   causes, so this willprobably subject to refinements in future
#
# Now that we determined the reboot reason, we need to arrange information so
# that some other instance can digest it.
#
# Default path to store this information: /var/lib/omnect/reboot-reason
# For every boot ...
#  - a directory with a timestamp of the analysis is created
#  - all available pstore files are copied into it and get compressed
#  - a file reboot-reason.json is created with appropriate contents
#
# JSON structure of reboot reason file is like this:
#
# {
#     "report": {
#         "datetime":      "<YYYY-MM-DD HH:mm:ss>",
#         "timeepoch":     "<seconds-since-1970>",
#         "uptime":        "<uptime-of-report>",
#         "boot_id":       "<current-boot_id>",
#         "console_file:"  "<console-file-name-if-any>",
#         "dmesg_file:"    "<dmesg-file-name-if-any>",
#         "pmsg_file:"     "<pmsg-file-name-if-any>"
#     },
#     "reboot_reason": {
#         "datetime":      "<datetime-of-logged-reboot-event-if-any>",
#         "timeepoch":     "<timeepoch-of-logged-reboot-event-if-any>",
#         "uptime":        "<uptime-of-logged-reboot-event-if-any>",
#         "boot_id":       "<boot_id-of-logged-reboot-event-if-any>",
#         "reason":        "<deduced-reason>",
#         "extra_info":    "<extra-info-of-logged-reboot-event-if-any>"
#     }
# }
#
# struct "report" gathers information /wrt reboot reason file generation.
# it could be used for checking reboot history.
#
# deduced reboot reasons are:
#  - reboot
#    -> plain reboot without further information about who or why
#  - shutdown
#    -> shutdown; unlikely to be seen unless an external reset mechanism
#       exists which leaves pstore intact
#  - swupdate
#    -> reboot after SW update installation
#  - swupdate-validation-failed
#    -> reboot after validation of SW update installation failed
#  - systemd-networkd-wait-online
#    -> systemd service didn't successfully come up, e.g. due to no internet
#       access
#  - system-crash
#    -> reboot after system panic
#  - power-loss
#    -> pstore is completely empty
#    NOTE: if RAM is not stable during reboot, this will the only reboot reason
#          deduced regardless of the reboot circumstances!
#  - unrecognized
#    if examination of files didn't yield something unambiguous, this reason
#    is used, together with additional hints in extra-info field
#

RAMOOPS_FILENAME_POSTFIX=-ramoops-0
PSTORE_DFLT_DIR=/sys/fs/pstore
REASON_DFLT_DIR=/var/lib/omnect/reboot-reason
REASON_ANALYSIS_FILE=reboot-reason.json

# output directory for reboot reason file
: ${REASON_DIR:=${REASON_DFLT_DIR}}

# file containing console logs of ast boot
CONSOLE_DFLT_FILE="console${RAMOOPS_FILENAME_POSTFIX}"
CONSOLE_FILE="${1:-${PSTORE_DFLT_DIR}/${CONSOLE_DFLT_FILE}}"

# file containing logs to /dev/pmsg0
PMSG_DFLT_FILE="pmsg${RAMOOPS_FILENAME_POSTFIX}"
PMSG_FILE="${2:-${PSTORE_DFLT_DIR}/${PMSG_DFLT_FILE}}"

# file containing logs to /dev/pmsg0
DMESG_DFLT_FILE="dmesg${RAMOOPS_FILENAME_POSTFIX}"
DMESG_FILE="${3:-${PSTORE_DFLT_DIR}/${DMESG_DFLT_FILE}}"

# we need to treat pmsg content different if ECC is enabled on ramoops, because
# the last line returned by read from sysfs file contains an ECC status
ecc_enabled="$(</sys/module/ramoops/parameters/ecc)"

function err() {
    local exitval="${1:-1}"
    shift
    local msg="$*"

    [ "$msg" ] || msg="unspecified"

    >&2 echo "ERROR: $msg"
    exit $exitval
}

function copy_and_compress_file() {
    local srcpath="$1"
    local dstpath="$2"
    local del_after_copy="$3"
    local dont_compress="$4"
    local ecc_quirk="$5"
    local srcfile=$(basename "$srcpath")
    local retval

    [ -f "${dstpath}" ] || dstpath=$(realpath "${dstpath}/${srcfile}")

    if [ "${ecc_quirk}" ]; then
	sed '$d' "${srcpath}" > "${dstpath}"
    else
	cp "${srcpath}" "${dstpath}"
    fi
    retval=$?
    [ $retval = 0 ] || err 1 "Copying file failed: ${srcpath} -> ${dstpath} [ecc_quirk:${ecc_quirk}]"

    if [ -z "${dont_compress}" ]; then

	gzip "${dstpath}"
	retval=$?
	if [ $retval = 0 ]; then
	    [ -f "${dstpath}" ] || dstpath="${dstpath}.gz"
	else
	    err 1 "Compressing copied file failed: ${srcpath} -> ${dstpath}"
	fi
    fi

    [ "${del_after_copy}" ] && rm "${srcpath}"

    # at last return destination file
    realpath "${dstpath}"
}

# three possible command line arguments ware already processed above, so
# start looking for input files ...

[ -r "${CONSOLE_FILE}" ] || CONOSLE_FILE=
[ -r "${DMESG_FILE}"   ] || DMESG_FILE=
[ -r "${PMSG_FILE}"    ] || PMSG_FILE=

# determine current time, uptime and other stuff for the first part of
# the reboot reason JSON file
boot_id="$(</proc/sys/kernel/random/boot_id)"
remIFS="${IFS}"
IFS=, time=( $(date +%F\ %T,%s) )
IFS="${remIFS}"
datetime="${time[0]}"
timeepoch="${time[1]}"
uptime="$(set -- $(cat /proc/uptime); echo $1)"

# convert timestamp into dir name w/o potential for trouble
reason_dirname="${datetime//:/-}"
reason_dirname="${reason_dirname// /_}"

reason_path="${REASON_DIR}/${reason_dirname}"
mkdir "${reason_path}"
retval=$?
[ $retval = 0 ] || err 1 "Could not create directory (${reason_path}) for reboot reason data [$retval]"

# now change to destination directory to process available data (if any)
cd "${reason_path}"

# at first, copy over all available pstore files
del_after_copy=
[ -r "${CONSOLE_FILE}" ] \
    && console_file=$(copy_and_compress_file "${CONSOLE_FILE}" "${PWD}" "${del_after_copy}")
[ -r "${DMESG_FILE}"   ] \
    && dmesg_file=$(copy_and_compress_file "${DMESG_FILE}"   "${PWD}" "${del_after_copy}")
[ -r "${PMSG_FILE}"    ] \
    && pmsg_file=$(copy_and_compress_file "${PMSG_FILE}"    "${PWD}" "${del_after_copy}" 1 "${ecc_enabled}")

# start with empty reason structure values later inserted into reason file
r_datetime=
r_timeepoch=
r_uptime=
r_boot_id=
r_reason=
r_extra_info=

if [ -r "${dmesg_file}" ]; then
    # highest priority: obviously a panic occured, so report it
    r_reason="system-crash"
    # FFS:
    #   can we obtain some reasonable extra information here?
    #   maybe from dmesg file itself or from console file?

    if [ -z "${r_extra_info}" -a -r "${pmsg_file}" ]; then
	# gather all recorded reboot reasons for extra info
	no_reasons=$(jq -s 'length' < "${pmsg_file}")
	r_extra_info="pmsg file with multiple (${no_reasons}) reason entries exists: ${all_reasons}"

	# use latest reboot record to fill reason; best approximation available
	# and at least boot_id is correct!
	r_datetime=$(jq -rs '[ .[] | ."datetime" ] | last' < "${pmsg_file}")
	r_timeepoch=$(jq -rs '[ .[] | ."timeepoch" ] | last' < "${pmsg_file}")
	r_uptime=$(jq -rs '[ .[] | ."uptime" ] | last' < "${pmsg_file}")
	r_boot_id=$(jq -rs '[ .[] | ."boot_id" ] | last' < "${pmsg_file}")
    fi
elif [ -r "${PMSG_FILE}" ]; then
    # we do have an annotated intentional reboot, so gather information
    #  - how many?
    no_reasons=$(jq -s 'length' < "${pmsg_file}")
    retval=$?
    [ $retval = 0 ] || err 1 "Coudln't determine number of reason logs in pmsg file (corrupted?)"

    # now we need to analyze them
    if [ $no_reasons = 1 ]; then
	# just use PMSG content
	r_datetime=$(jq -r '."datetime"' < "${pmsg_file}")
	r_timeepoch=$(jq -r '."timeepoch"' < "${pmsg_file}")
	r_uptime=$(jq -r '."uptime"' < "${pmsg_file}")
	r_boot_id=$(jq -r '."boot_id"' < "${pmsg_file}")
	r_reason=$(jq -r '."reason"' < "${pmsg_file}")
	r_extra_info=$(jq -r '."extra-info"' < "${pmsg_file}")
    elif [ $no_reasons = 0 ]; then
	err 1 "Unrecognized pmsg file contents (no reason elements found)"
    else
	# that might become tricky: we face several PMSG entries so let's see
	# what we could have here ...

	# 1. having reboot as last reason
	# here we assume the real reason to be contained in the next-to-last
	# entry
	last_reason=$(jq -rs  '[ .[] | .reason ] | last' < "${pmsg_file}")
	next_to_last_reason=$(jq -rs  '[ .[] | .reason ] | nth('$((no_reasons - 2))')' < "${pmsg_file}")

	if [ "${last_reason}" = "reboot" ]; then
	    # FIXME: what cases do we need to sort out here?
	    case "${next_to_last_reason}" in
		swupdate | swupdate-validation-failed)
		    r_reason="${next_to_last_reason}"
		    r_extra_info="reboot after ${next_to_last_reason}"
		    ;;
	    esac
	    if [ "$r_reason" ]; then
		# now that we determined a reboot reason, gather all other info
		# from last entry
		r_datetime=$(jq -rs '[ .[] | ."datetime" ] | last' < "${pmsg_file}")
		r_timeepoch=$(jq -rs '[ .[] | ."timeepoch" ] | last' < "${pmsg_file}")
		r_uptime=$(jq -rs '[ .[] | ."uptime" ] | last' < "${pmsg_file}")
		r_boot_id=$(jq -rs '[ .[] | ."boot_id" ] | last' < "${pmsg_file}")
	    fi
	fi

	# if resulting reason is still not set do it now and provide more info
	if [ -z "${r_reason}" ]; then
	    r_reason="unrecognized"
	    all_reasons=$(jq -rs 'map(.reason) | join(", ")' < "${pmsg_file}")
	    r_extra_info="multiple (${no_reasons}) reason entries found in pmsg file: ${all_reasons}"
	fi
    fi
elif [ -r "${console_file}" ]; then
    r_reason="unrecognized"
    r_extra_info="console file w/o pmsg file"
else
    r_reason="power-loss"
fi

# at last output reboot reason file
jq \
    -n \
    --arg report_boot_id "${boot_id}" \
    --arg report_datetime "${datetime}" \
    --arg report_uptime "${uptime}" \
    --arg report_timeepoch "${timeepoch}" \
    --arg report_console_file "${console_file}" \
    --arg report_dmesg_file "${dmesg_file}" \
    --arg report_pmsg_file "${pmsg_file}" \
    --arg r_datetime "${r_datetime}" \
    --arg r_timeepoch "${r_timeepoch}" \
    --arg r_uptime "${r_uptime}" \
    --arg r_boot_id "${r_boot_id}" \
    --arg r_reason "${r_reason}" \
    --arg r_extra_info "${r_extra_info}" \
    '{
        "report": {
            "datetime":     $report_datetime,
            "timeepoch":    $report_timeepoch,
            "uptime":       $report_uptime,
            "boot_id":      $report_boot_id,
            "console_file": $report_console_file,
            "dmesg_file":   $report_dmesg_file,
            "pmsg_file":    $report_pmsg_file,
        },
        "reboot_reason": {
            "datetime":    $r_datetime,
            "timeepoch":   $r_timeepoch,
            "uptime":      $r_uptime,
            "boot_id":     $r_boot_id,
            "reason":      $r_reason,
            "extra_info":  $r_extra_info,
        }
    }' | tee reboot-reason.json
