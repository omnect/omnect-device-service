# Update validation workflow

After flashing an update to the new root partition, the device boots this partition and it is probed if the update was successful. Only after this test has passed successfully, the new root partition is permanently set for booting the device. Otherwise, if the goal state for a valid update wasn't reached, a reboot is triggered which boots to the old root partition.

## Criteria for a successful update

The following checks must be passed in order to successfully validate an update:

- omnect-device-service.service status is in state [running](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html#status%20PATTERN%E2%80%A6%7CPID%E2%80%A6%5D)
- system is in state [running](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html#is-system-running)
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
  - created by [initramfs](https://github.com/omnect/meta-omnect/blob/bcaac3baa2948e71a494a958f3db37593031f690/recipes-omnect/initrdscripts/omnect-os-initramfs/omnect-device-service-setup#L80) in case the device boots after the update validation failed
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

#### Global timeout

- defined in [update-validation-observer.timer](../../../systemd/update-validation-observer.timer)
- reboots the system if /run/omnect-device-service/omnect_validate_update isn't deleted by omnect-device-service in time

### Local validation

In `/var/lib/omnect-device-service/update_validation_conf.json` it can be configured, if the update validation happens in a local environment, where no connection to the iothub is present. (if the file doesn't exist `"local": false` is assumed):

```json
{
  "local": true
}
```
