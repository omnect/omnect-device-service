use ics_dm_azure::client::{DirectMethod, EventHandler, IotHubModuleClient};
use log::debug;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::{fs, fs::File, fs::OpenOptions};
use std::{thread, time};

#[derive(Deserialize, Debug, Serialize)]
struct Downgrade {
    services: String,
    version: String,
}

#[derive(Deserialize, Debug, Serialize)]
struct Factory {
    reset: String,
}

pub struct DemoPortal<'b> {
    direct_methods: Option<&'b HashMap<String, DirectMethod<'b>>>,
}

impl<'a> EventHandler for &'a DemoPortal<'_> {
    fn handle_message(&self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("handle_message called in DemoPortal");
        Ok(())
    }

    fn handle_twin(&self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("handle_message called in DemoPortal");
        Ok(())
    }

    fn get_direct_methods(&self) -> Option<&'a HashMap<String, DirectMethod>> {
        self.direct_methods
    }
}

impl DemoPortal<'_> {
    pub fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut client = IotHubModuleClient::from_identity_service(self)?;

        let hundred_millis = time::Duration::from_millis(100);

        loop {
            client.do_work();

            thread::sleep(hundred_millis);
        }
    }
}

pub fn factory(
    in_json: serde_json::Value,
) -> Result<Option<serde_json::Value>, Box<dyn std::error::Error>> {
    let content: Factory = serde_json::from_value(in_json).unwrap();
    let result = "OK";
    debug!("direct method called with: {:?}", content.reset);
    OpenOptions::new()
        .write(true)
        .create(true)
        .open("/tmp/factory-reset")
        .expect("/tmp/factory-reset");

    fs::write(
        "/tmp/factory-reset",
        format!("{}\n", content.reset.as_str()),
    )
    .expect("Can't write");

    Ok(Some(serde_json::to_value(result).unwrap()))
}

pub fn downgrade(
    in_json: serde_json::Value,
) -> Result<Option<serde_json::Value>, Box<dyn std::error::Error>> {
    let content: Downgrade = serde_json::from_value(in_json).unwrap();
    let mut result = "OK";
    debug!("direct method called with: {:?}", content.services);
    match content.services.as_str() {
        "stop" => {
            OpenOptions::new()
                .write(true)
                .create(true)
                .open("/tmp/downgrade-stop")
                .expect("/tmp/downgrade-stop");

            fs::write(
                "/tmp/downgrade-stop",
                format!("{}\n", content.version.as_str()),
            )
            .expect("Can't write");
        }
        "start" => {
            File::create("/tmp/downgrade-start")?;
        }
        _ => {
            result = "NOK";
        }
    }

    Ok(Some(serde_json::to_value(result).unwrap()))
}

fn make_direct_method<'a, F>(f: F) -> DirectMethod<'a>
where
    F: Fn(serde_json::Value) -> Result<Option<serde_json::Value>, Box<dyn std::error::Error>>
        + 'static,
{
    Box::new(f) as DirectMethod
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let methods = HashMap::from([
        (
            String::from("closure1"),
            make_direct_method(|_in_json| Ok(None)),
        ),
        (
            String::from("closure2"),
            make_direct_method(|in_json| {
                let mut out_json = json!({
                    "closure2": "called",
                    "location": "nowhere"
                });

                out_json["in_json"] = in_json;

                Ok(Some(out_json))
            }),
        ),
        (String::from("factory"), Box::new(factory)),
        (String::from("downgrade"), Box::new(downgrade)),
    ]);

    let module = DemoPortal {
        direct_methods: Some(&methods),
    };

    module.run()
}
