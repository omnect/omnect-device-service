#!/bin/bash

# https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#%24EXIT_CODE
echo SERVICE_RESULT=${SERVICE_RESULT}, EXIT_CODE=${EXIT_CODE}, EXIT_STATUS=${EXIT_STATUS}

if [[ -f /run/omnect-device-service/omnect_validate_update_complete_barrier ]]; then
   if [[ "${SERVICE_RESULT}" != "exit-code" ]] || [[ "${EXIT_STATUS}" != "0" ]]; then
     dbus-send --system --print-reply --dest=org.freedesktop.login1 /org/freedesktop/login1 "org.freedesktop.login1.Manager.Reboot" boolean:true
   fi

#else
  # if we are not in update validation do nothing for now

fi
