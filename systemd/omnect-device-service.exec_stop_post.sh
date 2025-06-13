#!/bin/bash -x
script=${0}
update_validation_file="/run/omnect-device-service/omnect_validate_update"
barrier_json="/run/omnect-device-service/omnect_validate_update_complete_barrier"
max_restart_count=9

# https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#%24EXIT_CODE
echo SERVICE_RESULT=${SERVICE_RESULT}, EXIT_CODE=${EXIT_CODE}, EXIT_STATUS=${EXIT_STATUS}

function reboot() {
  echo "reboot triggered by ${script}: ${1}"
  /usr/sbin/omnect_reboot_reason.sh log swupdate-validation-failed "${1}"
  dbus-send --system --print-reply --dest=org.freedesktop.login1 /org/freedesktop/login1 "org.freedesktop.login1.Manager.Reboot" boolean:true
}

# for now we only check for ods failed (EXIT_STATUS not 0) and ignore SERVICE_RESULT
# and EXIT_CODE for the decision to reboot the system.
# however, it does potentially make sense to reboot on certain combinations
# even if restart_count < max_restart_count or update validation has not timed
# out yet. (we have to gain experience.)
if [ -f ${barrier_json} ]; then
  # we are run during update validation and ods exited with an error
  if [ "${EXIT_STATUS}" != "0" ]; then
    restart_count=$(jq -r .restart_count ${barrier_json})

    if [ ${restart_count} -ge ${max_restart_count} ]; then
      reboot "too many restarts during update validation"
    fi
  fi
elif [ -f ${update_validation_file} ]; then
  # we detected an update validation, but the barrier file was not created by omnect-device-service
  reboot "omnect-device-service failed to init update validation"
#else
  # if we are not in update validation do nothing for now
fi
