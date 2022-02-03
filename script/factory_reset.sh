#!/bin/sh

factory_file=$1

if [ ! -e  "${factory_file}" ]; then
    echo "factory_file not found" 1>&2
    exit 1
fi
RESET=$(cat ${factory_file})
echo ${RESET}

fw_setenv factory-reset ${RESET} && /sbin/reboot

exit 0
