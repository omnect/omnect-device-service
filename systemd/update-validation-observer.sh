#!/bin/bash -x
script=${0}
update_validation_file="/run/omnect-device-service/omnect_validate_update"

if [ -f ${update_validation_file} ]; then
  echo "reboot triggered by ${script}"
  /usr/sbin/omnect_reboot_reason.sh log swupdate-validation-failed "overall timeout"
  systemctl start reboot.target
fi
