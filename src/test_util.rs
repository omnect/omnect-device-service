use cp_r::CopyOptions;
use env_logger::{Builder, Env};
use lazy_static::lazy_static;
use log::error;
use std::fs::copy;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::PathBuf;

const TMPDIR_FORMAT_STR: &'static str = "/tmp/omnect-device-service-tests/";

lazy_static! {
    static ref LOG: () = if cfg!(debug_assertions) {
        Builder::from_env(Env::default().default_filter_or("debug")).init()
    } else {
        Builder::from_env(Env::default().default_filter_or("info")).init()
    };
}

pub struct Testrunner {
    dirpath: std::string::String,
}

impl Testrunner {
    pub fn new(prefix: &str) -> Testrunner {
        lazy_static::initialize(&LOG);
        let dirpath = format!("{}{}", TMPDIR_FORMAT_STR, prefix);
        create_dir_all(&dirpath).unwrap();
        Testrunner { dirpath }
    }

    pub fn copy_directory(&self, dir: &str) -> PathBuf {
        let destdir = String::from(dir);
        let destdir = destdir.split('/').last().unwrap();
        let path = PathBuf::from(format!("{}/{}", self.dirpath, destdir));
        CopyOptions::new().copy_tree(dir, &path).unwrap();
        path
    }

    pub fn copy_file(&self, file: &str) -> PathBuf {
        let destfile = String::from(file);
        let destfile = destfile.split('/').last().unwrap();
        let path = PathBuf::from(format!("{}/{}", self.dirpath, destfile));
        copy(file, &path).unwrap();
        path
    }

    pub fn get_dirpath(&self) -> String {
        self.dirpath.clone()
    }
}

impl Drop for Testrunner {
    fn drop(&mut self) {
        // place your cleanup code here
        remove_dir_all(&self.dirpath).unwrap_or_else(|err| {
            // ignore all errors if dir cannot be deleted
            error!("cannot remove_dir_all: {}", err);
        });
    }
}
