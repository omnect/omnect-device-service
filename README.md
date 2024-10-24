# omnect-device-service
**Product page: https://www.omnect.io/home**

This module serves as interface between omnect cloud and device to support certain end to end workflows:

- [omnect-device-service](#omnect-device-service)
  - [Configuration](#configuration)
    - [Log level](#log-level)
    - [azure-iot-sdk](#azure-iot-sdk)
  - [Azure twin features](#azure-twin-features)
    - [System Info](#system-info)
      - [Feature availability](#feature-availability)
      - [Current reported system info](#current-reported-system-info)
    - [Factory reset](#factory-reset)
      - [Feature availability](#feature-availability-1)
      - [Trigger factory reset](#trigger-factory-reset)
      - [Report factory reset status](#report-factory-reset-status)
    - [iot-hub-device-update user consent](#iot-hub-device-update-user-consent)
      - [Feature availability](#feature-availability-2)
      - [Configure current desired general consent](#configure-current-desired-general-consent)
      - [Grant user consent](#grant-user-consent)
      - [Current reported user consent status](#current-reported-user-consent-status)
    - [provisioning configuration](#provisioning-configuration)
      - [Feature availability](#feature-availability-3)
      - [Current reported provisioning configuration](#current-reported-provisioning-configuration)
    - [Reboot](#reboot)
      - [Feature availability](#feature-availability-4)
      - [Trigger reboot](#trigger-reboot)
      - [Configure wait-online reboot timeout](#configure-wait-online-reboot-timeout)
    - [Modem Info](#modem-info)
      - [Feature availability](#feature-availability-5)
      - [Current reported modem info](#current-reported-modem-info)
    - [Network status](#network-status)
      - [Feature availability](#feature-availability-6)
      - [Current reported network status](#current-reported-network-status)
    - [SSH Tunnel handling](#ssh-tunnel-handling)
      - [Feature availability](#feature-availability-7)
      - [Access to Device SSH Public Key](#access-to-device-ssh-public-key)
      - [Opening the SSH tunnel](#opening-the-ssh-tunnel)
      - [Closing the SSH tunnel](#closing-the-ssh-tunnel)
    - [Wifi commissioning service](#wifi-commissioning-service)
      - [Feature availability](#feature-availability-8)
  - [Local web service](#local-web-service)
    - [Factory reset](#factory-reset-1)
    - [Trigger reboot](#trigger-reboot-1)
    - [Reload network daemon](#reload-network-daemon)
    - [Status updates](#status-updates)
      - [Publish status](#publish-status)
      - [Republish status](#republish-status)
      - [Get status](#get-status)
  - [Update validation](#update-validation)
    - [Criteria for a successful update](#criteria-for-a-successful-update)
- [License](#license)
- [Contribution](#contribution)

<small><i><a href='http://ecotrust-canada.github.io/markdown-toc/'>Table of contents generated with markdown-toc</a></i></small>

## Configuration

### Log level

Use `RUST_LOG` environment variable in order to configure log level as described [here](https://docs.rs/env_logger/latest/env_logger/#enabling-logging).

### azure-iot-sdk

Runtime configuration options of the underlying azure-iot-sdk crate can be found [here](https://github.com/omnect/azure-iot-sdk/blob/main/README.md).

## Azure twin features

### System Info

#### Feature availability

The availability of the feature is reported by the following module twin property:
```
"system_info":
{
  "version": <ver>
}
```

The availability of the feature might be suppressed by creating the following environment variable:
```
SUPPRESS_SYSTEM_INFO=true
```

#### Current reported system info

The module reports some system information. For this purpose the module sends a reported property to the cloud.

```
"system_info": {
    "version": <vers>,
    "azure_sdk_version": "<version>",
    "omnect_device_service_version": "<version>",
    "os": {
        "name": <"omnect-os varian">,
        "version": "<version>"
    },
    "boot_time": <"utc timestamp">
},
```

### Factory reset
The module itself does not perform a factory reset.
It serves as an interface between the cloud and the built-in factory reset from the [omnect yocto image](https://github.com/omnect/meta-omnect).

#### Feature availability

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

#### Trigger factory reset

**Direct method: factory reset**

Method Name: `factory_reset`

Payload:
```
{
  "mode": <factory reset mode number>,
  "preserve":
  [
      "network", "firewall", "certificates", "applications"
  ]
}
```

Result:
{
  "status": <HTTP-Statusode>,
  "payload": {"<result>"}
}


The supported reset `mode` and the documentation in general can be found in the [meta-omnect layer](https://github.com/omnect/meta-omnect#factory-reset).

The **optional** `preserve` array can be used to define system resp. user settings that must be restored after wiping device storage. Supported are keys from [here](https://github.com/omnect/meta-omnect/blob/main/recipes-omnect/omnect-device-service/omnect-device-service/factory-reset.json) and the key "applications" if there is a [custom configuration file](https://github.com/omnect/meta-omnect/tree/main?tab=main#custom-factory-reset-configuration) in `/etc/omnect/factory-reset.d/`.

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

#### Report factory reset status

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


### iot-hub-device-update user consent

omnect os uses [iot-hub-device-update](https://github.com/Azure/iot-hub-device-update) service for device firmware update. The service is extended by a "user consent" [content handler](https://github.com/Azure/iot-hub-device-update/blob/main/docs/agent-reference/how-to-implement-custom-update-handler.md), which allows the user to individually approve a new device update for his IoT device.

The module itself does not perform a user consent. It serves as an interface between the cloud and the content handler in [omnect yocto image](https://github.com/omnect/meta-omnect).

Adapt the following environment variable in order to configure the directory used for consent files at runtime:
```
# use the following directory for consent files (defaults to "/etc/omnect/consent"), e.g.:
CONSENT_DIR_PATH: "/my/path"
```

#### Feature availability

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

The current general consent status is also exposed to the cloud as reported property. In case no desired general_consent is defined the current general_consent settings of the device are reported.

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

### provisioning configuration

omnect-os uses the [azure iot-identity-service](https://github.com/Azure/iot-identity-service) to provision the device.

#### Feature availability

The availability of the feature is reported by the following module twin property:
```
"provisioning_config":
{
  "version": <ver>
}
```

The availability of the feature might be suppressed by creating the following environment variable:
```
SUPPRESS_PROVISIONING_CONFIG=true
```

#### Current reported provisioning configuration

The module reports the current provisioning configuration status. For this purpose the module sends a reported property to the cloud.

non-x509 method:
```
"provisioning_config": {
  "version":number,
  "source": string("dps" | "manual"),
  "method": string("tpm", "sas", "symmetric_key")
}
```
x509 method:
```
"provisioning_config":{
  "version":number,
  "source": string("dps" | "manual"),
  "method": {
    "x509": {
      "expires": string(datetime), // e.g. "2024-06-21T07:12:30Z"
      "est": bool
    }
  }
}
```

### Reboot

#### Feature availability

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

#### Trigger reboot

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

#### Configure wait-online reboot timeout

There is a configurable timeout the device waits for a network connection. Further information about used network interfaces and configuration options can be found in [meta-omnect: modify-set-of-interfaces-considered-when-detecting-online-state](https://github.com/omnect/meta-omnect?tab=readme-ov-file#modify-set-of-interfaces-considered-when-detecting-online-state).

**Direct method: set_wait_online_timeout**

Method Name: `set_wait_online_timeout`

Payload:<br>
The timeout is defined in seconds. A "timeout_secs" value of 0 means no timeout. An empty payload also means no timeout to be set at all.
```
{
  "timeout_secs": <secs>
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

### Modem Info

This feature adds the ability to report status information of connected modem. The status is refreshed in an interval which can be configured by REFRESH_MODEM_INFO_INTERVAL_SECS environment variable. The default is 10min.

**NOTE**: this is an optional feature and must be explicitly turned on when
building, i.e., `cargo build --features modem_info,...`.

#### Feature availability

The availability of the feature is reported by the following module twin property:
```
"modem_info":
{
  "version": <ver>
}
```

#### Current reported modem info

The module reports the status of any attached modems. For this purpose the module sends this reported property to the cloud.

```
"modem_info":
{
  "modems": [
    {
      "bearers": [],
      "imei": "xxxxxxxx",
      "manufacturer": "Sierra Wireless, Incorporated",
      "model": "EM7455",
      "preferred_technologies": [
        12
      ],
      "revision": "SWI9X30C_02.33.03.00 r8209 CARMD-EV-FRMWR2 2019/08/28 20:59:30",
      "sims": [
        {
          "iccid": "yyyyyyyy",
          "operator": "Telekom.de"
        }
      ]
    }
  ]
}
```

### Network status

The network status is refreshed in an interval which can be configured by `REFRESH_NETWORK_STATUS_INTERVAL_SECS` environment variable. The default is 60s.

**NOTE**: Currently reporting status of modems is no supported!

#### Feature availability

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

#### Current reported network status

The module reports the status of network adapters. For this purpose the module sends this reported property to the cloud.

```
"network_status": {
  "version": 3,
  "interfaces": [
    {
      "ipv4": {
        "addrs": [
          {
            "addr": "172.18.18.97",
            "dhcp": true,
            "prefix_len": 24
          }
        ],
        "dns": [
          "172.18.18.1"
        ],
        "gateways": []
      },
      "mac": "228:95:1:114:47:14",
      "name": "eth0",
      "online": true
    },
    {
      "ipv4": {
        "addrs": [],
        "dns": [],
        "gateways": []
      },
      "mac": "228:95:1:114:47:15",
      "name": "wlan0",
      "online": false
    }
  ]
}
```

### SSH Tunnel handling

#### Feature availability

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

#### Access to Device SSH Public Key

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


#### Opening the SSH tunnel

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


#### Closing the SSH tunnel

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

### Wifi commissioning service

#### Feature availability

The availability of the feature is reported by the following module twin property:
```
"wifi_commissioning":
{
  "version": <ver>
}
```

## Local web service

omnect-device-service provides a http web service that exposes a web API over a unix domain socket.<br>
Information about the socket can be found in the appropriate [socket file](systemd/omnect-device-service.socket)<br>

The web service features is disabled by default and must be explicitly activated via environment variable `WEBSERVICE_ENABLED=true`.

### Factory reset

```
curl -X POST --unix-socket /run/omnect-device-service/api.sock http://localhost/factory-reset/v1
```

### Trigger reboot

```
curl -X POST --unix-socket /run/omnect-device-service/api.sock http://localhost/reboot/v1
```

### Reload network daemon

```
curl -X POST --unix-socket /run/omnect-device-service/api.sock http://localhost/reload-network/v1
```

### Status updates

omnect-device-service is capable to publish certain properties to a list of defined endpoints. Currently the following properties are published:
- online status: connection status to iothub
- info: software versions of various components and device boot timestamp
- timeouts: currently configured [wait-online-timeout](https://www.freedesktop.org/software/systemd/man/latest/systemd-networkd-wait-online.service.html)
- factory-reset: if there was a factory-reset in previous boot, the result is published
- network status: network adapter and its current configuration (LTE modems are currently not included). The reported structure is equal to [Current reported network status](#current-reported-network-status)

#### Publish status

Publishing messages in omnect-device-service is inspired by [centrifugo](https://centrifugal.dev/) and e.g. makes use of it in [omnect-ui](https://github.com/omnect/omnect-ui).

In order to receive updates, a http POST endpoint must be present, where omnect-device-service can post messages to. Interested endpoints must be added to "/etc/omnect/publish_endpoints.json" in the following format (headers are optional):
```
[
  {
    "url": "http://localhost:8000/api/publish",
    "headers": [
      {
        "name": "Content-Type",
        "value": "application/json"
      },
      {
        "name": "X-API-Key",
        "value": "my-api-key"
      }
    ]
  }
]
```

The publish message format is also inspired by [centrifugo](https://centrifugal.dev/). A message must define a channel and a data attribute, e.g.:
```
{
  "channel": "OnlineStatus",
  "data": {
    "iothub": true
  }
}
```
#### Republish status
The client can trigger omnect-device-service to republish its status:

```
curl -X POST --unix-socket /run/omnect-device-service/api.sock http://localhost/republish/v1
```

#### Get status
It is also possible to query ths current status directly:

```
curl -X GET --unix-socket /run/omnect-device-service/api.sock http://localhost/status/v1
```

## Update validation

On `iot-hub-device-update` update, after flashing the new root partition, we boot into the new root partition and test if the update was successful.<br>
We don't set the new root partition permanently yet. On this boot the startup of `iot-hub-device-update` is prevented and has to be triggered by `omnect-device-service`.<br>
`omnect-device-service` validates if the update was successful. If so, the new root partition is permanently set and the start of `iot-hub-device-update` gets triggered. If not, the device gets rebooted and we boot to the old root partition.<br>
The overall update validation timeout can be overwritten by `UPDATE_VALIDATION_TIMEOUT_IN_SECS` environment variable.

### Criteria for a successful update

The following checks must be passed in order to successfully validate an update:
- system is running
- adu-agent could be started successfully
- omnect-device-service is connected to iot-hub (successfully provisioned)

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

copyright (c) 2024 conplement AG<br>
Content published under the Apache License Version 2.0 or MIT license, are marked as such. They may be used in accordance with the stated license conditions.
