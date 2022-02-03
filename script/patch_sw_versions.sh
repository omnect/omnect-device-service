#!/bin/sh

sw_versions_file="/etc/sw-versions"
downgrade_versions_file=$1

if [ ! -e  "${downgrade_versions_file}" ]; then
    echo "downgrade_versions_file not found" 1>&2
    exit 1
fi
NEW_SW_VERSION=$(cat ${downgrade_versions_file})

if [ ! -e  "${sw_versions_file}" ]; then
    echo "sw-versions file not found" 1>&2
else
    echo "$(awk 'NR==1 {$2 = '\"${NEW_SW_VERSION}\"'}1' ${sw_versions_file})" > ${sw_versions_file}
    exit 0
fi

exit 1
