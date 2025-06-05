#!/bin/bash -x
script=${0}
update_validation_file="/run/omnect-device-service/omnect_validate_update"

if [ -f ${update_validation_file} ]; then
  echo "reboot triggered by ${script}"
  /usr/sbin/omnect_reboot_reason.sh log swupdate-validation-failed "overall timeout"
  dbus-send --system --print-reply --dest=org.freedesktop.login1 /org/freedesktop/login1 "org.freedesktop.login1.Manager.Reboot" boolean:true
fi
