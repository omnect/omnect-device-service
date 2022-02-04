# demo-portal-module

## Instruction
This module is based on the ICS_DeviceManagement [ics-dm-iot-module-rs](https://github.com/ICS-DeviceManagement/ics-dm-iot-module-rs). All information you need to build the project can be found there.


## What is demo-portal-module
This [module](src/lib.rs) on the device side serves the purpose to **demonstrate** both the device downgrade and a factory rest workflow for the demo portal.

A [downgrade sequence diagram](docs/downgrade.png) illustrates this process.<br>

**Interface syntax Direct method: downgrade**

Method Name:

**downgrade**

Payload:
```
{
"services":"<stop|start>",
"version": "<downgrade version>"
}
```

Result:
{"status": <HTTP-Statusode>,"payload":"<OK/NOK>"}


**Interface syntax Direct method: factory reset**

Method Name:

**factory**

Payload:
```
{
"reset":"<factory reset type>"
}
```

Result:
{"status": <HTTP-Statusode>,"payload":"<OK/NOK>"}



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
