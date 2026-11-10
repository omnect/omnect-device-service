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
const SERVICE_EXITLOGDIR_ASSIGNMENT: &str =
    "SERVICE_EXITLOGDIR=\"/var/run/${SERVICE_EXITLOGDIRNAME}\"";

/// meta-omnect installs the check scripts executable and the library readable.
const EXECUTABLE_MODE: u32 = 0o755;
const LIBRARY_MODE: u32 = 0o644;

const RATING_GREEN: i32 = 0;
const RATING_YELLOW: i32 = 1;
const RATING_RED: i32 = 2;

/// The dispatcher reads the whole check configuration in one `jq` call, and
/// does not read it a second time.
const CONFIG_READS: usize = 1;

const CHECK_SCRIPTS: &[&str] = &[
    "omnect_health__coredumps.sh",
    "omnect_health__crash_loop.sh",
    "omnect_health__services.sh",
    "omnect_health__system_running.sh",
    "omnect_health__timesync.sh",
];

const COREDUMPS_CONFIG: &str = r#"[ { "name": "coredumps", "type": "coredumps" } ]"#;

/// Eleven entries - as many as the stock configuration has - but none of type
/// "services", so every `jq` call observed comes from the main script itself.
const ELEVEN_CHECKS_CONFIG: &str = r#"[
    { "name": "cores1", "type": "coredumps" },
    { "name": "cores2", "type": "coredumps" },
    { "name": "cores3", "type": "coredumps" },
    { "name": "running1", "type": "system_running" },
    { "name": "running2", "type": "system_running" },
    { "name": "running3", "type": "system_running" },
    { "name": "loop1", "type": "crash_loop" },
    { "name": "loop2", "type": "crash_loop" },
    { "name": "loop3", "type": "crash_loop" },
    { "name": "time1", "type": "timesync" },
    { "name": "time2", "type": "timesync" }
]"#;

/// Two services where one name is a prefix of the other up to a hyphen, so a
/// selection of the shorter name must not match the longer one.
const OVERLAPPING_SERVICES: &str = r#"{
    "default-ratings": [],
    "services": [ { "service": "aziot" }, { "service": "aziot-certd" } ]
}"#;

/// Two services that both override the default rating of the same condition,
/// in opposite directions.
const PER_SERVICE_RATINGS: &str = r#"{
    "default-ratings": [
        { "condition": ".exitstatus == \"HUP\"", "rating": "yellow" }
    ],
    "services": [
        {
            "service": "lenient",
            "ratings": [ { "condition": ".exitstatus == \"HUP\"", "rating": "green" } ]
        },
        {
            "service": "strict",
            "ratings": [ { "condition": ".exitstatus == \"HUP\"", "rating": "red" } ]
        }
    ]
}"#;

/// Two services whose rules name a rating the script cannot know: one gets it
/// wrong by a capital letter, the other leaves the attribute out altogether.
const MISNAMED_RATINGS: &str = r#"{
    "default-ratings": [],
    "services": [
        {
            "service": "typo",
            "ratings": [ { "condition": ".exitstatus == \"SEGV\"", "rating": "Red" } ]
        },
        {
            "service": "nameless",
            "ratings": [ { "condition": ".exitstatus == \"SEGV\"" } ]
        }
    ]
}"#;

/// A service whose own rules cover only one condition, so an entry that none of
/// them matches has to fall back to the unrated default.
const UNMATCHED_SERVICE_RATING: &str = r#"{
    "default-ratings": [
        { "condition": ".exitstatus == \"SEGV\"", "rating": "red" }
    ],
    "services": [
        {
            "service": "picky",
            "ratings": [ { "condition": ".exitstatus == \"HUP\"", "rating": "yellow" } ]
        }
    ]
}"#;

/// The very same rules as PER_SERVICE_RATINGS, with the two services swapped,
/// as a configuration update reordering them would leave them.
const PER_SERVICE_RATINGS_SWAPPED: &str = r#"{
    "default-ratings": [
        { "condition": ".exitstatus == \"HUP\"", "rating": "yellow" }
    ],
    "services": [
        {
            "service": "strict",
            "ratings": [ { "condition": ".exitstatus == \"HUP\"", "rating": "red" } ]
        },
        {
            "service": "lenient",
            "ratings": [ { "condition": ".exitstatus == \"HUP\"", "rating": "green" } ]
        }
    ]
}"#;

/// A service that tells a clean exit from a crash, so a rating taken before a
/// crash is green and one taken after it is red.
const CRASH_RATED_SERVICE: &str = r#"{
    "default-ratings": [
        { "condition": ".exitstatus == \"clean\"", "rating": "green" },
        { "condition": ".exitstatus == \"SEGV\"", "rating": "red" }
    ],
    "services": [ { "service": "picky" } ]
}"#;

/// A check whose name consists of more than one word, which the selection on
/// the command line has to keep together.
const MULTIWORD_NAME_CONFIG: &str = r#"[
    { "name": "disk space", "type": "coredumps" },
    { "name": "time", "type": "timesync" }
]"#;

/// Two services selected by a single `extra-args` value that is broken across
/// lines, which JSON allows and which must stay the arguments of one check.
const NEWLINE_EXTRA_ARGS_CONFIG: &str = r#"[
    { "type": "services", "extra-args": "svc-a\nsvc-b" }
]"#;

/// Two services without any own rules, so they are rated by the defaults.
const TWO_SERVICES: &str = r#"{
    "default-ratings": [
        { "condition": ".exitstatus == \"SEGV\"", "rating": "red" }
    ],
    "services": [ { "service": "svc-a" }, { "service": "svc-b" } ]
}"#;

/// Rates green the very condition that the stricter policy below no longer
/// mentions, so a stale rating stands out when the policy is replaced.
const SEGV_RATED_GREEN: &str = r#"{
    "default-ratings": [
        { "condition": ".exitstatus == \"SEGV\"", "rating": "green" }
    ],
    "services": [ { "service": "picky" } ]
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

/// Puts a directory where a file is expected. Reading or writing it fails
/// with `EISDIR`, which permission bits cannot stand in for, because the
/// suite may run as a user who is allowed to ignore them.
fn block_path(path: &Path) {
    fs::create_dir(path).expect("create the directory that blocks the path");
}

/// The number of calls the counting `jq` wrapper recorded.
fn jq_calls(counter: &Path) -> usize {
    fs::read_to_string(counter)
        .unwrap_or_default()
        .lines()
        .count()
}

fn require_jq() {
    Command::new("jq")
        .arg("--version")
        .output()
        .expect("jq not found in PATH, but the healthcheck scripts need it to read their config");
}

/// The real `jq`, resolved before any sandbox wrapper shadows it.
fn real_jq() -> PathBuf {
    let output = Command::new("bash")
        .args(["-c", "command -v jq"])
        .output()
        .expect("look up jq");
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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

        sandbox.stub("journalctl", "");
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

    fn root(&self) -> PathBuf {
        self.dir.path().to_path_buf()
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

    /// Installs `omnect_service_log.sh`, the StopPost hook that writes the
    /// exit logs these checks read, with its deployment paths redirected at
    /// the sandbox.
    fn install_service_log(&self) {
        let source = read_script("omnect_service_log.sh");
        for assignment in [HEALTHCHECK_DIR_ASSIGNMENT, SERVICE_EXITLOGDIR_ASSIGNMENT] {
            assert!(
                source.contains(assignment),
                "omnect_service_log.sh no longer contains \"{assignment}\""
            );
        }

        let redirected = source
            .replace(
                HEALTHCHECK_DIR_ASSIGNMENT,
                &format!("HEALTHCHECK_DIR={}", self.lib().display()),
            )
            .replace(
                SERVICE_EXITLOGDIR_ASSIGNMENT,
                &format!("SERVICE_EXITLOGDIR={}", self.run().display()),
            );
        write_with_mode(
            &self.lib().join("omnect_service_log.sh"),
            &redirected,
            EXECUTABLE_MODE,
        );
        self.stub("journalctl", "echo \"stub journal\"\n");
    }

    /// Installs a `jq` that records what a check would have found at the exit
    /// log's own path at the moment the first entry is being written.
    fn stub_jq_watching_the_exit_log(&self, service: &str, observed: &Path) {
        self.stub(
            "jq",
            &format!(
                r#"if [ "$1" = "-n" ]; then
    if [ -e "{log}" ]; then
        printf 'present with %s bytes\n' "$(wc -c < "{log}")" >>"{observed}"
    else
        printf 'absent\n' >>"{observed}"
    fi
fi
exec "{jq}" "$@"
"#,
                log = self.run().join(format!("{service}.exit-log")).display(),
                observed = observed.display(),
                jq = real_jq().display()
            ),
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

    /// Writes the exit log that `omnect_service_log.sh` would have left behind.
    fn write_exit_log(&self, service: &str, exitstatus: &str) {
        fs::write(
            self.run().join(format!("{service}.exit-log")),
            format!(
                r#"{{ "service": "{service}", "infos": [ {{ "exitstatus": "{exitstatus}" }} ] }}"#
            ),
        )
        .expect("write exit log");
    }

    /// Installs a check script of the given type, so a configuration entry
    /// can select it.
    fn install_check(&self, check_type: &str, body: &str) {
        write_with_mode(
            &self.lib().join(format!("omnect_health__{check_type}.sh")),
            &format!("#!/bin/bash\n{body}"),
            EXECUTABLE_MODE,
        );
    }

    /// Installs a counting `jq` wrapper and returns the file it records calls in.
    fn count_jq_calls(&self) -> PathBuf {
        let counter = self.dir.path().join("jq-calls");
        self.stub(
            "jq",
            &format!(
                "echo call >>\"{}\"\nexec \"{}\" \"$@\"\n",
                counter.display(),
                real_jq().display()
            ),
        );
        counter
    }

    fn command(&self, script: &str) -> Command {
        let mut command = Command::new(self.lib().join(script));
        self.set_env(&mut command);
        command.current_dir(self.dir.path());
        command
    }

    fn set_env(&self, command: &mut Command) {
        let path = format!(
            "{}:{}",
            self.bin().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .env("PATH", path)
            .env("CFGDIR", self.etc())
            .env("LOGDIR", self.run())
            .env("ANALYSISDIR", self.run());
    }

    fn run_script(&self, script: &str, args: &[&str]) -> Report {
        Report::new(
            self.command(script)
                .args(args)
                .output()
                .expect("run healthcheck script"),
        )
    }

    /// Mirrors `bash <bare-name>` from the script's own directory, the one
    /// invocation form in which `${BASH_SOURCE[0]}` carries no directory part.
    fn run_script_by_bare_name(&self, script: &str, args: &[&str]) -> Report {
        let mut command = Command::new("bash");
        self.set_env(&mut command);
        command.arg(script).args(args).current_dir(self.lib());
        Report::new(command.output().expect("run healthcheck script"))
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
        self.ratings().into_iter().map(|(_, name)| name).collect()
    }

    /// The reported ratings as `(rating, name)` pairs, in the order they ran.
    fn ratings(&self) -> Vec<(&str, &str)> {
        self.stdout
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let rating = fields.next().filter(|r| r.starts_with('['))?;
                Some((rating, fields.next()?))
            })
            .collect()
    }
}

fn stock_config() -> String {
    read_script("omnect_health_checks.json")
}

#[test]
fn an_empty_configuration_is_not_rated_green() {
    let sandbox = Sandbox::new("[]");

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_YELLOW,
        "a device that was never checked must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        report.stderr.contains("no check was run"),
        "the empty configuration must still be reported:\n{}{}",
        report.stdout,
        report.stderr
    );
    assert_eq!(
        report.ratings(),
        [("[YELLOW]", "omnect_health_checks.json")],
        "a run that checked nothing must say so where the rating is \
         read:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_config_file_without_a_document_is_rated_red() {
    let sandbox = Sandbox::new("");

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a config file carrying no document must not pass as an empty list \
         of checks:\n{}{}",
        report.stdout, report.stderr
    );
    assert_eq!(
        report.ratings(),
        [("[RED]", "omnect_health_checks.json")],
        "the config file that rated this run must be named where the rating \
         is read:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_missing_config_file_is_rated_where_the_rating_is_read() {
    let sandbox = Sandbox::new("[]");
    fs::remove_file(sandbox.etc().join("omnect_health_checks.json")).expect("remove check config");

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a device without a check configuration must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    assert_eq!(
        report.ratings(),
        [("[RED]", "omnect_health_checks.json")],
        "the missing config file must be named where the rating is read:\n{}{}",
        report.stdout,
        report.stderr
    );
    assert!(
        report.stdout.contains("OVERALL:"),
        "a run that rates this device must end in an overall rating:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn an_unknown_check_name_is_rated_yellow_and_named() {
    let sandbox = Sandbox::new(&stock_config());

    let report = sandbox.run_script("omnect_health_check.sh", &["check", "aziot"]);

    assert_eq!(
        report.rating, RATING_YELLOW,
        "a run in which nothing was checked leaves the health of this device \
         unknown:\n{}{}",
        report.stdout, report.stderr
    );
    assert_eq!(
        report.ratings(),
        [("[YELLOW]", "omnect_health_checks.json")],
        "a run that checked nothing must say so where the rating is \
         read:\n{}{}",
        report.stdout,
        report.stderr
    );
    assert!(
        report.stdout.contains("OVERALL:"),
        "a run that rates this device must end in an overall rating:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_check_that_cannot_be_run_is_named_by_its_entry() {
    let sandbox = Sandbox::new(
        r#"[ { "name": "first", "type": "absent" },
             { "name": "second", "type": "absent" } ]"#,
    );

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "first"), ("[RED]", "second")],
        "every entry whose check cannot be run must be told apart by its \
         name:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_check_failing_beyond_the_known_ratings_is_rated_red() {
    let sandbox = Sandbox::new(r#"[ { "name": "confused", "type": "confused" } ]"#);
    sandbox.install_check("confused", "echo 'something happened'\nexit 9\n");

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "an exit status outside the known ratings must not pass as a \
         rating:\n{}{}",
        report.stdout, report.stderr
    );
    assert_eq!(
        report.ratings(),
        [("[RED]", "confused")],
        "the check that left an unusable exit status must be named where the \
         rating is read:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn an_entry_without_a_type_is_rated_red() {
    let sandbox = Sandbox::new(r#"[ { "name": "nameless" } ]"#);

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "an unusable configuration entry must be rated red:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        !report.stderr.contains("no check was run"),
        "a rated entry must not count as nothing having been checked:\n{}{}",
        report.stdout,
        report.stderr
    );
    assert_eq!(
        report.ratings(),
        [("[RED]", "nameless")],
        "the entry that rated this run must be named where the rating is \
         read:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_check_that_says_nothing_is_still_named_with_its_rating() {
    let sandbox = Sandbox::new(r#"[ { "name": "silent", "type": "silent" } ]"#);
    sandbox.install_check("silent", "exit 2\n");

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "silent")],
        "a failing check that printed nothing must still be named with its \
         rating:\n{}{}",
        report.stdout,
        report.stderr
    );
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
    sandbox.stub("journalctl", &format!("echo '{CORE_DUMP_ENTRY}'\n"));

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

/// A reporter can hand systemd-coredump a backtrace with `--backtrace`, and
/// that entry carries its own message id.
#[test]
fn a_backtrace_entry_is_rated_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub(
        "journalctl",
        &format!(
            "for arg in \"$@\"; do\n\
             \x20   case \"$arg\" in\n\
             \x20       MESSAGE_ID=1f4e0a44a88649939aaea34fc6da8c95)\n\
             \x20           echo '{CORE_DUMP_ENTRY}'\n\
             \x20           ;;\n\
             \x20   esac\n\
             done\n"
        ),
    );

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a backtrace of a crashed process must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn a_core_dump_lookup_that_failed_is_not_green() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub("journalctl", "echo 'Failed to open journal.' >&2\nexit 1\n");

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a device nobody could look for core dumps on must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        report.stderr.contains("Failed to open journal."),
        "check must say why the core dumps could not be looked up:\n{}{}",
        report.stdout,
        report.stderr
    );
}

/// Devices without an RTC start the wall clock from the timesyncd clock file,
/// so a crash in early boot carries a timestamp from before the device was
/// switched off. Selecting by `now - uptime` leaves such a crash out, which
/// is exactly the crash loop the check is there for.
#[test]
fn a_crash_from_early_boot_is_rated_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub(
        "journalctl",
        &format!(
            "for arg in \"$@\"; do\n\
             \x20   case \"$arg\" in\n\
             \x20       --since) exit 0 ;;\n\
             \x20       -b) echo '{CORE_DUMP_ENTRY}'; exit 0 ;;\n\
             \x20   esac\n\
             done\n\
             exit 0\n"
        ),
    );

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a crash whose timestamp predates the computed boot time must not be \
         rated green:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn a_system_without_core_dumps_is_green() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    // journalctl succeeds when no entry matched, so the default stub already
    // behaves like the tool does on a device without core dumps

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_GREEN,
        "a device without core dumps must be rated green:\n{}{}",
        report.stdout, report.stderr
    );
}

// the message systemd emits for a journal file it had to skip; the remaining
// files are searched normally, so journalctl still succeeds
const SKIPPED_JOURNAL: &str =
    "Journal file /var/log/journal/0/user-1000@x.journal corrupted, ignoring file.";

// the message systemd-coredump logs for a core dump
const CORE_DUMP_ENTRY: &str = "Process 1234 (crasher) of user 0 dumped core.";

// systemd follows this skip with a second line that carries no wording of its
// own about the file being left out
const UNSUPPORTED_JOURNAL: &str = "Journal file /var/log/journal/0/system@x.journal uses an \
                                   unsupported feature, ignoring file.\nUse \
                                   SYSTEMD_LOG_LEVEL=debug journalctl \
                                   --file=/var/log/journal/0/system@x.journal to see the details.";

#[test]
fn a_skipped_journal_file_is_rated_by_the_exit_status_not_the_wording() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub(
        "journalctl",
        &format!("cat >&2 <<'EOT'\n{UNSUPPORTED_JOURNAL}\nEOT\n"),
    );

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_YELLOW,
        "a skipped journal file is a blind spot whatever systemd calls \
         it:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn a_skipped_journal_file_is_not_rated_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub("journalctl", &format!("echo '{SKIPPED_JOURNAL}' >&2\n"));

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_YELLOW,
        "a damaged journal file leaves a blind spot, so the device is of \
         unknown health:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn a_skipped_journal_file_does_not_hide_a_core_dump() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub(
        "journalctl",
        &format!("echo '{SKIPPED_JOURNAL}' >&2\necho '{CORE_DUMP_ENTRY}'\n"),
    );

    let report = sandbox.run_script("omnect_health_check.sh", &["get-infos"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a core dump found beside a damaged journal file is still a core dump:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        report.stdout.contains(CORE_DUMP_ENTRY),
        "get-infos must report the core dump it found:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_skipped_journal_file_is_still_reported() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub("journalctl", &format!("echo '{SKIPPED_JOURNAL}' >&2\n"));

    let report = sandbox.run_script("omnect_health_check.sh", &["get-infos"]);

    assert!(
        report.stdout.contains("ignoring file."),
        "get-infos must say which part of the journal could not be searched:\n{}{}",
        report.stdout,
        report.stderr
    );
}

// journalctl(1) on -q: "Suppresses all informational messages ... any warning
// messages regarding inaccessible system journals when run as a normal user."
const INACCESSIBLE_JOURNAL: &str =
    "Journal file /var/log/journal/0/system@x.journal not accessible: Permission denied.";

#[test]
fn a_journal_the_caller_may_not_read_is_not_rated_green() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub(
        "journalctl",
        &format!(
            "for arg; do [ \"$arg\" = -q ] && exit 0; done\n\
             echo '{INACCESSIBLE_JOURNAL}' >&2\n"
        ),
    );

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_YELLOW,
        "a journal the check may not read holds unknown core dumps:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn get_infos_reports_a_core_dump_lookup_that_failed() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub("journalctl", "echo 'Failed to open journal.' >&2\nexit 1\n");

    let report = sandbox.run_script("omnect_health_check.sh", &["get-infos"]);

    assert_eq!(
        report.rating, RATING_RED,
        "get-infos must not rate a failed core dump lookup green:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        report.stdout.contains("Failed to open journal."),
        "get-infos must show why the core dumps could not be looked up:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_check_script_finds_its_library_when_invoked_by_bare_name() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);

    for script in CHECK_SCRIPTS {
        let report = sandbox.run_script_by_bare_name(script, &["check"]);

        assert!(
            !report.stderr.contains("healthchecklib.sh"),
            "{script} must locate its library when called by bare name:\n{}{}",
            report.stdout,
            report.stderr
        );
        assert!(
            !report.stdout.is_empty(),
            "{script} must still report a rating when called by bare name:\n{}{}",
            report.stdout,
            report.stderr
        );
    }
}

#[test]
fn a_service_specific_rating_overrides_the_default_rating() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(PER_SERVICE_RATINGS);
    sandbox.write_exit_log("lenient", "HUP");
    sandbox.write_exit_log("strict", "HUP");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[GREEN]", "lenient"), ("[RED]", "strict")],
        "each service must be rated by its own ratings, not by the defaults:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn an_entry_no_service_rule_matches_is_rated_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(UNMATCHED_SERVICE_RATING);
    sandbox.write_exit_log("picky", "SEGV");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "picky")],
        "an entry that no service rule rates must not pass as green:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_configuration_jq_cannot_read_completely_is_rated_red() {
    let sandbox = Sandbox::new(r#"[ { "type": "system_running" }, 42, { "type": "coredumps" } ]"#);

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a configuration that cannot be read completely must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    assert_eq!(
        report.ratings(),
        [("[RED]", "omnect_health_checks.json")],
        "the configuration that rated this run must be named where the \
         rating is read:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_line_broken_extra_args_value_stays_one_check() {
    let sandbox = Sandbox::new(NEWLINE_EXTRA_ARGS_CONFIG);
    sandbox.set_service_analysis(TWO_SERVICES);

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[GREEN]", "svc-a"), ("[GREEN]", "svc-b")],
        "both names must reach the services check as arguments:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_changed_policy_re_rates_an_already_rated_entry() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(SEGV_RATED_GREEN);
    sandbox.write_exit_log("picky", "SEGV");

    let first = sandbox.run_script("omnect_health__services.sh", &["check"]);
    assert_eq!(
        first.ratings(),
        [("[GREEN]", "picky")],
        "the lenient policy has to rate the entry green first:\n{}{}",
        first.stdout,
        first.stderr
    );

    sandbox.set_service_analysis(UNMATCHED_SERVICE_RATING);
    let second = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        second.ratings(),
        [("[RED]", "picky")],
        "a rating the current rules no longer justify must not survive:\n{}{}",
        second.stdout,
        second.stderr
    );
}

#[test]
fn a_rating_condition_is_not_run_by_the_shell() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    let marker = sandbox.run().join("expanded");
    sandbox.set_service_analysis(&format!(
        r#"{{
    "default-ratings": [
        {{ "condition": ".exitstatus == \"$(touch {})SEGV\"", "rating": "green" }}
    ],
    "services": [ {{ "service": "picky" }} ]
}}"#,
        marker.display()
    ));
    sandbox.write_exit_log("picky", "SEGV");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert!(
        !marker.exists(),
        "a condition from the configuration file must not be run as a command:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn the_rating_never_replaces_the_exit_log() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(PER_SERVICE_RATINGS);
    sandbox.write_exit_log("strict", "HUP");
    let log = sandbox.run().join("strict.exit-log");
    let recorded = fs::read_to_string(&log).expect("read exit log");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        fs::read_to_string(&log).expect("read exit log"),
        recorded,
        "the only record of what happened to a service must not be rewritten:\n{}{}",
        report.stdout,
        report.stderr
    );
    let mut left_behind: Vec<String> = fs::read_dir(sandbox.run())
        .expect("read log directory")
        .map(|entry| {
            entry
                .expect("read log entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    left_behind.sort();
    assert_eq!(
        left_behind,
        ["strict.exit-log"],
        "rating writes nothing down, so it must leave nothing behind:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn an_entry_added_after_a_rating_is_rated_too() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    let log = sandbox.run().join("picky.exit-log");
    fs::write(
        &log,
        r#"{ "service": "picky", "infos": [ { "exitstatus": "clean" } ] }"#,
    )
    .expect("write exit log");
    let rated = sandbox.run_script("omnect_health__services.sh", &["check"]);
    assert_eq!(
        rated.ratings(),
        [("[GREEN]", "picky")],
        "the entry this test appends to has to be rated green, or the rating \
         below cannot tell a re-rating from the rating it replaces:\n{}{}",
        rated.stdout,
        rated.stderr
    );

    fs::write(
        &log,
        r#"{ "service": "picky", "infos": [ { "exitstatus": "clean" }, { "exitstatus": "SEGV" } ] }"#,
    )
    .expect("append to exit log");
    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "picky")],
        "an entry recorded after the last rating must not stay unrated:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn get_infos_rates_an_unratable_exit_log_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(UNMATCHED_SERVICE_RATING);
    fs::write(
        sandbox.run().join("picky.exit-log"),
        r#"{ "service": "picky", "infos": [ { "exitstatus": "#,
    )
    .expect("write corrupt exit log");

    let report = sandbox.run_script("omnect_health__services.sh", &["get-infos"]);

    assert_eq!(
        report.rating, RATING_RED,
        "an exit log that cannot be read must not be reported as green:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn get_infos_applies_the_current_rating_rules() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(SEGV_RATED_GREEN);
    sandbox.write_exit_log("picky", "SEGV");

    let first = sandbox.run_script("omnect_health__services.sh", &["check"]);
    assert_eq!(
        first.rating, RATING_GREEN,
        "the lenient policy has to rate the entry green first:\n{}{}",
        first.stdout, first.stderr
    );

    sandbox.set_service_analysis(UNMATCHED_SERVICE_RATING);
    let report = sandbox.run_script("omnect_health__services.sh", &["get-infos"]);

    assert_eq!(
        report.rating, RATING_RED,
        "get-infos has to rate by the current rules, just like check:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn a_service_config_that_cannot_be_read_is_rated_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(r#"{ "default-ratings": ["#);
    sandbox.write_exit_log("picky", "SEGV");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a service configuration that cannot be read must not pass as green:\n{}{}",
        report.stdout, report.stderr
    );
}

/// An entry without a `service` key used to make `jq` print the literal
/// string `null`, which then passed as a service of that name with nothing
/// to rate.
#[test]
fn a_service_entry_without_a_name_is_rated_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(
        r#"{
    "default-ratings": [
        { "condition": ".exitstatus == \"clean\"", "rating": "green" }
    ],
    "services": [ { "service": "picky" }, { "rules": [] } ]
}"#,
    );
    sandbox.write_exit_log("picky", "clean");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a service entry without a name must not pass as green:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn a_requested_service_without_rules_is_rated_yellow() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(UNMATCHED_SERVICE_RATING);

    let report = sandbox.run_script("omnect_health__services.sh", &["check", "typo-service"]);

    assert_eq!(
        report.ratings(),
        [("[YELLOW]", "typo-service")],
        "a service that was asked for but cannot be assessed must be reported:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_check_name_is_not_rejected_by_an_unreadable_config() {
    let sandbox = Sandbox::new(r#"[ { "type": "timesync" }, 42 ]"#);

    let report = sandbox.run_script("omnect_health_check.sh", &["check", "timesync"]);

    assert_eq!(
        report.rating, RATING_RED,
        "naming a check must not turn an unreadable config into an unknown name:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        report.stdout.contains("OVERALL:"),
        "a check run must report its overall rating even when it could not \
         be made:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn an_unselected_broken_entry_does_not_rate_the_run() {
    let sandbox = Sandbox::new(
        r#"[ { "name": "coredumps", "type": "coredumps" },
    { "name": "broken", "type": "does_not_exist" },
    { "name": "nameless" } ]"#,
    );

    let report = sandbox.run_script("omnect_health_check.sh", &["check", "coredumps"]);

    assert_eq!(
        report.ratings(),
        [("[GREEN]", "coredumps")],
        "an entry that was not selected must not rate the run:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn an_unratable_exit_log_is_rated_red_and_kept() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(UNMATCHED_SERVICE_RATING);
    let log = sandbox.run().join("picky.exit-log");
    let corrupt = r#"{ "service": "picky", "infos": [ { "exitstatus": "#;
    fs::write(&log, corrupt).expect("write corrupt exit log");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "picky")],
        "an exit log that cannot be rated must not pass as green:\n{}{}",
        report.stdout,
        report.stderr
    );
    assert_eq!(
        fs::read_to_string(&log).expect("read exit log"),
        corrupt,
        "a failed rating must leave the exit log untouched"
    );
}

#[test]
fn an_empty_exit_log_is_not_rated_green() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    // a log can be empty for reasons of its own - omnect_service_log.sh
    // assembles every version under a temporary name, so it never leaves one
    // behind itself - and an empty log says nothing about the service
    fs::write(sandbox.run().join("picky.exit-log"), "").expect("write empty exit log");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "picky")],
        "a log that yielded no rating at all must not pass as a healthy \
         one:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_service_is_rated_by_its_own_rules_after_a_reordering() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(PER_SERVICE_RATINGS);
    sandbox.write_exit_log("strict", "HUP");

    // an update reorders the services right after their names were read, so
    // the position a name had is no longer the position its rules are at
    let policy = sandbox.etc().join("omnect_service_log_analysis.json");
    let swapped = sandbox.etc().join("swapped.json");
    fs::write(&swapped, PER_SERVICE_RATINGS_SWAPPED).expect("write swapped policy");
    sandbox.stub(
        "jq",
        &format!(
            r#"case "$*" in
  *".services[].service"*)
      "{jq}" "$@"
      status=$?
      if [ ! -e "{marker}" ]; then
          touch "{marker}"
          cp "{swapped}" "{policy}"
      fi
      exit $status
      ;;
esac
exec "{jq}" "$@"
"#,
            jq = real_jq().display(),
            marker = sandbox.root().join("reordered").display(),
            swapped = swapped.display(),
            policy = policy.display(),
        ),
    );

    let report = sandbox.run_script("omnect_health__services.sh", &["check", "strict"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "strict")],
        "a service has to be rated by the rules carrying its name, whatever \
         place they are in:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_check_name_of_several_words_can_be_selected() {
    let sandbox = Sandbox::new(MULTIWORD_NAME_CONFIG);

    let report = sandbox.run_script("omnect_health_check.sh", &["check", "disk space"]);

    assert_eq!(
        report.rated_names(),
        ["coredumps"],
        "a check name holding a blank must select its entry and nothing else:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn the_check_configuration_is_read_in_a_single_pass() {
    let sandbox = Sandbox::new(ELEVEN_CHECKS_CONFIG);
    let counter = sandbox.count_jq_calls();

    let report = sandbox.run_script("omnect_health_check.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_GREEN,
        "an undisturbed system must still be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    let calls = jq_calls(&counter);
    assert_eq!(
        calls, CONFIG_READS,
        "reading 11 check entries must not start {calls} jq processes:\n{}{}",
        report.stdout, report.stderr
    );
}

#[test]
fn naming_a_check_does_not_read_the_configuration_again() {
    let sandbox = Sandbox::new(ELEVEN_CHECKS_CONFIG);
    let counter = sandbox.count_jq_calls();

    let report = sandbox.run_script("omnect_health_check.sh", &["check", "cores1"]);

    assert_eq!(
        report.ratings(),
        [("[GREEN]", "coredumps")],
        "only the named check must run:\n{}{}",
        report.stdout,
        report.stderr
    );
    let calls = jq_calls(&counter);
    assert_eq!(
        calls, CONFIG_READS,
        "a name has to be looked up in the entries already read, but the \
         configuration was read {calls} times:\n{}{}",
        report.stdout, report.stderr
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

/// Both commands are rated, so both have to say green on a healthy device.
/// The stock configuration runs every check, which is where a check that
/// rates itself green but exits non-zero shows up.
#[test]
fn both_commands_rate_an_undisturbed_system_green() {
    for command in ["check", "get-infos"] {
        let sandbox = Sandbox::new(&stock_config());

        let report = sandbox.run_script("omnect_health_check.sh", &[command]);

        assert_eq!(
            report.rating, RATING_GREEN,
            "\"{command}\" must rate a healthy device green:\n{}{}",
            report.stdout, report.stderr
        );
    }
}

/// Every check script carries its own command dispatch, so each one has to
/// return the rating it printed, for both commands.
#[test]
fn every_check_rates_an_undisturbed_system_green_on_its_own() {
    for script in CHECK_SCRIPTS {
        for command in ["check", "get-infos"] {
            let sandbox = Sandbox::new(COREDUMPS_CONFIG);

            let report = sandbox.run_script(script, &[command]);

            assert_eq!(
                report.rating, RATING_GREEN,
                "{script} \"{command}\" must rate a healthy device \
                 green:\n{}{}",
                report.stdout, report.stderr
            );
        }
    }
}

#[test]
fn get_infos_rates_a_core_dump_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.stub("journalctl", &format!("echo '{CORE_DUMP_ENTRY}'\n"));

    let report = sandbox.run_script("omnect_health_check.sh", &["get-infos"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a core dump must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        report.stdout.contains(CORE_DUMP_ENTRY),
        "get-infos must report the core dump it found:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_rating_the_rules_misname_is_not_green() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(MISNAMED_RATINGS);
    sandbox.write_exit_log("typo", "SEGV");
    sandbox.write_exit_log("nameless", "SEGV");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "typo"), ("[RED]", "nameless")],
        "a rule that names no rating this script knows must not pass the \
         entry it matched as healthy:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn get_infos_shows_the_exit_info_of_a_rated_service() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    sandbox.write_exit_log("picky", "SEGV");
    fs::write(
        sandbox.run().join("picky.exit-info"),
        "picky died of SEGV\n",
    )
    .expect("write exit info");

    let report = sandbox.run_script("omnect_health__services.sh", &["get-infos"]);

    assert!(
        report.stdout.contains("picky died of SEGV"),
        "get-infos has to show the exit info beside the rating it belongs \
         to:\n{}{}",
        report.stdout,
        report.stderr
    );
}

#[test]
fn a_service_config_that_cannot_be_read_names_the_services_it_fails() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(r#"{ "default-ratings": ["#);

    let report = sandbox.run_script("omnect_health__services.sh", &["check", "picky", "choosy"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "picky"), ("[RED]", "choosy")],
        "a run that rates red has to say which services it is talking \
         about:\n{}{}",
        report.stdout,
        report.stderr
    );

    let infos = sandbox.run_script("omnect_health__services.sh", &["get-infos", "picky"]);

    assert!(
        infos.stdout.contains("(picky):RED"),
        "get-infos has to report the failure just as check does:\n{}{}",
        infos.stdout,
        infos.stderr
    );
}
/// Both the checks config and this rules file ship in the image, so a missing
/// one is a broken image and is rated like an unreadable one, as the
/// dispatcher already does.
#[test]
fn a_missing_service_config_is_rated_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    fs::remove_file(sandbox.etc().join("omnect_service_log_analysis.json"))
        .expect("remove service log config");

    let report = sandbox.run_script("omnect_health__services.sh", &["check", "picky"]);

    assert_eq!(
        report.rating, RATING_RED,
        "a missing rules file is a broken image and must be rated as one:\n{}{}",
        report.stdout, report.stderr
    );
    assert_eq!(
        report.ratings(),
        [("[RED]", "picky")],
        "the service that could not be rated has to be named:\n{}{}",
        report.stdout,
        report.stderr
    );
}

/// A rules file naming no service at all leaves the check with nothing to
/// rate, which says nothing about this device and must not pass as health.
#[test]
fn a_check_that_rated_no_service_at_all_is_rated_yellow() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(r#"{ "services": [], "default-ratings": [] }"#);

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.rating, RATING_YELLOW,
        "a check that rated no service must not be rated green:\n{}{}",
        report.stdout, report.stderr
    );
    assert_eq!(
        report.ratings(),
        [("[YELLOW]", "omnect_health__services.sh")],
        "a check that rated nothing must say so where the rating is \
         read:\n{}{}",
        report.stdout,
        report.stderr
    );
}

/// An exit log that exists but cannot be read says nothing about the service
/// it belongs to, and skipping it silently rates that service green.
#[test]
fn an_unreadable_exit_log_is_rated_red() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    block_path(&sandbox.run().join("picky.exit-log"));

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "picky")],
        "an exit log that cannot be read must not leave its service \
         green:\n{}{}",
        report.stdout,
        report.stderr
    );
}

/// The service check reads the exit logs while services are being stopped, so
/// a log must not turn up under its final name before it holds an entry.
#[test]
fn an_exit_log_turns_up_only_once_its_first_entry_is_in_it() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    sandbox.install_service_log();
    let observed = sandbox.root().join("seen-while-writing");
    sandbox.stub_jq_watching_the_exit_log("picky", &observed);

    sandbox.run_script(
        "omnect_service_log.sh",
        &["stop", "picky", "exit-code", "exited", "SEGV"],
    );

    let seen = fs::read_to_string(&observed).expect("the first entry was never written");
    assert_eq!(
        seen.trim(),
        "absent",
        "a check reading the exit log while it was created would have found it {}",
        seen.trim()
    );
    assert!(
        sandbox.run().join("picky.exit-log").exists(),
        "the exit log never turned up at all"
    );
}

/// The header holds the exit status of the service, so a journal that cannot
/// be read must not take it down with it.
#[test]
fn an_exit_info_keeps_its_header_when_the_journal_cannot_be_read() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    sandbox.install_service_log();
    sandbox.stub("journalctl", "echo 'Failed to open journal.' >&2\nexit 1\n");

    sandbox.run_script(
        "omnect_service_log.sh",
        &["stop", "picky", "exit-code", "exited", "SEGV"],
    );

    let info = fs::read_to_string(sandbox.run().join("picky.exit-info"))
        .expect("the exit info of a service whose journal could not be read");
    assert!(
        info.contains("Exitstatus:  SEGV"),
        "the exit info must keep the exit status of the service:\n{info}"
    );
    assert!(
        info.contains("journal"),
        "the exit info must say that the journal is missing from it:\n{info}"
    );
}

/// The exit info is the only record of what the journal held when a service
/// died, so one that could not be written must not be installed, let alone
/// kept as the first failure of that service.
#[test]
fn an_exit_info_that_could_not_be_written_is_not_installed() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    sandbox.install_service_log();
    block_path(&sandbox.run().join("picky.exit-info.tmp"));

    let report = sandbox.run_script(
        "omnect_service_log.sh",
        &["stop", "picky", "exit-code", "exited", "SEGV"],
    );

    assert_ne!(
        report.rating, 0,
        "an exit info that could not be written was reported as \
         written:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        !sandbox.run().join("picky.exit-info").exists(),
        "an exit info that could not be written was installed"
    );
    assert!(
        !sandbox.run().join("picky.1st.exit-info").exists(),
        "an exit info that could not be written was kept as the first \
         failure of this service"
    );
}

/// The hook says in its exit status whether the entry was written. No unit
/// acts on that today, because they all run the hook with "-", so this
/// covers the status itself and not what systemd makes of it.
#[test]
fn a_stop_that_could_not_be_logged_is_reported() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    sandbox.install_service_log();
    block_path(&sandbox.run().join("picky.exit-log.tmp"));

    let report = sandbox.run_script(
        "omnect_service_log.sh",
        &["stop", "picky", "exit-code", "exited", "SEGV"],
    );

    assert_ne!(
        report.rating, 0,
        "a stop whose exit log could not be written was reported as \
         logged:\n{}{}",
        report.stdout, report.stderr
    );
}

/// The first start of a service is kept as the record of when it last came
/// up for the first time, so it must not be taken from an entry that was
/// never written. The exit status carries the same caveat as the stop case.
#[test]
fn a_start_that_could_not_be_logged_is_reported() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    sandbox.install_service_log();
    block_path(&sandbox.run().join("picky.last-start.tmp"));

    let report = sandbox.run_script("omnect_service_log.sh", &["start", "picky"]);

    assert_ne!(
        report.rating, 0,
        "a start that could not be logged was reported as logged:\n{}{}",
        report.stdout, report.stderr
    );
    assert!(
        !sandbox.run().join("picky.1st-start").exists(),
        "a start that was never logged was kept as the first start"
    );
}

/// Stopping a service must not rate it: nothing reads the result, and the
/// rating is worked out afresh whenever a check actually asks for one.
#[test]
fn the_stop_hook_does_not_rate_the_service_it_logs() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    sandbox.install_service_log();
    let marker = sandbox.root().join("the-service-check-ran");
    sandbox.install_check("services", &format!("touch \"{}\"\n", marker.display()));

    sandbox.run_script(
        "omnect_service_log.sh",
        &["stop", "picky", "exit-code", "exited", "SEGV"],
    );

    assert!(
        !marker.exists(),
        "stopping a service ran a check whose rating is then thrown away"
    );
}
