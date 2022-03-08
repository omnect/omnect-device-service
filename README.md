# demo-portal-module

## Instruction
This module is based on the ICS_DeviceManagement [iot-client-template-rs](https://github.com/ICS-DeviceManagement/iot-client-template-rs). All information you need to build the project can be found there.


## What is demo-portal-module
This module on the device side is designed to **demonstrate** a factory rest workflow.

The module itself does not perform a factory reset.
It serves as an interface between the cloud and the built-in factory reset from the [ics-dm yocto image](https://github.com/ICS-DeviceManagement/meta-ics-dm).

A function was specified for this purpose, a so-called direct method which is described below.

**Direct method: factory reset**

Method Name:

**factory**

Payload:
```
{
  "reset":"<factory reset type>"
}
```

Result:
{"status": <HTTP-Statusode>,"payload":"<result>"}


The supported "factory reset type" and the documentation in general about the factory reset can be found in the [meta-ics-dm layer](https://github.com/ICS-DeviceManagement/meta-ics-dm#factory-reset).

In case the method was successful received by the module the return value of the method looks like this:

```
{"status":200,"payload":"Ok"}
```

In all other cases there will be a meaningful error message in the status and payload.

Performing a factory reset also triggers a device restart. The restart time of a device depends on the selected factory reset. After the device has been restarted, this module sends a confirmation to the cloud as reported property in the module twin.

```
"factory_reset_status": {
    "date": "<UTC time of the factory reset status>",
    "status": "<status>"
}
```

The following status information is defined:
 - "succeeded"
 - "failed"
 - "unexpected factory reset type"


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
