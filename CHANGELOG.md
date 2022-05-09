# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] Q2 2022
- factory reset: optional restore wifi settings

## [0.4.1] Q2 2022
- general:
  - improved error logging and handling
- user consent:
  - fixed handling of empty general consent in desired properties
  - fixed opening file for reading

## [0.4.0] Q2 2022
- factory reset:
  - report new status "in_progress"
- user consent:
  - report general consent status

## [0.3.0] Q2 2022
- added user consent handling for "iot-hub-device-update"
- bumped to latest azure-iot-sdk 0.5.5
- firmware_reset_trigger_file: delete file content with OpenOptions "truncate"
- catch error message if fw_printenv command is not supported on the used platform
- rollback placeholder handling for formatting string, due to compatibility with rust toolchain < 1.56

## [0.2.0] Q1 2022
- bumped to latest azure-iot-sdk 0.5.3

## [0.1.1] Q1 2022
- evaluation of the uboot variable "factory-reset-status" optimized

## [0.1.0] Q1 2022
- initial version
