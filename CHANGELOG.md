# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.14.17] Q1 2024
- added infos at startup: current root device and if a bootloader update occurred

## [0.14.16] Q1 2024
- updated azure-iot-sdk to 0.11.10 to introduce configurable outgoing message confirmation timeout

## [0.14.15] Q1 2024
- fixed sending watchdog notify in time

## [0.14.14] Q1 2024
- updated dependencies in order to fix audit warnings

## [0.14.13] Q1 2024
- updated azure-iot-sdk to 0.11.8 to introduce configurable do_work frequency and logging in azure-iot-sdk-c
- prolonged watchdog interval while running update validation

## [0.14.12] Q4 2023
- removed multiline error log messages to get a more compact view in the journal

## [0.14.11] Q4 2023
- updated azure-iot-sdk to 0.11.6

## [0.14.10] Q4 2023
- bumped env_logger to 0.10
- logging: added sd-daemon logging priority prefixes to get different log levels in the journal

## [0.14.9] Q4 2023
- update rust toolchain to 1.74

## [0.14.8] Q4 2023
- fixed cargo.toml version

## [0.14.7] Q4 2023
- tests: fixed clippy warning

## [0.14.6] Q4 2023
- removed feature ssh_handling, ssh connection via ssh_tunnel still working
- fixed an issue with ssh tunnels were certificates were deleted early
- fixed ssh tunnel to enforce certificate based authentication

## [0.14.5] Q4 2023
- fixed passing of errors to direct methods results

## [0.14.4] Q4 2023
- ssh tunnel:
  - fixed error in tests
  - fixed error when sending device 2 cloud notifications by bumping azure-iot-sdk to 0.11.4
- tests: fixed clippy warnings

## [0.14.3] Q4 2023
- reduced permissions for ssh keys and certificates

## [0.14.2] Q3 2023
- fixed time stamp parsing causing flaky tests
- removed bogus debug prints in tests

## [0.14.1] Q3 2023
- fixed RUSTSEC-2020-0071 (explicit `cargo update`)

## [0.14.0] Q3 2023
- ssh tunnel: added ssh tunnel feature to access possibly NATed devices.
- fixed code format
- Changelog: fixed indentation

## [0.13.2] Q3 2023
- bumped to azure-iot-sdk 0.11.0 which prevents potential deadlocks
- introduced signal handler to handle termination and shutdown twin properly

## [0.13.1] Q3 2023
- fixed rust-toolchain.toml

## [0.13.0] Q3 2023
- replaced channel/thread based event dispatching by async dispatching based on azure-iot-sdk 0.10.0
- introduced mockall crate for unit tests
- introduced cargo feature "mock"
  - usefull for host testing
  - automatically used by cargo test
- updated to rust toolchain to v1.65

## [0.12.0] Q2 2023
- added abstraction layer `bootloader_env` for uboot and grub to
  support environment variable handling
- update validation: fixed bug where root partition was switched
  on update validation fail

## [0.11.5] Q2 2023
- systemd: removed enforcing reboot on errors

## [0.11.4] Q2 2023
- fixed RUSTSEC-2023-0044

## [0.11.3] Q2 2023
- updated to azure-iot-sdk 0.9.5
- changed omnect git dependencies from ssh to https url's

## [0.11.2] Q2 2023
- added feature toggle for reboot direct method
- added feature reporting for wifi-commissioning-gatt-service
- improved error messages

## [0.11.1] Q2 2023
- fixed cargo clippy error

## [0.11.0] Q1 2023
- refactored and introduced feature toggles

## [0.10.4] Q1 2023
- systemd::start_unit: added timeout handling
- systemd::wait_for_system_running: added timeout handling
- systemd::reboot: sync journal before reboot
- module version: also log the short git revision
- explicit `cargo update` to fix RUSTSEC-2023-0034

## [0.10.3] Q1 2023
- fixed twin update handling when key is not present at all vs. key is null

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
