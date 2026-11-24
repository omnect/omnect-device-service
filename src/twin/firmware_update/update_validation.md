# Update validation workflow

After flashing an update to the new root partition, the device boots this partition and it is probed if the update was successful. Only after this test has passed successfully, the new root partition is permanently set for booting the device. Otherwise, if the goal state for a valid update wasn't reached, a reboot is triggered which boots to the old root partition.

:exclamation: There is one exception from this behavior. In case the update includes a bootloader update too, the device cannot recover by booting to the old partition due to potential backward compatibility issues. Thus no update validation is triggered at all. In a worst case scenario this might lead to a stuck device or a reboot loop.

## Criteria for a successful update

The following checks must be passed in order to successfully validate an update:

- omnect-device-service.service status is in state [running](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html#status%20PATTERN%E2%80%A6%7CPID%E2%80%A6%5D)
- system is in state [running](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html#is-system-running) and no service is in a crash loop
  (repeatedly restarting without staying active); a `degraded` system
  state fails the validation
- in case local update is **NOT** [configured](#local-validation)
  - adu-agent could be started successfully
  - omnect-device-service is connected to iothub (successfully provisioned)

## Files and their meaning

- /run/omnect-device-service/omnect_validate_update:
  - created after flashing and before rebooting the first time to the updated partition
  - signals that update validation must be run
  - the file is removed as a result of a positive validation
- /run/omnect-device-service/omnect_validate_update_complete_barrier:
  - created by omnect-device-service
  - saves current state of update validation, such as restart count and validation timeout/deadline
  - the file is removed as a result of a positive validation
- /run/omnect-device-service/omnect_validate_update_failed:
  - created by [initramfs](https://github.com/omnect/omnect-os-init/blob/main/docs/usage.md#update-validation) in case the device boots after the update validation failed
  - signals that the device was recovered by booting the old rootfs
- [omnect-device-service.exec_stop_post.sh](../../../systemd/omnect-device-service.exec_stop_post.sh):
  - observes omnect-device-service restarts while update validation is in progress
  - reboots the device if omnect-device-service failed to start more often as the defined threshold

## Configuration options

### Timeouts

#### Internal timeout

- configurable via environment variable `UPDATE_VALIDATION_TIMEOUT_IN_SECS`
- timeout used internally by omnect-device-service
- the timeout is canceled as soon as initialization completed and (if configured) iothub connection is established

#### System health deadline

- the system state is polled until the deadline, which is the remaining
  validation time minus a safety margin; healthy and unhealthy both need several
  observations, so a single poll landing in a restart window decides nothing
- a service in a restart cycle below the crash loop threshold keeps the check
  polling, since it can still reach the threshold
- before the deadline a verdict needs consecutive observations of one kind
- a failed poll is no observation: the wait keeps polling and only gives up when
  the same number of polls fails in a row. If the deadline comes first and no poll
  ever succeeded, validation fails without a system state to report
- at the deadline all unhealthy observations of the wait count, consecutive or
  not: validation fails once they reach the same number, or if no poll was
  healthy at all. The reported cause is the one with the most observations
- otherwise one healthy observation is enough to succeed, whatever the last poll
  saw, because a rollback needs evidence
- without any healthy observation the last one decides: a system that is still
  starting fails, a restart pending below the threshold succeeds
- when the remaining validation time is at or below the safety margin the
  deadline is zero. The first observation then decides by the rules above, and
  since no poll was healthy yet a single unhealthy one fails the validation
- on a failed validation the reboot reason `swupdate-validation-failed` is
  logged with the cause as extra info: the failed units, the crash-looping
  units, the last system state, or why the polls failed

#### Global timeout

- defined in [update-validation-observer.timer](../../../systemd/update-validation-observer.timer)
- reboots the system if /run/omnect-device-service/omnect_validate_update isn't deleted by omnect-device-service in time

### Crash loop detection

- configurable via environment variable `CRASH_LOOP_RESTART_THRESHOLD` (default 3, must be >= 1)
- a unit counts as crash-looping after this many restarts while it is still in a
  restart cycle

### Local validation

`/var/lib/omnect-device-service/update_validation_conf.json` tells if the update validation happens in a local environment, where no connection to the iothub is present (if the file doesn't exist `"local": false` is assumed):

```json
{
  "local": true
}
```

A local update writes the file right before the reboot into the validation boot, and ods deletes it after the validation. omnect-os-init deletes the file on every boot without update validation, so a file written by hand is gone after the next reboot.
