//! Behaviour contract of the healthcheck shell scripts. The scripts are copied
//! into a sandbox that mirrors the deployment layout of meta-omnect, and the
//! system tools they query are replaced by stubs, so the tests pin what a check
//! reports rather than the state of the machine running them.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

const HEALTHCHECK_SRC: &str = "healthcheck";
const HEALTHCHECK_DIR_ASSIGNMENT: &str = "HEALTHCHECK_DIR=/usr/lib/omnect/healthcheck";
const CFGFILEDIR_ASSIGNMENT: &str = "CFGFILEDIR=/etc/omnect/health_check";

/// meta-omnect installs the check scripts executable and the library readable.
const EXECUTABLE_MODE: u32 = 0o755;
const LIBRARY_MODE: u32 = 0o644;

const RATING_GREEN: i32 = 0;
const RATING_RED: i32 = 2;

const CHECK_SCRIPTS: &[&str] = &[
    "omnect_health__coredumps.sh",
    "omnect_health__crash_loop.sh",
    "omnect_health__services.sh",
    "omnect_health__system_running.sh",
    "omnect_health__timesync.sh",
];

const COREDUMPS_CONFIG: &str = r#"[ { "name": "coredumps", "type": "coredumps" } ]"#;

/// Two services where one name is a prefix of the other up to a hyphen, which
/// `grep -w` treats as a word boundary.
const OVERLAPPING_SERVICES: &str = r#"{
    "default-ratings": [],
    "services": [ { "service": "aziot" }, { "service": "aziot-certd" } ]
}"#;

/// Reports one healthy service so the crash-loop check finds something to look at.
const SYSTEMCTL_STUB: &str = r#"case "$*" in
    *list-units*) echo "stub.service loaded active running Stub" ;;
    *show*) echo "ActiveState=active"; echo "SubState=running" ;;
esac
exit 0
"#;

const TIMEDATECTL_STUB: &str = "echo NTP=yes\necho NTPSynchronized=yes\n";

fn write_with_mode(path: &Path, content: &str, mode: u32) {
    fs::write(path, content).expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod script");
}

fn read_script(name: &str) -> String {
    fs::read_to_string(Path::new(HEALTHCHECK_SRC).join(name)).expect("read healthcheck script")
}

fn require_jq() {
    Command::new("jq")
        .arg("--version")
        .output()
        .expect("jq not found in PATH, but the healthcheck scripts need it to read their config");
}

struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new(checks_config: &str) -> Self {
        require_jq();

        let sandbox = Sandbox {
            dir: TempDir::new().expect("temp dir"),
        };
        for dir in [sandbox.lib(), sandbox.etc(), sandbox.bin(), sandbox.run()] {
            fs::create_dir_all(dir).expect("create sandbox dir");
        }

        write_with_mode(
            &sandbox.lib().join("healthchecklib.sh"),
            &read_script("healthchecklib.sh"),
            LIBRARY_MODE,
        );
        for script in CHECK_SCRIPTS {
            write_with_mode(
                &sandbox.lib().join(script),
                &read_script(script),
                EXECUTABLE_MODE,
            );
        }
        sandbox.install_dispatcher();

        fs::write(
            sandbox.etc().join("omnect_health_checks.json"),
            checks_config,
        )
        .expect("write check config");
        fs::write(
            sandbox.etc().join("omnect_service_log_analysis.json"),
            read_script("omnect_service_log_analysis.json"),
        )
        .expect("write service log config");

        sandbox.stub("coredumpctl", "");
        sandbox.stub("timedatectl", TIMEDATECTL_STUB);
        sandbox.stub("systemctl", SYSTEMCTL_STUB);
        sandbox
    }

    fn lib(&self) -> PathBuf {
        self.dir.path().join("lib")
    }

    fn etc(&self) -> PathBuf {
        self.dir.path().join("etc")
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    fn run(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    /// The dispatcher hard-codes its deployment paths, so redirect them at the sandbox.
    fn install_dispatcher(&self) {
        let source = read_script("omnect_health_check.sh");
        for assignment in [HEALTHCHECK_DIR_ASSIGNMENT, CFGFILEDIR_ASSIGNMENT] {
            assert!(
                source.contains(assignment),
                "omnect_health_check.sh no longer contains \"{assignment}\""
            );
        }

        let redirected = source
            .replace(
                HEALTHCHECK_DIR_ASSIGNMENT,
                &format!("HEALTHCHECK_DIR={}", self.lib().display()),
            )
            .replace(
                CFGFILEDIR_ASSIGNMENT,
                &format!("CFGFILEDIR={}", self.etc().display()),
            );
        write_with_mode(
            &self.lib().join("omnect_health_check.sh"),
            &redirected,
            EXECUTABLE_MODE,
        );
    }

    fn stub(&self, name: &str, body: &str) {
        write_with_mode(
            &self.bin().join(name),
            &format!("#!/bin/bash\n{body}"),
            EXECUTABLE_MODE,
        );
    }

    fn set_service_analysis(&self, config: &str) {
        fs::write(self.etc().join("omnect_service_log_analysis.json"), config)
            .expect("write service log config");
    }

    fn command(&self, script: &str) -> Command {
        let path = format!(
            "{}:{}",
            self.bin().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(self.lib().join(script));
        command
            .env("PATH", path)
            .env("CFGDIR", self.etc())
            .env("LOGDIR", self.run())
            .env("ANALYSISDIR", self.run())
            .current_dir(self.dir.path());
        command
    }

    fn run_script(&self, script: &str, args: &[&str]) -> Report {
        Report::new(
            self.command(script)
                .args(args)
                .output()
                .expect("run healthcheck script"),
        )
    }
}

struct Report {
    rating: i32,
    stdout: String,
    stderr: String,
}

impl Report {
    fn new(output: Output) -> Self {
        Report {
            rating: output
                .status
                .code()
                .expect("healthcheck script killed by signal"),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// The names of the checks that reported a rating, in the order they ran.
    fn rated_names(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                fields.next().filter(|r| r.starts_with('['))?;
                fields.next()
            })
            .collect()
    }
}

fn stock_config() -> String {
    read_script("omnect_health_checks.json")
}

#[test]
fn a_partial_check_name_is_rejected() {
    let sandbox = Sandbox::new(&stock_config());

    let report = sandbox.run_script("omnect_health_check.sh", &["check", "aziot"]);

    assert_ne!(
        report.rating, RATING_GREEN,
        "a name that matches no check must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        report.stderr.contains("unknown check name(s) [aziot]"),
        "a name that matches no check must be reported as unknown:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn an_exact_check_name_runs_only_the_named_check() {
    let sandbox = Sandbox::new(&stock_config());

    let report = sandbox.run_script("omnect_health_check.sh", &["check", "aziot-certd"]);

    assert_eq!(
        report.rated_names(),
        ["aziot-certd"],
        "only the named check may run:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_check_type_runs_every_check_of_that_type() {
    let sandbox = Sandbox::new(&stock_config());

    let report = sandbox.run_script("omnect_health_check.sh", &["check", "timesync"]);

    assert_eq!(
        report.rated_names(),
        ["timesync"],
        "selecting a type must run the checks of that type:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_service_selection_does_not_match_a_shorter_service_name() {
    let sandbox = Sandbox::new(&stock_config());
    sandbox.set_service_analysis(OVERLAPPING_SERVICES);

    let report = sandbox.run_script("omnect_health__services.sh", &["check", "aziot-certd"]);

    assert_eq!(
        report.rated_names(),
        ["aziot-certd"],
        "only the requested service may be rated:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn check_rates_a_core_dump_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub("coredumpctl", "echo 'SIGSEGV /usr/bin/crasher'\n");

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a core dump must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        report.stdout.contains("OVERALL:  [RED]"),
        "check must summarize the core dump as red:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn check_rates_an_undisturbed_system_green() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_GREEN,
        "a system without core dumps must be rated green:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn get_infos_rates_a_core_dump_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub("coredumpctl", "echo 'SIGSEGV /usr/bin/crasher'\n");

    let report = sandbox.run_script("omnect_health_check.sh", &["get-infos"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a core dump must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        report.stdout.contains("/usr/bin/crasher"),
        "get-infos must report the core dump it found:\n{}{}",
        report.stdout,
        report.stderr
    );
}
