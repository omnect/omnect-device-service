#!/bin/bash
#
# usage:
#    omnect_reboot_reason.sh get [ <console-file> <pmsg-file> <dmsg-file> ]
#    omnect_reboot_reason.sh log <reason> [ <extra-info> ... ]
#
# A) omnect_reboot_reason.sh get [ <console-file> <pmsg-file> <dmsg-file> ]
#
# analyze current situation in order to deduce reboot reason:
#  - console-ramoops-0
#     or
#  - /sys/firmware/efi/efivars/boot-marker-53d7f47e-126f-bd7c-d98a-5aa0643aa921
#    -> ramoops file will always exist unless update from image not yet
#       supporting reboot reason feature
#    -> EFI bootmarker file only exists if boot tagging service ran
#       successfully; this means that reboots can go unnoticed if happening
#       before
#  - /sys/fs/pstore/dmesg-ramoops-0
#     or
#  - /sys/fs/pstore/dmesg-efi-XXXXXX
#    -> definitely means that a crash happened
#       (possibly on the way to an intentional reboot, though)
#  - /sys/fs/pstore/pmsg-ramoops-0
#     or
#  - /sys/firmware/efi/efivars/reboot-reason-53d7f47e-126f-bd7c-d98a-5aa0643aa921
#    -> exists only upon regular reboots or reboot attempts
#
# that means, we need to prioritize what to report:
# 1. a crash is a crash, whatever the circumstances
#    -> existence of dmesg file wins!
#    -> extra info can be provided for pmsg info
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
#   causes, so this will probably be subject to refinements in future.
#
# after determination of the reboot reason,  information needs to be arranged
# so that some other instance can digest it.
#
# Default path to store this information: /var/lib/omnect/reboot-reason
# For every boot ...
#  - a directory with a timestamp of the analysis is created
#  - all available pstore files/related EFI variable contents are copied into
#    it and get compressed
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
#         "os_version":    "<current-os-version>",
#         "console_file:"  "<console-file-name-if-any>",
#         "dmesg_file:"    "<dmesg-file-name-if-any>",
#         "pmsg_file:"     "<pmsg-file-name-if-any>"
#     },
#     "reboot_reason": {
#         "datetime":      "<datetime-of-logged-reboot-event-if-any>",
#         "timeepoch":     "<timeepoch-of-logged-reboot-event-if-any>",
#         "uptime":        "<uptime-of-logged-reboot-event-if-any>",
#         "boot_id":       "<boot_id-of-logged-reboot-event-if-any>",
#         "os_version":    "<os-version-of-logged-reboot>",
#         "reason":        "<deduced-reason>",
#         "extra_info":    "<extra-info-of-logged-reboot-event-if-any>"
#     }
# }
#
# structure "report" gathers information /wrt reboot reason file generation.
# it could be used for checking history of system reboots over time.
# structure "reboot_reason" is used in reboot reason result file (aka report
# or analysis) and identically for reason logging by this script.
#
# deduced reboot reasons are:
#  - reboot
#    -> plain reboot without further information about who or why
#  - shutdown
#    -> shutdown; unlikely to be seen unless an external reset mechanism
#       exists which leaves pstore intact
#  - ods-reboot
#    -> reboot initiated by means of omnect-device-service
#  - factory-reset
#    -> reboot after initiating factory reset
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
#    is used, together with additional hints in extra_info field
#
# B) omnect_reboot_reason.sh log <reason> [ <extra-info> ... ]
#
# log a reboot reason to be retrieved after sytem started anew.
#
# format is identical to structure "reboot_reason" in reboot reason result
# JSON file (see above in description of "get" command).
# multiple such entries can be logged, e.g. ...
#  - a standard reboot (via whatever means) will always log reason "reboot"
#    during system shutdown by a dedicated service
# - if that happens in the wake of a software update, the update process will
#   already have logged a reboot reason "swupdate" beforehand
#

# common variables
ME="${0##*/}"
REASON_DFLT_DIR=/var/lib/omnect/reboot-reason
: ${REASON_DIR:=${REASON_DFLT_DIR}}
REASON_ANALYSIS_FILE=reboot-reason.json
PSTORE_DFLT_DIR=/sys/fs/pstore

# variable below is to hold a time stamp which can be calculated once (by
# function get_timestamp()) and used from then on to have one unique timestamp
# whereever needed
declare -a timestamp

##################### common functions       #####################

# if in any function below a temporary file is needed, its location should be
# stored in variable tmpfile, so that cleanup function can take care of
# automatic removel upon exit
function cleanup() {
    [ "${tmpfile}" ] && rm -f "${tmpfile}"
    exit $rv
}

function err() {
    local exitval="${1:-1}"
    local msg="${*:2}"

    [ "$msg" ] || msg="unspecified"

    >&2 echo "ERROR: $msg"
    rv=$exitval
    exit
}

# NOTE: sets global variable timestamp which can be used from then on
function get_timestamp() {
    local IFS=,

    # only calculate timestamp once to yield uniform timestamp when multiply
    # used
    [ "${timestamp}" ] && return

    # index 0 has date and tim, index 1 time epoch
    timestamp=( $(date +%F\ %T,%s) )
}

# calculate reason directory name to place latest analysis into and create it
function get_new_reason_dir() {
    local basedir="$1"
    local timestamp="$2"
    local seqno reasondirname reasonpath

    # convert timestamp into dir name w/o potential for trouble
    timestamp="${timestamp//:/-}"
    timestamp="${timestamp// /_}"

    # now find next sequence number for new directory
    if [ -d "${basedir}" ]; then
	for path_entry in "${basedir}"/*; do
	    # make it work independently from globbing by conluding that all
	    # found entries still exist, because nobody else should bother with
	    # that directory
	    [ -e "${path_entry}" ] || break

	    local entry=$(basename "${path_entry}")
	    # format is something like NNNNNN+YYYY-mm-dd_HH-MM-SS
	    local no="${entry%%+*}"

	    # check format of no now
	    case "X${no}" in
		X | X[0-9]*[^0-9][0-9]*) continue;;  # ignored
	    esac

	    # now remove leading zeros
	    no="${no#${no%%[^0]*}}"
	    : ${no:=0}
	    [ -z "${seqno}" -o ${no} -gt ${seqno:-0} ] && seqno="${no}"
	done

	# we need to increment if we found a number!
	[ "${seqno}" ] && seqno=$((seqno + 1))
    fi

    : ${seqno:=0}

    reasondirname=$(printf '%06u+%s' "${seqno}" "${timestamp}")
    reasonpath="${basedir}/${reasondirname}"
    mkdir -p "${reasonpath}" \
	|| err 1 "Could not create directory (${reasonpath}) for reboot reason data [$retval]"

    echo "${reasonpath}"
}

# calculate reboot reason JSON structure to be logged as reason
function get_reboot_reason_data() {
    local reason="${1:-NOT-SET}"
    local extra_info="$2"
    local os_version=$(. /etc/os-release; echo "${VERSION}")

    cat <<EOF
{
    "datetime":   "${timestamp[0]}",
    "timeepoch":  "${timestamp[1]}",
    "uptime":     "$(set -- $(</proc/uptime); echo $1)",
    "boot_id":    "$(</proc/sys/kernel/random/boot_id)",
    "os_version": "${os_version}",
    "reason":     "${reason}",
    "extra_info": "${extra_info}"
}
EOF
}

# analyze situation by means of existence of files:
#  - dmesgfile: contains system crash information
#  - console file: contains either system logs (ramoops) or boot tag (efi)
#  - pmsg file with reboot reason hints
function analyze() {
    local basedir="$1"
    local dmesg_file="$2"
    local console_file="$3"
    local pmsg_file="$4"
    local datetime timeepoch uptime boot_id os_version reason
    local extra_info
    local r_datetime r_timeepoch r_uptime r_boot_id r_os_version r_reason
    local r_extra_info

    if [ -r "${dmesg_file}" ]; then
	# highest priority: obviously a panic occured, so report it
	r_reason="system-crash"
	# FFS:
	#   can we obtain some reasonable extra information here?
	#   maybe from dmesg file itself or from console file (if any)?
	#   or do we want to investigate on whether we were already about to
	#   reboot?

	if [ -z "${r_extra_info}" -a -r "${pmsg_file}" ]; then
	    # gather all recorded reboot reasons for extra info
	    no_reasons=$(jq -s 'length' < "${pmsg_file}")
	    all_reasons=$(jq -rs 'map(.reason) | join(", ")' < "${pmsg_file}")
	    r_extra_info="with multiple reasons exist: ${all_reasons} + ${r_reason}"

	    # use latest reboot record to fill reason; best approximation available
	    # and at least boot_id is correct!
	    r_datetime=$(jq -rs '[ .[] | ."datetime" ] | last' < "${pmsg_file}")
	    r_timeepoch=$(jq -rs '[ .[] | ."timeepoch" ] | last' < "${pmsg_file}")
	    r_uptime=$(jq -rs '[ .[] | ."uptime" ] | last' < "${pmsg_file}")
	    r_os_version=$(jq -rs '[ .[] | ."os_version" ] | last' < "${pmsg_file}")
	    r_boot_id=$(jq -rs '[ .[] | ."boot_id" ] | last' < "${pmsg_file}")
	fi
    elif [ -r "${pmsg_file}" ]; then
	# we do have an annotated intentional reboot, so gather information
	#  - how many?
	no_reasons=$(jq -s 'length' < "${pmsg_file}")
	retval=$?
	[ $retval = 0 ] || err 1 "Coudln't determine number of reason logs in pmsg file (corrupted?)"

	# now we need to analyze them
	if [ $no_reasons = 0 ]; then
	    err 1 "Unrecognized pmsg file contents (no reason elements found)"
	elif [ $no_reasons = 1 ]; then
	    # just use PMSG content
	    r_datetime=$(jq -r '."datetime"' < "${pmsg_file}")
	    r_timeepoch=$(jq -r '."timeepoch"' < "${pmsg_file}")
	    r_uptime=$(jq -r '."uptime"' < "${pmsg_file}")
	    r_boot_id=$(jq -r '."boot_id"' < "${pmsg_file}")
	    r_os_version=$(jq -r '."os_version"' < "${pmsg_file}")
	    r_reason=$(jq -r '."reason"' < "${pmsg_file}")
	    r_extra_info=$(jq -r '."extra_info"' < "${pmsg_file}")
	else
	    # that might become tricky: we face several PMSG entries so let's see
	    # what we could have here ...

	    # 1. having reboot as last reason
	    # here we assume the real reason to be contained in the next-to-last
	    # entry
	    last_reason=$(jq -rs  '[ .[] | .reason ] | last' < "${pmsg_file}")
	    next_to_last_reason=$(jq -rs  '[ .[] | .reason ] | nth('$((no_reasons - 2))')' < "${pmsg_file}")
	    next_to_last_extra_info=$(jq -rs  '[ .[] | .extra_info ] | nth('$((no_reasons - 2))')' < "${pmsg_file}")

	    if [ "${last_reason}" = "reboot" ]; then
		case "${next_to_last_reason}" in
		    swupdate | swupdate-validation-failed | factory-reset | portal-reboot | ods-reboot)
			r_reason="${next_to_last_reason}"
			r_extra_info="${next_to_last_extra_info}"
			if [ -z "${r_extra_info}" -o "null" = "${r_extra_info}" ]; then
			    r_extra_info="reboot after ${next_to_last_reason}"
			fi
			;;
		esac
		if [ "$r_reason" ]; then
		    # now that we determined a reboot reason, gather all other info
		    # from last entry
		    r_datetime=$(jq -rs '[ .[] | ."datetime" ] | last' < "${pmsg_file}")
		    r_timeepoch=$(jq -rs '[ .[] | ."timeepoch" ] | last' < "${pmsg_file}")
		    r_uptime=$(jq -rs '[ .[] | ."uptime" ] | last' < "${pmsg_file}")
		    r_boot_id=$(jq -rs '[ .[] | ."boot_id" ] | last' < "${pmsg_file}")
		    r_os_version=$(jq -rs '[ .[] | ."os_version" ] | last' < "${pmsg_file}")
		fi
	    fi

	    # 2. if resulting reason is still not set do it now and provide more info
	    if [ -z "${r_reason}" ]; then
		r_reason="unrecognized"
	    fi
	    if [ -z "${r_extra_info}" -o "null" = "${r_extra_info}" ]; then
		all_reasons=$(jq -rs 'map(.reason) | join(", ")' < "${pmsg_file}")
		r_extra_info="multiple (${no_reasons}) reason entries found in pmsg file: ${all_reasons}"
	    fi
	fi
    elif [ -r "${console_file}" ]; then
	r_reason="unrecognized"
	r_extra_info="console file w/o reboot reason indication file"
    else
	r_reason="power-loss"
    fi

    # - determine current time, uptime and other stuff for the first part of
    #   the reboot reason JSON file
    boot_id="$(</proc/sys/kernel/random/boot_id)"
    os_version=$(. /etc/os-release; echo "${VERSION}")
    datetime="${timestamp[0]}"
    timeepoch="${timestamp[1]}"
    uptime="$(set -- $(cat /proc/uptime); echo $1)"

    # - at last output reboot reason file with gathered field values
    jq \
	-n \
	--arg report_boot_id "${boot_id}" \
	--arg report_os_version "${os_version}" \
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
	--arg r_os_version "${r_os_version}" \
	--arg r_reason "${r_reason}" \
	--arg r_extra_info "${r_extra_info}" \
	'{
        "report": {
            "datetime":     $report_datetime,
            "timeepoch":    $report_timeepoch,
            "uptime":       $report_uptime,
            "boot_id":      $report_boot_id,
            "os_version":   $report_os_version,
            "console_file": $report_console_file,
            "dmesg_file":   $report_dmesg_file,
            "pmsg_file":    $report_pmsg_file,
        },
        "reboot_reason": {
            "datetime":    $r_datetime,
            "timeepoch":   $r_timeepoch,
            "uptime":      $r_uptime,
            "boot_id":     $r_boot_id,
            "os_version":  $r_os_version,
            "reason":      $r_reason,
            "extra_info":  $r_extra_info,
        }
    }' | tee "${basedir}/reboot-reason.json"
}

##################### pstore backend ramoops #####################

RAMOOPS_FILENAME_POSTFIX=-ramoops-0
RAMOOPS_CONSOLE_DFLT_FILE="${PSTORE_DFLT_DIR}/console${RAMOOPS_FILENAME_POSTFIX}"
RAMOOPS_DMESG_DFLT_FILE="${PSTORE_DFLT_DIR}/dmesg${RAMOOPS_FILENAME_POSTFIX}"
RAMOOPS_PMSG_DFLT_FILE="${PSTORE_DFLT_DIR}/pmsg${RAMOOPS_FILENAME_POSTFIX}"
RAMOOPS_DEV_PMSG=/dev/pmsg0

function ramoops_copy_file() {
    local srcpath="$1"
    local dstpath="$2"
    local del_after_copy="$3"
    local dont_compress="$4"
    local ecc_quirk="$5"
    local srcfile=$(basename "$srcpath")
    local retval

    [ -f "${dstpath}" ] || dstpath=$(realpath "${dstpath}/${srcfile}")

    set +e
    # NOTE:
    #   even though an ECC feature seems orthogonal to using special commands
    #   for copy or remove operations, is only available for ramoops
    if [ "${ecc_quirk}" ]; then
	sed '$d' "${srcpath}" > "${dstpath}"
    else
	cp "${srcpath}" "${dstpath}"
    fi
    retval=$?
    [ $retval = 0 ] || err 1 "Copying file failed: ${srcpath} -> ${dstpath} [ecc_quirk:${ecc_quirk}]"
    set -e

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

function reboot_reason_log_for_ramoops() {
    local json="$1"

    out=$(echo -n "${json}" 2>&1 > ${RAMOOPS_DEV_PMSG}) \
     || {
	>&2 echo -e "${ME}: logging to ${RAMOOPS_DEV_PMSG} had issues:\n${out}";
	exit 1
    }
}

function reboot_reason_get_for_ramoops() {
    local pmsg_file="${RAMOOPS_PMSG_FILE:-${RAMOOPS_PMSG_DFLT_FILE}}"
    local console_file="${RAMOOPS_CONSOLE_FILE:-${RAMOOPS_CONSOLE_DFLT_FILE}}"
    local dmesg_file="${RAMOOPS_DMESG_FILE:-${RAMOOPS_DMESG_DFLT_FILE}}"
    local reason_dir=$(get_new_reason_dir "${REASON_DIR}" "${timestamp[0]}")
    local ecc_enabled

    [ -r /sys/module/ramoops/parameters/ecc ] \
	&& ecc_enabled="$(</sys/module/ramoops/parameters/ecc)"

    [ "${console_file}" -a -r "${console_file}" ] || console_file=
    [ "${dmesg_file}"   -a -r "${dmesg_file}"   ] || dmesg_file=
    [ "${pmsg_file}"    -a -r "${pmsg_file}"    ] || pmsg_file=

    # copy over reboot reason files as available and replace variable content
    # with resulting files
    local del_after_copy=1
    [ "${console_file}" ] \
	&& console_file=$(ramoops_copy_file "${console_file}" "${reason_dir}" "${del_after_copy}")
    [ "${dmesg_file}"   ] \
	&& dmesg_file=$(ramoops_copy_file "${dmesg_file}" "${reason_dir}" "${del_after_copy}")
    [ "${pmsg_file}"    ] \
	&& pmsg_file=$(ramoops_copy_file "${pmsg_file}" "${reason_dir}" "${del_after_copy}" 1 "${ecc_enabled}")

    analyze "${reason_dir}" "${dmesg_file}" "${console_file}" "${pmsg_file}"
}

##################### pstore backend efi     #####################

# NOTE:
#   EFI vars always have a UUID component which can specify some vendor
#   or a dedicated purpose, but are often just random values.
#   here, use a UUID generated according to RFC 4122 Section 4.3
#   (https://www.rfc-editor.org/rfc/rfc4122#section-4.3) using
#   "omnect.io" and "reboot-reason" in SHA256 hashed string .
#   this means to hash string "io.omnect:reboot-reason" ...
#      echo -n "" | sha256sum
#   ... and take the first 128 bits of the resulting hash value for UUID
#   and format them properly:
#      t=( $(echo -n "io.omnect:reboot-reason" | sha256sum) )
#      uuid="${t:0:8}-${t:8:4}-${t:12:4}-${t:16:4}-${t:20:12}"
#   this results in the UUID as in file name defined below
EFIVARS_DFLT_DIR=/sys/firmware/efi/efivars
: ${SYSFS_DIR_EFIVARS:=${EFIVARS_DFLT_DIR}}
EFIVAR_UUID="53d7f47e-126f-bd7c-d98a-5aa0643aa921"
EFIVAR_PMSG_VAR="${EFIVAR_UUID}-reboot-reason"
EFIVAR_PMSG_DFLT_FILE="${SYSFS_DIR_EFIVARS}/reboot-reason-${EFIVAR_UUID}"
: ${EFIVAR_PMSG_FILE:=${EFIVAR_PMSG_DFLT_FILE}}
EFIVAR_BOOTTAG_VAR="${EFIVAR_UUID}-boottag"
EFIVAR_BOOTTAG_DFLT_FILE="${SYSFS_DIR_EFIVARS}/boottag-${EFIVAR_UUID}"
: ${EFIVAR_BOOTTAG_FILE:=${EFIVAR_BOOTTAG_DFLT_FILE}}
EFIVAR_DMESG_DFLT_DIR="${PSTORE_DFLT_DIR}"
: ${EFIVAR_DMESG_DIR:=${EFIVAR_DMESG_DFLT_DIR}}
EFIVAR_DMESG_VARNAME_PREFIX="dmesg-efi-"

# NOTE:
#   EFI variables are initially protected by kernel against (accidental)
#   deletion (amongst other things) via file attribute 'i' (immutable flag).
#   this means that we need to lift that restriction prior to deleting the
#   file and hence the variable
function rm_efivars() {
    local var
    for var; do
	chattr -i "${var}"
	rm "${var}"
    done
}

# NOTE:
#   EFI variables when read from sysfs contain a 4 byte header indicating the
#   variable's flags (in little endian notation); this needs to be stripped
#   in order to yield actual variable's content.
function read_efivar() {
    local src="$1"
    local dst="$2"

    # be prepared to get empty arguments for input and/or output to allow also
    # use in pipes
    dd ${src:+if="${src}"} ${dst:+of="${dst}"} bs=1 skip=4 2>/dev/null
}

function boottag_get_boot_ids() {
    local omit_cur_boot_id="${1}"
    local efivar_file="${EFIVAR_BOOTTAG_FILE}"
    local cur_boot_id="invalid"

    if [ ! -r "${efivar_file}" ]; then
	>&2 echo "WARNING: no EFI variable \"${efivar_file##*/}\" exists"
	return
    fi
    [ "${omit_cur_boot_id}" ] \
	&& cur_boot_id="$(</proc/sys/kernel/random/boot_id)"

    read_efivar "${efivar_file}" \
	| jq -sr --arg omit_id "${cur_boot_id}" 'map(select(.boottag != $omit_id) | .boottag) | join(" ")'
}

function boottag_write_boot_id() {
    local boot_id="${1:-$(</proc/sys/kernel/random/boot_id)}"
    local append="${2:+-a}"
    local efivar_var="${EFIVAR_BOOTTAG_VAR}"
    local efivar_file="${EFIVAR_BOOTTAG_FILE}"
    local tmpfile out

    # for efivar command write we unfortunately need a file
    tmpfile=$(mktemp -p /tmp boottag.XXXXXXXX 2>&1) \
	|| {
	>&2 echo -e "${ME}: creating temporary reboot reason file had issues:\n${tmpfile}";
	rv=1
	exit
    }

    # now write boottag entry(s) to the file
    jq -cn --arg boottag "${boot_id}" '{ "boottag": $boottag }' > "${tmpfile}"

    # ensure that we don't try appending if variable doesn't yet exist
    [ -r "${efivar_file}" ] || append=

    # finally write contnt to efivar
    out=$(efivar ${append:--w} -f "${tmpfile}" -n "${efivar_var}" 2>&1 > /dev/null)
    if [ $? != 0 -o "${out}" ]; then
	>&2 echo -e "${ME}: logging to EFI variable ${efivar_var} had issues:\n${out}";
	rv=1
	exit
    fi
    rm -f "${tmpfile}"
}

function boottag_rm_bootid() {
    local boot_id="${1:-$(</proc/sys/kernel/random/boot_id)}"
    local other_ids

    # can we delete the variable or do we need to keep IDs?
    other_ids=$(boottag_get_boot_ids 1)
    if [ "${other_ids}" ]; then
	# need to keep
	local multiple=
	for id in ${other_ids}; do
	    boottag_write_boot_id "${id}" "${multiple:+append}"
	    multiple=1
	done
    else
	# no other IDs contained so we can remove the variable
	rm_efivars "${EFIVAR_BOOTTAG_FILE}"
    fi
}

function reboot_reason_log_for_efi() {
    local json="$1"
    local final_reason="$2"
    local tmpfile

    # unfortunately we need a temporary file containing log data, because
    # command efivar is not capable to receive these via stdin
    tmpfile=$(mktemp -p /tmp reboot-reason.XXXXXXXX 2>&1) \
	|| {
	>&2 echo -e "${ME}: creating temporary reboot reason file had issues:\n${tmpfile}";
	rv=1
	exit
    }
    out=$(echo -n "$json" 2>&1 > "${tmpfile}") \
	|| {
	>&2 echo -e "${ME}: writing JSON data to temporary reboot reason file (${tmpfile}) had issues:\n${out}";
	rv=1
	exit
    }

    # EFI variable handling with efivar is rather strange compared to kernel
    # driver handling:
    #  - sysfs:   <name>-<uuid>
    #  - efivar:  <uuid>-<name>
    # for uuid use 53d7f47e-126f-bd7c-d98a-5aa0643aa921 as explained in
    local efivar_file="${EFIVAR_PMSG_FILE}"
    local efivar_var="${EFIVAR_PMSG_VAR}"

    # variable can't be directly created by efivar for whatever reason, so
    # ensure it exists before appending data to it
    # NOTE:
    #   variable file can't be checked for being writable because, even though
    #   the file has permission 0644 and we operate as root, direct write is
    #   forbidden by means of immutable file attribute
    [ -r "${efivar_file}" ] || touch "${efivar_file}"

    # time to actually write our data to the variable
    set +e
    out=$(efivar -a -f "${tmpfile}" -n "${efivar_var}" 2>&1 > /dev/null)
    if [ $? != 0 -o "${out}" ]; then
	>&2 echo -e "${ME}: logging to ${efivar_file} had issues:\n${out}";
	rv=1
	exit
    fi
    set -e

    # if the reason is considered the final one prior to reboot/shutdown
    # we remove the boottag variable. this means that if the variable contains
    # another boot ID in boottag file (or even multiple) there was a power loss
    # (or multiple of them, one per non-current ID)
    # NOTE:
    #  it is assumed that processing of reboot reason already took place when
    #  getting here!
    if [ "${final_reason}" ]; then
	boottag_rm_bootid
    fi
}

# NOTE:
#   crash logs are too long to get stored into single EFI variables so they
#   are distributed over several variables differing only in a timestamp like
#   last name component.
#
#   apparently, the format of this timestamp is ...
#       xxxxxxxxxxyyzzz
#   ... where x represents the timestamp (system time) of the EFI variable
#   logging, y indicates the part of the full log, and zzz seems to be some
#   kind sub-part sequencing which is obviously currently unused.
#
#   additionally, the snippets contain - as kind of in-band ordering
#   information - one header line of the format ...
#      "Panic#X PartY"
#   ... with "X" and Y being decimal numbers.
#
#   due to this naming scheme, standard alphabetical file order corresponds
#   with the dump snippets order.
#   but be aware that snippets are in reverse chronological order: the first
#   part contains the end of the crash log!
#   this ensures that the most important part of the crash information -
#   crash cause, stacktrace and register duump - is contained in the log
#   regardless of the chosen log buffer size.
#
# NOTE:
#   on the welotec device the "X" in the header line is apparently not handled
#   correctly, instead for multiple crash logs there's always "1" used.
#
#   however, from the timestamps of the pstore files it can still be deduced
#   which files belong together: if they are within a few seconds they most
#   likely stem from the same crash
function gather_efi_crashlog() {
    local dstfile="$1"
    local cwd="${PWD}"
    local log_part log log_full
    local head_part head head_type
    local curpartno
    local tstamp time_epoch partno ipartno seq

    # touch destination file right here to ensure we will be later able to
    # write log to it
    touch "${dstfile}"

    cd "${EFIVAR_DMESG_DIR}"
    for f in dmesg-efi-*; do
	[ -r "$f" ] || break

	# calculate parts of timestamp like last name component
	tstamp="${f/dmesg-efi-/}"
	time_epoch="${tstamp:0:-5}"
	seq="${tstamp/${time_epoch}/}"
	partno="${seq:0:2}"
	ipartno="${partno#${partno%%[^0]*}}"
	seq="${seq:2}"

	# read that log part right now, we'll need it anyway
	head_part=$(head -n 1 ${f})
	# ignore crash unrelated entries for now
	head_type="${head_part%%#*}"
	[ "${head_type}" = "Panic" -o "${head_type}" = "Panic" ] \
	    || continue

	log_part=$(tail -n +2 ${f})

	# have we already started gathering parts of a log?
	if [ "${curpartno}" ]; then
	    # yes, we are, bit does that part belong to the same crash log?
	    if [ $((curpartno + 1)) = $((ipartno)) ]; then
		# yes, this is the next (actual chronologically previous) part
		# of the log
		curpartno="$((ipartno))"
		[ "${head}" ] && head="
${head}"
		head="${head_part}${head}"
		[ "${log}" ] && log="
${log}"
		log="${log_part}${log}"
		continue;
	    fi
	    # this is part of a new log, so append previous log to the full
	    # log possibly consisting of multiple logs which weren't gathered
	    # yet for whatever reason
	    [ "${log_full}" ] && log_full="${log_full}
"
	    log_full="${log_full}[log from $(date --date @${time_epoch})]
${head}
${log}"
	fi

	# we need to start gathering a new crash log here
	curpartno="$((ipartno))"
	[ ${curpartno} = 1 ] \
	    || err 1 "crash log part (${f}) doesn't start with part no 1 but with ${curpartno}"
	log="${log_part}"
	head="${head_part}"
    done

    # here we need to gather last log which exist if log_part is no empty
    # string
    if [ "${log_part}" ]; then
	# add log to full log
	[ "${log_full}" ] && log_full="${log_full}
"
	log_full="${log_full}[log from $(date --date @${time_epoch})]
${head}
${log}"
    fi

    # now we successfully gathered logs remove dmesg files now so that we don't
    # process them again in another boot
    rm -f dmesg-efi-*

    # change back to original working directory to create destination file if
    # there was a panic and return the time_epoch of the crash
    cd "${cwd}"
    if [ "${log_full}" ]; then
	echo "${log_full}" > "${dstfile}"

	# finaly print time epoch as explicit return value
	echo -n "${time_epoch}"
    fi
}

# gather reboot reason files needed for analysis from various places:
#  - /sys/firmware/efi/efivars:
#    - reboot-reason-53d7f47e-126f-bd7c-d98a-5aa0643aa921
#      counterpart to ramoops pmsg file
#    - boottag-53d7f47e-126f-bd7c-d98a-5aa0643aa921
#      alternative to ramoops console file which is always there after a
#      reboot (given that memory region is safe to keep content), but won't
#      exist after a power loss:
#       - it exists and holds the boot_id of the current run while up
#       - it gets removed before actually rebooting or shutting down the
#         system
#       - existence during analysis with different boot_id than current means
#         there was either a power-loss or a sudden reboot
#         (these cases arent't distinguishable, unfortunately
# - /sys/fs/pstore:
#   when system crashes the EFI backend stores the crash log into EFI variables
#   of the form dump-typeX1-X2-X3-X4-X5-<uuid> which are automatically
#   transformed to files in pstore of the form dmesg-efi-NNNNNNNNNNNN
#   (have a look at description above function gather_efi_crashlog() for
#   detailed information about use of placeholder NNNNNNNNNNNN above)
function reboot_reason_get_for_efi() {
    local dmesg_dir="${EFIVAR_DMESG_DIR}"
    local reason_dir=$(get_new_reason_dir "${REASON_DIR}" "${timestamp[0]}")
    local out time_epoch tmpfile pmsg_file dmesg_file console_file
    local bootids reboot_reasons extra_info

    # check for dmesg at first; create a temporary file for that
    tmpfile=$(mktemp -p /tmp dmesg-efi.XXXXXXXX 2>&1) \
	|| {
	>&2 echo -e "${ME}: creating temporary dmesg file had issues:\n${tmpfile}";
	rv=1
	exit
    }
    time_epoch=$(gather_efi_crashlog "${tmpfile}")
    if [ "${time_epoch}" ]; then
	dmesg_file="${reason_dir}/dmesg-efi"
	out=$(cp "${tmpfile}" "${dmesg_file}" 2>&1) \
	    || {
	    >&2 echo -e "${ME}: creating dmesg file (${dmesg_file}) had issues:\n${out}";
	    rv=1
	    exit
	}
    fi

    # check for boottag(s) in order to detect a power loss
    bootids="$(boottag_get_boot_ids 1)"
    if [ "${bootids}" ]; then
	# there is at least one boot_id not matching current one meaning that
	# there was no ordinary reboot/halt during which it would have been
	# deleted; conclusion is a power loss
	# FFS: should we pass on leftover IDs like this
	#   extra_info="${bootids// /, }"

	# replace EFI variable content with only current boot ID now
	boottag_write_boot_id
    else
	# at most current boot_id is contained in variable, so we assume a
	# normal reboot or shutdown & power-on
	console_file="${reason_dir}/console-efi"
	out=$(touch "${console_file}" 2>&1) \
	    || {
	    >&2 echo -e "${ME}: creating console file (${console_file}) had issues:\n${out}";
	    rv=1
	    exit
	}
    fi

    # retrieve logged reboot reasons
    reboot_reasons=$(read_efivar "${EFIVAR_PMSG_FILE}")
    if [ "${reboot_reasons}" ]; then
	pmsg_file="${reason_dir}/pmsg-efi"
	out=$(echo "${reboot_reasons}" > "${pmsg_file}" 2>&1) \
	    || {
	    >&2 echo -e "${ME}: creating pmsg file (${pmsg_file}) had issues:\n${dmesg_file}";
	    rv=1
	    exit
	}
	rm_efivars "${EFIVAR_PMSG_FILE}"
    fi

    analyze "${reason_dir}" "${dmesg_file}" "${console_file}" "${pmsg_file}"
}

function reboot_reason_boottag_for_efi() {
    local cmd="$1"

    if [ "$cmd" = "get" ]; then
	boottag_get_boot_ids "${2:+1}"
    elif [ "$cmd" = "set" ]; then
	boottag_write_boot_id "$2" "${3:+append}"
    fi
}

##################### define entry points    #####################

# distinguish between ramoops and EFI kind of reboot reason handling
if [ -c "${RAMOOPS_DEV_PMSG}" ]; then
    REBOOT_REASON_LOG=reboot_reason_log_for_ramoops
    REBOOT_REASON_GET=reboot_reason_get_for_ramoops
    REBOOT_REASON_BOOTTAG=
else
    REBOOT_REASON_LOG=reboot_reason_log_for_efi
    REBOOT_REASON_GET=reboot_reason_get_for_efi
    REBOOT_REASON_BOOTTAG=reboot_reason_boottag_for_efi
fi

function reboot_reason_log() {
    local reason="$1"
    local extra_info="$2"
    local final_reason="${3:+final}"
    local json

    json=$(get_reboot_reason_data "${reason}" "${extra_info}")
    ${REBOOT_REASON_LOG} "${json}" "${final_reason}"

    >&2 echo -e "${ME}: successfully logged reboot reason \"${reason}\"${extra_info:+ [extra info:${extra_info}]}"
}

function reboot_reason_get() {
    ${REBOOT_REASON_GET}
}

function reboot_reason_boottag() {
    local cmd="$1"

    [ "${REBOOT_REASON_BOOTTAG}" ] \
	|| return

    case "$cmd" in
	get | set)
	    ${REBOOT_REASON_BOOTTAG} "$@"
	    ;;
	*)
	    err 1 "unrecognized boottag command \"${cmd}\""
	    ;;
    esac
}

#####################           main          #####################

trap cleanup EXIT

rv=0
cmd="$1"
[ "${cmd}" ] || err 1 "no command given; use log, get, boottag or is_enabled"

# this variable gets set during image build process accordingly
is_enabled="@@OMNECT_REBOOT_REASON_ENABLED@@"

if [ "${is_enabled}" = "true" ]; then
   # we will need a timestamp for current operation so get it right now stored
   # into timestamp array variable for later use
   get_timestamp

    case "$cmd" in
	log)
            # usage: omnect_reboot_reason.sh log <reason> <extra-info> ...
	    reboot_reason_log "${@:2}"
	    ;;
	get)
	    reboot_reason_get
	    ;;
	boottag_set)
	    reboot_reason_boottag set "${@:2}"
	    ;;
	boottag_get)
	    reboot_reason_boottag get "${@:2}"
	    ;;
	is_enabled)
	    [ "${is_enabled}" = "true" ]
	    ;;
	*)
	    err 1 "unrecognized command \"$cmd\"" \
		;;
    esac
else
    # we simply succeed for all commands but "is_enabled" in case reboot reason
    # functionality not given on a device
    if [ "${cmd}" = "is_enabled" ]; then
	[ "${is_enabled}" = "true" ]
    fi
fi
