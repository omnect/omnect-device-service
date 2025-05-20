use anyhow::{Context, Result};
use regex_lite::Regex;
use std::fmt::Display;
use std::{cmp::Ordering, env, fmt, fs};

macro_rules! sw_versions_path {
    () => {
        env::var("SW_VERSIONS_PATH").unwrap_or("/etc/sw-versions".to_string())
    };
}

static OS_VERSION_REGEX: &str = r"^(\d+).(\d+).(\d+).(\d+)$";
static SW_VERSION_FILE_REGEX: &str = r"^.* (\d+).(\d+).(\d+).(\d+)$";

#[derive(PartialEq, Eq)]
pub struct OmnectOsVersion {
    major: u32,
    minor: u32,
    patch: u32,
    build: u32,
}

impl PartialOrd for OmnectOsVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let mut order = self.major.cmp(&other.major);

        if order == Ordering::Equal {
            order = self.minor.cmp(&other.minor);
        }

        if order == Ordering::Equal {
            order = self.patch.cmp(&other.patch);
        }

        if order == Ordering::Equal {
            order = self.build.cmp(&other.build);
        }

        Some(order)
    }
}

impl Display for OmnectOsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}",
            self.major, self.minor, self.patch, self.build
        )
    }
}

impl OmnectOsVersion {
    pub fn from_string(version: &str) -> Result<OmnectOsVersion> {
        let regex = Regex::new(OS_VERSION_REGEX).context("failed to create regex")?;

        let c = regex
            .captures(version.trim())
            .context("failed to create captures")?;

        Ok(OmnectOsVersion {
            major: c[1].to_string().parse().context("failed to parse major")?,
            minor: c[2].to_string().parse().context("failed to parse minor")?,
            patch: c[3].to_string().parse().context("failed to parse patch")?,
            build: c[4].to_string().parse().context("failed to parse build")?,
        })
    }

    pub fn from_sw_versions_file() -> Result<OmnectOsVersion> {
        let sw_versions =
            fs::read_to_string(sw_versions_path!()).context("failed to read sw-versions file")?;

        let regex = Regex::new(SW_VERSION_FILE_REGEX).context("failed to create regex")?;

        let c = regex
            .captures(sw_versions.trim())
            .context(format!("no captures found in: {sw_versions}"))?;

        Ok(OmnectOsVersion {
            major: c[1].to_string().parse().context("failed to parse major")?,
            minor: c[2].to_string().parse().context("failed to parse minor")?,
            patch: c[3].to_string().parse().context("failed to parse patch")?,
            build: c[4].to_string().parse().context("failed to parse build")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test() {
        assert!(
            OmnectOsVersion::from_string("0.1.2.3").unwrap()
                == OmnectOsVersion::from_string("0.1.2.3").unwrap()
        );
        assert!(
            OmnectOsVersion::from_string("0.1.2.3").unwrap()
                < OmnectOsVersion::from_string("0.1.2.4").unwrap()
        );
        assert!(
            OmnectOsVersion::from_string("0.1.2.3").unwrap()
                < OmnectOsVersion::from_string("0.1.3.3").unwrap()
        );
        assert!(
            OmnectOsVersion::from_string("0.1.2.3").unwrap()
                < OmnectOsVersion::from_string("0.2.2.3").unwrap()
        );
        assert!(
            OmnectOsVersion::from_string("0.1.2.3").unwrap()
                < OmnectOsVersion::from_string("1.1.2.3").unwrap()
        );
        assert!(
            OmnectOsVersion::from_string("2.1.2.3").unwrap()
                > OmnectOsVersion::from_string("1.1.2.3").unwrap()
        );

        assert!(OmnectOsVersion::from_string("1234..12.12").is_err());
        assert!(OmnectOsVersion::from_string("12").is_err());
        assert!(OmnectOsVersion::from_string("123.123.123.123.123").is_err());
        assert!(OmnectOsVersion::from_string("asdf.123.123.123").is_err());

        crate::common::set_env_var("SW_VERSIONS_PATH", "testfiles/positive/sw-versions");

        assert!(
            OmnectOsVersion::from_sw_versions_file().unwrap()
                == OmnectOsVersion::from_string("4.0.10.0").unwrap()
        );
    }
}
