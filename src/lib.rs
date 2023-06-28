pub mod bootloader_env;
pub mod systemd;
pub mod twin;
pub mod update_validation;

use anyhow::Result;
use twin::Twin;
mod test_util;

#[tokio::main]
pub async fn run() -> Result<()> {
    // ToDo rem
    //Twin::run(Some("HostName=omnect-cp-dev-iot-hub.azure-devices.net;DeviceId=jza-gateway-devel-rpi;ModuleId=omnect-device-service;SharedAccessKey=ZK5inTKxbz4yx6ccW0vrm86rg2XxIqGl9SaorNXEb4s=")).await

    Twin::run(None).await
}
