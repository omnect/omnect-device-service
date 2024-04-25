#!/bin/bash -x
socket_file="/run/omnect-device-service/api.sock"

# in some cases the socket file isn't deleted reliable by systemd, e.g. when service crashes or returns with an error.
# thus we enforce removal
if [ -f ${socket_file} ]; then
  rm -f ${socket_file}
elif [ -d ${socket_file} ]; then
  rm -rf ${socket_file}
fi
