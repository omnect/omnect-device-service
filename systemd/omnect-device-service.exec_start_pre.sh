#!/bin/bash -x
socket_file="/run/omnect-device-service/api.sock"

# in some cases the socket file isn't deleted reliable by systemd, e.g. when service crashes or returns with an error.
# thus we enforce removal
rm -f ${socket_file}
