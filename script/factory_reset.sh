#!/bin/sh

type_file=$1
restore_list_file=$2

if [ ! -e  "${type_file}" ]; then
    echo "type_file not found" 1>&2
    exit 1
fi

RESET_TYPE=$(cat ${type_file})
echo ${RESET_TYPE}

if [ -e  "${restore_list_file}" ]; then
    RESTORE_LIST=$(cat ${restore_list_file})
    echo ${RESTORE_LIST}
    fw_setenv factory-reset-restore-list ${RESTORE_LIST}
fi

# This timeout enables the application to 
# answer the direct method call before the device reboots.
sleep 2s

fw_setenv factory-reset ${RESET_TYPE} && /sbin/reboot

exit 0
