# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.11] Q3 2022
 - added "reboot" direct method
 - improved logging for AuthenticationStatus changes

## [0.5.10] Q3 2022
 - log message with severity error on panics

## [0.5.9] Q3 2022
 - fixed report azure-sdk-version in twin
 - updated to notify 5.0
 - switched from forked sd-notify to new official release 0.4.1
 - changed some debug messages to log level info

## [0.5.8] Q3 2022
 - report azure-sdk-version in twin
 - log info message for azure-sdk-version
 - bump to azure-iot-sdk 0.8.4

## [0.5.7] Q3 2022
 - start service after time-sync target to avoid time jumps during service start
 - added info message for logging the package version

## [0.5.6] Q3 2022
- fixed panic when closing message channel

## [0.5.5] Q3 2022
- fixed panic when calling IotHubClient::from_identity_service
- fixed terminating on ExpiredSasToken
- bumped to latest azure-iot-sdk 0.8.3

## [0.5.4] Q3 2022
- bumped to latest azure-iot-sdk 0.8.2
- fixed tokio dependency

## [0.5.3] Q2 2022
- replaced std::thread by tokio for async tasks

## [0.5.2] Q2 2022
- bumped to latest azure-iot-sdk 0.8.1

## [0.5.1] Q2 2022
- bumped to latest azure-iot-sdk 0.8.0

## [0.5.0] Q2 2022
- factory reset: optional restore wifi settings
- direct methods: return with error in case of file system failures
- fix bug in main loop when channel is closed by sender
- report cargo package version

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
