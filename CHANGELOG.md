# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.10.3] Q1 2023
 - systemd::start_unit: added timeout handling
 - systemd::is_system_running: added timeout handling
 - systemd::reboot: sync journal before reboot

## [0.10.2] Q1 2023
 - fixed GHSA-4q83-7cq4-p6wg (explicit `cargo update`)

## [0.10.1] Q1 2023
 - update validation: added check if system is running

## [0.10.0] Q1 2023
 - refactored service permissions, reboot, factory-reset, ssh-report and
   update validation handling
   - removed path activation handling for permission propagation
   - added policy-kit configuration for accessing systemd via dbus for reboot
     and starting iot-hub-device-update
   - added sudo configuration for safe fw_setenv/printenv usage
   - added sudo configuration for writing ssh pubkey as user omnect
   - successful start of `iot-hub-device-update` is not part of update
     validation
 - allow service to use optional env file
   `/etc/omnect/omnect-device-service.env`, e.g. to set `RUST_LOG=DEBUG`

## [0.9.1] Q1 2023
 - improved readme
 - pined time dependency to 0.3.19 since newer versions need rust 1.63

## [0.9.0] Q1 2023
 - added ssh direct methods: open_ssh, close_ssh, refresh_ssh_status
 - updated azure-iot-sdk to 0.9.2

## [0.8.2] Q1 2023
 - fixed redundant locking on twin

## [0.8.1] Q1 2023
 - twin:
   - refactored into main module and submodules:
     - main module: src/twin/mod.rs
     - submodules of twin: common.rs, consent.rs, factory_reset.rs, network_status.rs
     - contains now handlers for: C2D messages, direct methods, and desired/reported properties
   - added unittests
   - don't report any network adapter in case no filter is set in desired properties
 - readme: prepared open sourcing repository

## [0.8.0] Q1 2023
 - introduced fallback update handling

## [0.7.3] Q1 2023
 - fixed bug when application didn't exit on Unauthenticated message

## [0.7.2] Q1 2023
 - report network status:
   - switched to include filter
   - fixed issue with empty filter
   - report vectors of addresses

## [0.7.1] Q1 2023
 - added audit exception for RUSTSEC-2020-0071

## [0.7.0] Q1 2023
 - bumped to notify 5.1
 - bumped to azure-iot-sdk 0.9.0
 - switched to anyhow based errors
 - report network status
 - general user consent:
   - only handle if list changed
   - list items are case insensitive

## [0.6.4] Q1 2023
 - introduce rust-toolchain.toml to enforce same rust version as used by kirkstone

## [0.6.3] Q1 2023
 - updated tokio to 1.23 in order to fix cargo audit warning

## [0.6.2] Q4 2022
 - renamed crate from icsdm-device-service to omnect-device-service

## [0.6.1] Q4 2022
 - renamed from ICS-DeviceManagement to omnect github orga

## [0.6.0] Q3 2022
 - renamed crate from demo-portal-module to icsdm-device-service

## [0.5.11] Q3 2022
 - added direct method "reboot"
 - fixed bug when async client does not terminate correctly
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
