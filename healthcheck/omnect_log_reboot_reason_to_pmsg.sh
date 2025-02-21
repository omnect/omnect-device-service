#!/bin/bash -e

# usage: omnect_log_reboot_reason <reason> <extra-info> [ <console-log-file> [ <pmsg-log-file> [ <dmesg-log-file> ] ] ]
reason="${1:-???}"
shift
extra="$*"

rv=0
function cleanup() {
    [ "${tmpfile}" ] && rm -f "${tmpfile}"
    exit $rv
}

trap cleanup EXIT

DEV_PMSG=/dev/pmsg0
DIR_EFIVARS=/sys/firmware/efi/efivars

VALID_REASONS="|reboot|shutdown|swupdate|swupdate-validation-failed|systemd-networkd-wait-online|factory-reset|system-crash|power-loss|"

if ! echo "$VALID_REASONS" | grep -q "|$reason|"; then
    echo "WARNING: unrecognized reboot reason \"$reason\""
fi

# get consistent timestamp
remIFS="${IFS}"
IFS=,
time=( $(date +%F\ %T,%s) )
IFS="${remIFS}"
os_version=$(. /etc/os-release; echo "${VERSION}" )

data="
        {
            \"datetime\":   \"${time[0]}\",
	    \"timeepoch\":  \"${time[1]}\",
            \"uptime\":     \"$(set -- $(</proc/uptime); echo $1)\",
            \"boot_id\":    \"$(</proc/sys/kernel/random/boot_id)\",
            \"os_version\": \"${os_version}\",
            \"reason\":     \"${reason}\",
            \"extra_info\": \"${extra}\"
        }
"

# create data file for uniformness
tmpfile=$(mktemp -p /tmp reboot-reason.XXXXXXXX 2>&1) \
 || {
    >&2 echo -e "$0: creating temporary reboot reason file had issues:\n${tmpfile}";
    exit 1
}
out=$(echo "$data" 2>&1 > "${tmpfile}") \
 || {
    >&2 echo -e "$0: writing data to temporary reboot reason file (${tmpfile}) had issues:\n${out}";
    exit 1
}

if [ -c "${DEV_PMSG}" ]; then
    out=$(cat "${tmpfile}" 2>&1 > ${DEV_PMSG}) \
     || {
	>&2 echo -e "$0: logging to ${DEV_PMSG} had issues:\n${out}";
	exit 1
    }
    logdestination="${DEV_PMSG}"
elif [ -d "${DIR_EFIVARS}" ]; then
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

    # EFI variable handling with efivar is rather strange compared to kernel
    # driver handling:
    #  - sysfs:   <name>-<uuid>
    #  - efivar:  <uuid>-<name>
    # for uuid use 53d7f47e-126f-bd7c-d98a-5aa0643aa921 as explained in
    efivar_uuid="53d7f47e-126f-bd7c-d98a-5aa0643aa921"
    efivar_var="${efivar_uuid}-reboot-reason"
    efivar_sysfs_path="/sys/firmware/efi/efivars/reboot-reason-${efivar_uuid}"
    logdestination="${efivar_sysfs_path}"

    # variable can't be directly created by efivar for whatever reason, so
    # ensure it exists before appending data to it
    [ -w "${efivar_sysfs_path}" ] || touch "${efivar_sysfs_path}"

    # time to actually write our data to the variable
    out=$(efivar -a -f "${tmpfile}" -n "${efivar_var}" 2>&1 > /dev/null)
    if [ "${out}" ]; then
	>&2 echo -e "$0: logging to ${efivar_sysfs_path} had issues:\n${out}";
	exit 1
    fi
else
    >&2 echo -e "$0: no logging possible: neither ${DEV_PMSG} nor ${DIR_EFIVARS} exists";
    false
fi

[ "$out" ] || >&2 echo -e "$0: successfully logged reboot reason to ${logdestination}"
