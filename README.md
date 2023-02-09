# omnect-device-service

## Instruction
This module is based on the omnect [iot-client-template-rs](https://github.com/omnect/iot-client-template-rs). All information you need to build the project can be found there.


## What is omnect-device-service
This module implements the device part for the following end to end workflows:
- factory reset
- iot-hub-device-update user consent
- reboot
- report network status

### Factory reset
The module itself does not perform a factory reset.
It serves as an interface between the cloud and the built-in factory reset from the [omnect yocto image](https://github.com/omnect/meta-omnect).

A function was specified for this purpose, a so-called direct method which is described below.

**Direct method: factory reset**

Method Name: `factory_reset`

Payload:
```
{
  "type": <factory reset type number>,
  "restore_settings":
  [
      "wifi"
  ]
}
```

Result:
{
  "status": <HTTP-Statusode>,
  "payload": {"<result>"}
}


The supported reset `type` and the documentation in general can be found in the [meta-omnect layer](https://github.com/omnect/meta-omnect#factory-reset).

The **optional** `restore_settings` array can be used to define user settings that must be restored after wiping device storage. Currently only `wifi` settings in `wpa_supplicant.conf` can be restored.

In case the method was successful received by the module the return value of the method looks like this:

```
{
  "status": 200,
  "payload": {}
}
```

In all other cases there will be an error status and a meaningful message in the payload:
```
{
  "status": 401,
  "payload": {"error message"}
}
```

Performing a factory reset also triggers a device restart. The restart time of a device depends on the selected factory reset. After the device has been restarted, this module sends a confirmation to the cloud as reported property in the module twin.

```
"factory_reset_status":
{
    "date": "<UTC time of the factory reset status>",
    "status": "<status>"
}
```

The following status information is defined:
 - "in_progress"
 - "succeeded"
 - "failed"
 - "unexpected factory reset type"


### iot-hub-device-update user consent

In our systems we use the service [iot-hub-device-update](https://github.com/Azure/iot-hub-device-update) for device firmware update. We have extended this service to include a "user consent" functionality, which allows the user to individually approve a new device update for his IoT device.

The module itself does not perform a user consent. It serves as an interface between the cloud and the built-in user consent from the [omnect yocto image](https://github.com/omnect/meta-omnect).

Adapt the following environment variable in order to configure the directory used for consent files at compile time:
```
# use the following directory for consent files (defaults to "/etc/omnect/consent"), e.g.:
CONSENT_DIR_PATH: "/my/path"
```

#### Configure current desired general consent

To enable a general consent for all swupdate based firmware updates, configure the following general_consent setting in the module twin (the setting is case insensitive):

```
"general_consent":
[
  "swupdate"
]
```

To disable the general consent enter the following setting in the module twin:

```
"general_consent":
[

]
```

The current general consent status is also exposed to the cloud as reported property.

#### Grant user consent

If there is no general approval for a firmware update, a separate approval must be given for each upcoming update.
A direct method was specified for this purpose which is described below.

**Direct method: user_consent**

Method Name: `user_consent`

Payload:
```
{
  "swupdate": "<version>"
}
```

Result:
```
{
  "status": <HTTP-Statusode>,
  "payload": {"<result>"}
}
```

In case the method was successful received by the module the return value of the method looks like this:

```
{
  "status": 200,
  "payload": {}
}
```

In all other cases there will be an error status and a meaningful message in the payload:
```
{
  "status": 401,
  "payload": {"error message"}
}
```

#### Current reported user consent status

The module reports the status for a required user consent. For this purpose the module sends a reported property to the cloud.

```
"user_consent_request": [
    {
        "swupdate": "<version>"
    }
]
```

As soon as the consent for a new update has been granted via the direct method "user_consent", this status is reported via the user_consent_history reported property in the module twin.

```
"user_consent_history":
{
  "swupdate":
  [
    "<version>"
  ]
}
```

### Reboot

A direct method to trigger a device reboot.

**Direct method: reboot**

Method Name: `reboot`

Payload:
```
{
}
```

Result:
```
{
  "status": <HTTP-Statusode>,
  "payload": {}
}
```
In case the method was successful received by the module the return value of the method looks like this:

```
{
  "status": 200,
  "payload": {}
}
```

In all other cases there will be an error status:
```
{
  "status": 401,
  "payload": {}
}
```

### Network stautus

#### Current reported network status

The module reports the status of network adapters. For this purpose the module sends a reported property to the cloud.

```
"network_interfaces": [
  {
    "name": "<adapter name>",
    "addr": "<ipv4/ipv6 address>",
    "mac":  "<mac address>",
  },
]
```

#### Configure current desired exclude network filter

In order to filter network adapters by name the following desired property can be used to exclude network from reporting. The filter is case insensitive and might contain a leading and/or trailing wildcard '*', e.g.:
```
"exclude_network_filter":
[
  "docker*",
  "veth*",
  "wlan0",
]
```

#### Refresh Network status

A direct method to refresh and report current network status.

**Direct method: refresh_network_status**

Method Name: `refresh_network_status`

Payload:
```
{
}
```

Result:
```
{
  "status": <HTTP-Statusode>,
  "payload": {}
}
```
In case the method was successful received by the module the return value of the method looks like this:

```
{
  "status": 200,
  "payload": {}
}
```

In all other cases there will be an error status:
```
{
  "status": 401,
  "payload": {}
}
```

## License

Licensed under either of
* Apache License, Version 2.0, (./LICENSE-APACHE or <http://www.apache.org/licenses/LICENSE-2.0>)
* MIT license (./LICENSE-MIT or <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

Copyright (c) 2022 conplement AG
