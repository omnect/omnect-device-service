#!/bin/sh

factory_file=$1

if [ ! -e  "${factory_file}" ]; then
    echo "factory_file not found" 1>&2
    exit 1
fi
RESET=$(cat ${factory_file})
echo ${RESET}

# This waiting time should enable the application to send
# an answer to the direct method call before the device reboots.
sleep 2s

fw_setenv factory-reset ${RESET} && /sbin/reboot

exit 0
