use anyhow::{Context, Result};
use regex_lite::Regex;
use std::fmt::Display;
use std::{cmp::Ordering, fmt, fs};

macro_rules! sw_versions_path {
    () => {{
        static SW_VERSIONS_PATH_DEFAULT: &'static str = "/etc/sw-versions";
        std::env::var("SW_VERSIONS_PATH").unwrap_or(SW_VERSIONS_PATH_DEFAULT.to_string())
    }};
}

pub struct OmnectOsVersion {
    major: u32,
    minor: u32,
    patch: u32,
    build: u32,
}

impl PartialOrd for OmnectOsVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OmnectOsVersion {
    fn cmp(&self, other: &Self) -> Ordering {
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

        order
    }
}

impl PartialEq for OmnectOsVersion {
    fn eq(&self, other: &Self) -> bool {
        self.major == other.major
            && self.minor == other.minor
            && self.patch == other.patch
            && self.build == other.build
    }
}

impl Eq for OmnectOsVersion {}

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
        let regex = Regex::new(r#"^(\d*).(\d*).(\d*).(\d*)$"#).context("failed to create regex")?;

        let c = regex
            .captures(version)
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

        // ToDo: re-check: r"^.* (\d*).(\d*).(\d*).(\d*)$"
        // dos2unix
        let regex = Regex::new(r".* (\d*).(\d*).(\d*).(\d*)").context("failed to create regex")?;

        let c = regex
            .captures(&sw_versions)
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

        std::env::set_var("SW_VERSIONS_PATH", "testfiles/positive/sw-versions");

        assert!(
            OmnectOsVersion::from_sw_versions_file().unwrap()
                == OmnectOsVersion::from_string("4.0.10.0").unwrap()
        );
    }
}
