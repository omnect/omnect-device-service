#!/bin/bash -x
script=${0}
update_validation_file="/run/omnect-device-service/omnect_validate_update"
barrier_json="/run/omnect-device-service/omnect_validate_update_complete_barrier"
max_restart_count=9

# https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#%24EXIT_CODE
echo SERVICE_RESULT=${SERVICE_RESULT}, EXIT_CODE=${EXIT_CODE}, EXIT_STATUS=${EXIT_STATUS}

function reboot() {
  echo "reboot triggered by ${script}: ${1}"
  [ "${log_reboot_reason}" ] && \
      /usr/sbin/omnect_reboot_reason.sh log swupdate-validation-failed "${1}"
  dbus-send --system --print-reply --dest=org.freedesktop.login1 /org/freedesktop/login1 "org.freedesktop.login1.Manager.Reboot" boolean:true
}

# reboot reason logging shall only happen if omnect-device-service exited with
# en error
log_reboot_reason=
[ "${EXIT_STATUS}" = 0 ] || log_reboot_reason=1

# for now we ignore SERVICE_RESULT and EXIT_STATUS for the decision to reboot
# the system.
# however, it does potentially make sense to reboot on certain combinations
# even if restart_count < max_restart_count or update validation has not timed
# out yet. (we have to gain experience.)
if [ -f ${barrier_json} ]; then
  # we are run during update validation
  now_boottime_secs=$(cat /proc/uptime | awk '{print $1}')
  start_boottime_secs=$(jq -r .start_boottime_secs ${barrier_json})
  deadline_boottime_secs=$(jq -r .deadline_boottime_secs ${barrier_json})
  restart_count=$(jq -r .restart_count ${barrier_json})
  authenticated=$(jq -r .authenticated ${barrier_json})
  local_update=$(jq -r .local_update ${barrier_json})

  if [ ${restart_count} -ge ${max_restart_count} ]; then
    reboot "too many restarts during update validation"
  fi

  if [ "${local_update}" =  "false" ] && [ "${authenticated}" =  "true" ]; then
    reboot "omnect-device-service authenticated, but update validation failed"
  fi

  if [ ${now_boottime_secs} -ge ${deadline_boottime_secs} ]; then
    reboot "update validation timeout"
  fi
elif [ -f ${update_validation_file} ]; then
  # we detected an update validation, but the barrier file was not created by omnect-device-service
  reboot "omnect-device-service failed to init update validation"
#else
  # if we are not in update validation do nothing for now
fi
