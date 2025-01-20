#!/bin/sh

# usage: omnect_log_reboot_reason <reason> <extra-info> [ <console-log-file> [ <pmsg-log-file> [ <dmesg-log-file> ] ] ]
reason="${1:-???}"
shift
extra="$*"

DEV_PMSG=/dev/pmsg0
VALID_REASONS="|reboot|shutdown|swupdate|swupdate-validation-failed|systemd-networkd-wait-online|system-crash|power-loss|"

if ! echo "$VALID_REASONS" | grep -q "|$reason|"; then
    echo "WARNING: unrecognized reboot reason \"$reason\""
fi

IFS=, time=( $(date +%F\ %T,%s) )
data="
        {
            \"datetime\":   \"${time[0]}\",
	    \"timeepoch\":  \"${time[1]}\",
            \"uptime\":     \"$(set -- $(</proc/uptime); echo $1)\",
            \"boot_id\":    \"$(</proc/sys/kernel/random/boot_id)\",
            \"reason\":     \"${reason}\",
            \"extra-info\": \"${extra}\"
        }
"
out=$(echo "$data" 2>&1 > ${DEV_PMSG})

if [ "$out" ]; then
    >&2 echo -e "$0: logging to ${DEV_PMSG} had issues:\n${out}";
else
    >&2 echo -e "$0: successfully logged to ${DEV_PMSG}";
fi
