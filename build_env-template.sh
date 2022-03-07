#!/bin/sh

# Hint:
# If you like to built this module the paths from the libraries listed below are required.
# Copy `.build_env-template.sh` to `.build_env.sh` and adapt it.
# Example:
# export LIB_PATH_AZURESDK=/home/user/projects/GitHub/simulator/build/.conan/data/azure-iot-sdk-c/LTS_07_2021_Ref01/_/_/package/3bf7811c9395d29095bf663023235996901b6af2
# export LIB_PATH_UUID=/home/joerg/projects/GitHub/simulator/build/.conan/data/libuuid/1.0.3/_/_/package/*
#
# build the source code via : ./build_env.sh cargo build

export LIB_PATH_AZURESDK=<path to the azure iot sdk c >
export LIB_PATH_UUID=<path to uid >
export LIB_PATH_OPENSSL=<path to openssl >
export LIB_PATH_CURL=<path to curl>

exec ${@}