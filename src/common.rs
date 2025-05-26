use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};

pub enum RootPartition {
    A,
    B,
}

impl RootPartition {
    pub fn as_str(&self) -> &str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }

    pub fn from_index_string(index: String) -> Result<Self> {
        match index
            .parse::<u8>()
            .context("cannot parse root partition index")?
        {
            2 => Ok(Self::A),
            3 => Ok(Self::B),
            _ => bail!("invalid root partition index"),
        }
    }

    pub fn index(&self) -> u8 {
        match self {
            Self::A => 2,
            Self::B => 3,
        }
    }

    pub fn root_update_params(&self) -> &str {
        match self {
            Self::A => "stable,copy1",
            Self::B => "stable,copy2",
        }
    }

    pub fn bootloader_update_params(&self) -> &str {
        "stable,bootloader"
    }

    pub fn other(&self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    #[cfg(not(feature = "mock"))]
    pub fn current() -> Result<RootPartition> {
        static DEV_OMNECT: &str = "/dev/omnect/";

        let current_root = std::fs::read_link(DEV_OMNECT.to_owned() + "rootCurrent")
            .context("current_root: getting current root device")?;

        if current_root
            == std::fs::read_link(DEV_OMNECT.to_owned() + "rootA")
                .context("current_root: getting rootA")?
        {
            return Ok(RootPartition::A);
        }

        if current_root
            == std::fs::read_link(DEV_OMNECT.to_owned() + "rootB")
                .context("current_root: getting rootB")?
        {
            return Ok(RootPartition::B);
        }

        bail!("current_root: device booted from unknown root")
    }

    #[cfg(feature = "mock")]
    pub fn current() -> Result<RootPartition> {
        Ok(RootPartition::A)
    }
}

pub fn to_json_file<T, P>(value: &T, path: P, create: bool) -> Result<()>
where
    T: ?Sized + Serialize,
    P: AsRef<std::path::Path> + std::fmt::Debug,
{
    serde_json::to_writer_pretty(
        std::fs::OpenOptions::new()
            .write(true)
            .create(create)
            .truncate(true)
            .open(&path)
            .context(format!("failed to open for write: {path:?}"))?,
        value,
    )
    .context(format!("failed to serialize json to: {path:?}"))
}

pub fn from_json_file<P, T>(path: P) -> Result<T>
where
    P: AsRef<std::path::Path> + std::fmt::Debug,
    T: DeserializeOwned,
{
    serde_json::from_reader(
        std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .context(format!("failed to open for read: {path:?}"))?,
    )
    .context(format!("failed to deserialize json from: {path:?}"))
}

pub fn path_ends_with<P>(path: P, end: &str) -> bool
where
    P: AsRef<std::path::Path>,
{
    path.as_ref().to_string_lossy().ends_with(end)
}
