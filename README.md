# omnect-device-service
**Product page: https://www.omnect.io/home**

This module serves as interface between omnect cloud and device to support certain end to end workflows:

- [omnect-device-service](#omnect-device-service)
  - [Instruction](#instruction)
  - [Factory reset](#factory-reset)
    - [Feature availability](#feature-availability)
    - [Trigger factory reset](#trigger-factory-reset)
    - [Report factory reset status](#report-factory-reset-status)
  - [iot-hub-device-update user consent](#iot-hub-device-update-user-consent)
    - [Feature availability](#feature-availability-1)
    - [Configure current desired general consent](#configure-current-desired-general-consent)
    - [Grant user consent](#grant-user-consent)
    - [Current reported user consent status](#current-reported-user-consent-status)
  - [Reboot](#reboot)
    - [Feature availability](#feature-availability-2)
    - [Trigger reboot](#trigger-reboot)
  - [Network status](#network-status)
    - [Feature availability](#feature-availability-3)
    - [Current reported network status](#current-reported-network-status)
    - [Configure current desired include network filter](#configure-current-desired-include-network-filter)
    - [Refresh Network status](#refresh-network-status)
  - [SSH handling](#ssh-handling)
    - [Feature availability](#feature-availability-4)
    - [SSH status](#ssh-status)
    - [Enabling SSH](#enabling-ssh)
    - [Disabling SSH](#disabling-ssh)
    - [Current reported ssh status](#current-reported-ssh-status)
  - [SSH Tunnel](#ssh-tunnel)
    - [Feature availability](#feature-availability-5)
    - [SSH tunnel status](#ssh-tunnel-status)
    - [Access to Device SSH Public Key](#access-to-device-ssh-public-key)
    - [Opening the SSH Tunnel](#open-ssh-tunnel)
    - [Closing the SSH Tunnel](#close-ssh-tunnel)
  - [Wifi commissioning service](#wifi-commissioning-service)
    - [Feature availability](#feature-availability-6)
  - [Update validation](#update-validation)
    - [Criteria for a successful update](#criteria-for-a-successful-update)
- [License](#license)
- [Contribution](#contribution)

<small><i><a href='http://ecotrust-canada.github.io/markdown-toc/'>Table of contents generated with markdown-toc</a></i></small>

## Instruction
This module is based on omnect [iot-client-template-rs](https://github.com/omnect/iot-client-template-rs). All information you need to build the project can be found there.

## Factory reset
The module itself does not perform a factory reset.
It serves as an interface between the cloud and the built-in factory reset from the [omnect yocto image](https://github.com/omnect/meta-omnect).

### Feature availability

The availability of the feature is reported by the following module twin property:
```
"factory_reset":
{
  "version": <ver>
}
```

The availability of the feature might be suppressed by creating the following environment variable:
```
SUPPRESS_FACTORY_RESET=true
```

### Trigger factory reset

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

### Report factory reset status

Performing a factory reset also triggers a device restart. The restart time of a device depends on the selected factory reset. After the device has been restarted, this module sends a confirmation to the cloud as reported property in the module twin.

```
"factory_reset":
{
  "status":
  {
      "date": "<UTC time of the factory reset status>",
      "status": "<status>"
  }
}
```

The following status information is defined:
 - "in_progress"
 - "succeeded"
 - "failed"
 - "unexpected factory reset type"


## iot-hub-device-update user consent

omnect os uses [iot-hub-device-update](https://github.com/Azure/iot-hub-device-update) service for device firmware update. The service is extended by a "user consent" [content handler](https://github.com/Azure/iot-hub-device-update/blob/main/docs/agent-reference/how-to-implement-custom-update-handler.md), which allows the user to individually approve a new device update for his IoT device.

The module itself does not perform a user consent. It serves as an interface between the cloud and the content handler in [omnect yocto image](https://github.com/omnect/meta-omnect).

Adapt the following environment variable in order to configure the directory used for consent files at runtime:
```
# use the following directory for consent files (defaults to "/etc/omnect/consent"), e.g.:
CONSENT_DIR_PATH: "/my/path"
```

### Feature availability

The availability of the feature is reported by the following module twin property:
```
"device_update_consent":
{
  "version": <ver>
}
```

The availability of the feature might be suppressed by creating the following environment variable:
```
SUPPRESS_DEVICE_UPDATE_USER_CONSENT=true
```

### Configure current desired general consent

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

The current general consent status is also exposed to the cloud as reported property. In case no desired general_consent is defined the current general_consent settings of the device are reported.

### Grant user consent

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

### Current reported user consent status

The module reports the status for a required user consent. For this purpose the module sends a reported property to the cloud.

```
"device_update_consent":
{
  "user_consent_request": [
      {
          "swupdate": "<version>"
      }
  ]
}
```

As soon as the consent for a new update has been granted via the direct method "user_consent", this status is reported via the user_consent_history reported property in the module twin.

```
"device_update_consent":
{
  "user_consent_history":
  {
    "swupdate":
    [
      "<version>"
    ]
  }
}
```

## Reboot

### Feature availability

The availability of the feature is reported by the following module twin property:
```
"reboot":
{
  "version": <ver>
}
```

The availability of the feature might be suppressed by creating the following environment variable:
```
SUPPRESS_REBOOT=true
```

### Trigger reboot

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

## Network status

### Feature availability

The availability of the feature is reported by the following module twin property:
```
"network_status":
{
  "version": <ver>
}
```

The availability of the feature might be suppressed by creating the following environment variable:
```
SUPPRESS_NETWORK_STATUS=true
```

### Current reported network status

The module reports the status of network adapters. For this purpose the module sends this reported property to the cloud.

```
"network_status":
{
  "interfaces": [
    {
      "addr_v4": [
        "172.17.0.1"
      ],
      "addr_v6": [
        "fe80::42:22ff:fe3b:ad66"
      ],
      "mac": "02:42:22:3b:ad:66",
      "name": "docker0"
    },
    {
      "addr_v4": [
        "172.25.0.1"
      ],
      "addr_v6": [
        "fe80::42:c3ff:fe87:3c03"
      ],
      "mac": "02:42:c3:87:3c:03",
      "name": "br-04171e27390a"
    },
    {
      "addr_v4": [
        "192.168.0.84"
      ],
      "addr_v6": [
        "fe80::33d9:9063:897d:4357"
      ],
      "mac": "08:00:27:6d:83:36",
      "name": "enp0s8"
    },
    {
      "addr_v4": [
        "127.0.0.1"
      ],
      "addr_v6": [
        "::1"
      ],
      "mac": "00:00:00:00:00:00",
      "name": "lo"
    },
    {
      "addr_v6": [
        "fe80::4c8e:77ff:fec1:10d3"
      ],
      "mac": "4e:8e:77:c1:10:d3",
      "name": "vethbd467ae"
    }
  ]
}
```

### Configure current desired include network filter

In order to report and filter network adapters by name the following desired property can be used. The filter is case insensitive and might contain a leading and/or trailing wildcard '*', e.g.:
```
"include_network_filter":
[
  "docker*",
  "*eth*",
  "wlan0"
]
```
If the filter is empty all network adapters are reported. In case the `include_network_filter` property doesn't exis at all no adapters will we reported.

### Refresh Network status

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
## SSH handling

### Feature availability

The availability of the feature is reported by the following module twin property:
```
"ssh":
{
  "version": <ver>
}
```

The availability of the feature might be suppressed by creating the following environment variable:
```
SUPPRESS_SSH_HANDLING=true
```

### SSH status

A direct method to refresh and report current ssh status.

**Direct method: refresh_ssh_status**

Method Name: `refresh_ssh_status`

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
### Enabling SSH

SSH gets enabled by adding the iptables nft filter rule for port 22 and adding the provided public key to `/etc/dropbear/authorized_keys`.

**Note**: This is intended for "release" images. In "devel" images SSH is enabled by default.

**Direct method: open_ssh**

Method Name: `open_ssh`

Payload:
```
{
  "pubkey" : "<content of your ssh pubkey file>"
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

### Disabling SSH

SSH gets disabled by removing the iptables nft filter rule for port 22 and deleting the content of `/etc/dropbear/authorized_keys`.

**Note**: If you use custom iptables rules, which don't have the default policy "DROP" for the "filter" table "INPUT" chain and use a "devel" image or have a custom `/etc/default/dropbear` which allows password logins this direct method has no effect.

**Direct method: open_ssh**

Method Name: `close_ssh`

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


### Current reported ssh status

The module reports the current ssh status. For this purpose the module reports the following properties to the cloud.

```
"ssh":
{
  "status":
  {
    "v4_enabled":false,
    "v6_enabled":false
  }
}
```

## SSH Tunnel handling

### Feature availability

The availability of the feature is reported by the following module twin property:
```
"ssh_tunnel":
{
  "version": <ver>
}
```

The availability of the feature might be suppressed by creating the following environment variable:
```
SUPPRESS_SSH_TUNNEL=true
```

### Access to Device SSH Public Key

This creates a single-use ssh key pair and retrieves the public key of the key pair. A signed certificate for this public key is then expected as an argument with a subsequent `open_ssh_tunnel` call.

**Direct method: get_ssh_pub_key**

Method Name: `get_ssh_pub_key`

Payload:
```
{
  "tunnel_id": "<uuid identifying the tunnel>",
}
```

Result:
```
{
  "status": <HTTP-Statuscode>,
  "payload": {}
}
```
In case the method was successful received by the module the return value of the method looks like this:

```
{
  "status": 200,
  "payload": {
    "key": "<PEM formatted ssh public key>"
  }
}
```

In all other cases there will be an error status:
```
{
  "status": 401,
  "payload": {}
}
```


### Opening the SSH tunnel

This creates a ssh tunnel to the bastion host, which can then be used to open an SSH connection to the device. This method therefore starts a SSH reverse tunnel connection to the bastion host and binds it there to a uniquely named socket. The connection to the device can then be established across this socket.

**Note:** The tunnel is maintained open only for 5 minutes, if no connection has been established after this time, it will automatically close.

**Direct method: open_ssh_tunnel**

Method Name: `open_ssh_tunnel`

Payload:
```
{
  "tunnel_id": "<uuid identifying the tunnel>",
  "certificate": "<PEM formatted ssh certificate which the device uses to create the tunnel>"
  "host": "<hostname of the bastion host>"
  "port": "<ssh port on the bastion host>"
  "user": "<ssh user on the bastion host>"
  "socket_path": "<socket path on the bastion host>"
}
```

Result:
```
{
  "status": <HTTP-Statuscode>,
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


### Closing the SSH tunnel

This closes an existing ssh tunnel. Typically, the ssh tunnel is terminated automatically, once it is not used any longer. This method provides a fallback to cancel an existing connection. This is facilitated by sending control commands to the SSH tunnel master socket.

**Direct method: close_ssh_tunnel**

Method Name: `open_ssh_tunnel`

Payload:
```
{
  "tunnel_id": "<uuid identifying the tunnel>"
}
```

Result:
```
{
  "status": <HTTP-Statuscode>,
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

## Wifi commissioning service

### Feature availability

The availability of the feature is reported by the following module twin property:
```
"wifi_commissioning":
{
  "version": <ver>
}
```

## Update validation

On `iot-hub-device-update` update, after flashing the new root partition, we boot into the new root partition and test if the update was successful.<br>
We don't set the new root partition permanently yet. On this boot the startup of `iot-hub-device-update` is prevented and has to be triggered by `omnect-device-service`.<br>
`omnect-device-service` validates if the update was successful. If so, the new root partition is permanently set and the start of `iot-hub-device-update` gets triggered. If not, the device gets rebooted and we boot to the old root partition.

### Criteria for a successful update
This service provisions against iothub.

# License

Licensed under either of
* Apache License, Version 2.0, (./LICENSE-APACHE or <http://www.apache.org/licenses/LICENSE-2.0>)
* MIT license (./LICENSE-MIT or <http://opensource.org/licenses/MIT>)

at your option.

# Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

---

copyright (c) 2022 conplement AG<br>
Content published under the Apache License Version 2.0 or MIT license, are marked as such. They may be used in accordance with the stated license conditions.
