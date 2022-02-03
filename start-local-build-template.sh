#!/bin/sh

# Hint: Both of the supported ICS_DeviceManagement targets
# [yocto](https://github.com/ICS-DeviceManagement/ics-dm-os) and
# [simulator](https://github.com/ICS-DeviceManagement/simulator)
# integrate the ics-dm-iot-module-rs.

# If you like to built it independently the paths from the libraries listed below are required.
# Copy `.start-local-build-template.sh` to `.start-local-build.sh` and adapt it.
# Example:
# export LIB_PATH_AZURESDK=/home/user/projects/GitHub/simulator/build/.conan/data/azure-iot-sdk-c/LTS_07_2021_Ref01/_/_/package/3bf7811c9395d29095bf663023235996901b6af2
# export LIB_PATH_EISUTILS=/home/user/projects/GitHub/simulator/build/.conan/data/libeis_utils/0.7.0/_/_/package/*

export LIB_PATH_AZURESDK=<path to the azure iot sdk c >
export LIB_PATH_EISUTILS=<path to eis utils >
export LIB_PATH_UUID=<path to uid >
export LIB_PATH_OPENSSL=<path to openssl >
export LIB_PATH_CURL=<path to curl>

cargo build