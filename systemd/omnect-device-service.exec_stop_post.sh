#!/bin/bash -x
script=${0}
socket_file="/run/omnect-device-service/api.sock"
update_validation_file="/run/omnect-device-service/omnect_validate_update"
barrier_json="/run/omnect-device-service/omnect_validate_update_complete_barrier"
max_restart_count=9

# https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#%24EXIT_CODE
echo SERVICE_RESULT=${SERVICE_RESULT}, EXIT_CODE=${EXIT_CODE}, EXIT_STATUS=${EXIT_STATUS}

function reboot() {
  echo "reboot triggered by ${script}: ${1}"
  dbus-send --system --print-reply --dest=org.freedesktop.login1 /org/freedesktop/login1 "org.freedesktop.login1.Manager.Reboot" boolean:true
}

rm -rf ${socket_file}

# for now we ignore SERVICE_RESULT and EXIT_STATUS. however it does potentially
# make sense to reboot on certain combinations even if restart_count < max_restart_count
# or update validation is not yet timeouted. (we have to gain experience.)
if [ -f ${barrier_json} ]; then
  # we are run during update validation
  now=$(cat /proc/uptime | awk '{print $1}')
  now_ms="${now%%\.*}$(printf %003d $((${now##*\.}*10)))"
  update_validation_start_ms=$(jq -r .start_monotonic_time_ms ${barrier_json})
  restart_count=$(jq -r .restart_count ${barrier_json})
  authenticated=$(jq -r .authenticated ${barrier_json})

  if [ ${restart_count} -ge ${max_restart_count} ]; then
    reboot "too many restarts during update validation"
  fi

  if [ "${authenticated}" =  "true" ]; then
    reboot "omnect-device-service authenticated, but update validation failed"
  fi

  if [ $((${now_ms} - ${update_validation_start_ms})) -ge $((UPDATE_VALIDATION_TIMEOUT_IN_SECS * 1000)) ]; then
    reboot "update validation timeout"
  fi
elif [ -f ${update_validation_file} ]; then
  # we detected an update validation, but the barrier file was not created by omnect-device-service
  reboot "omnect-device-service failed to init update validation"
#else
  # if we are not in update validation do nothing for now
fi
