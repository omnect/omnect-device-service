use log::debug;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::io::Write;
use std::thread;
use std::time::Duration;
use std::{fs, fs::OpenOptions};

#[derive(Deserialize, Debug, Serialize)]
struct Factory {
    reset: String,
}

pub fn factory(
    in_json: serde_json::Value,
) -> Result<Option<serde_json::Value>, Box<dyn Error + Send + Sync>> {
    let content: Factory;
    let result;
    match serde_json::from_value(in_json) {
        Ok(param) => {
            content = param;
            debug!("direct method called with: {:?}", content.reset);
            //   thread::spawn(|| {
            //     factory_write_to_file_thread(content.reset);
            //   });
            result = write_to_file(content.reset);
        }
        _ => {
            result = "param not supported".to_string();
        }
    }

    Ok(Some(serde_json::to_value(result).unwrap()))
}

// fn write_to_file_thread(reset_type: String) {

// thread::sleep(Duration::from_millis(2000));

// match OpenOptions::new()
//     .write(true)
//     .create(false)
//     .open("/run/factory-reset")
// {
//     Ok(mut file) => {
//         let test = format!("{}\n", reset_type.as_str());

//         file.write(test.as_bytes()).unwrap();
//         debug!({},"Ok".to_string());
//     }
//     _ => {
//         debug!({},"file not exists".to_string());
//     }
// }
//

//     OpenOptions::new()
//     .write(true)
//     .create(true)
//     .open("/tmp/factory-reset")
//     .expect("/tmp/factory-reset");

//     fs::write(
//         "/tmp/factory-reset",
//         format!("{}\n", reset_type.as_str()),
//     )
//     .expect("Can't write");
// }

fn write_to_file(reset_type: String) -> String {
    let res;
    match OpenOptions::new()
        .write(true)
        .create(false)
        .open("/run/factory-reset")
    {
        Ok(mut file) => {
            let content = format!("{}\n", reset_type.as_str());

            file.write(content.as_bytes()).unwrap();
            res = "Ok".to_string();
        }
        _ => {
            res = "file not exists".to_string();
        }
    }
    res
}
