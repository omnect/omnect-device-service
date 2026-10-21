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
const RATING_YELLOW: i32 = 1;
const RATING_RED: i32 = 2;

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

    fn root(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Starts a check that reads the exit log now but writes its rating only
    /// once `release` appears, so a second rater can get in between.
    fn spawn_stalling_rater(&self, signal: &Path, release: &Path) -> std::process::Child {
        self.stub(
            "jq",
            &format!(
                r#"case "${{@: -1}}" in
  *.exit-log)
      if [ "$STALL" = 1 ]; then
          content=$(cat "${{@: -1}}")
          touch "{signal}"
          while [ ! -e "{release}" ]; do sleep 0.02; done
          printf '%s' "$content" | "{jq}" "${{@:1:$#-1}}"
          exit $?
      fi
      ;;
esac
exec "{jq}" "$@"
"#,
                signal = signal.display(),
                release = release.display(),
                jq = real_jq().display()
            ),
        );
        self.command("omnect_health__services.sh")
            .args(["check", "picky"])
            .env("STALL", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start stalling rater")
    }

    /// Waits until the stalling rater announces that it has read the exit log.
    ///
    /// A child that dies before it gets there would otherwise leave the test
    /// waiting for a signal nobody is going to give, taking the whole suite
    /// with it, so give up after a while and say what happened.
    fn await_signal(&self, signal: &Path, rater: &mut std::process::Child, release: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !signal.exists() {
            if let Some(status) = rater.try_wait().expect("query stalling rater") {
                panic!("the stalling rater gave up with {status} before it read the exit log");
            }
            if std::time::Instant::now() >= deadline {
                // let the stub out of its wait first, so a child that is only
                // slow can still finish on its own, then make sure it is gone
                let _ = fs::write(release, "");
                let _ = rater.kill();
                let _ = rater.wait();
                panic!("the stalling rater did not read the exit log in time");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
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
        report.stderr.contains("no checks defined in config file"),
        "the empty configuration must still be reported:\n{}{}",
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
    let rated = fs::read_to_string(sandbox.run().join("strict.exit-log.rated"))
        .expect("read rated exit log");
    assert!(
        rated.contains("\"rating\""),
        "the rating must be kept beside the exit log, but found:\n{rated}"
    );
}

#[test]
fn rating_keeps_the_exit_log_as_readable_as_it_was() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(PER_SERVICE_RATINGS);
    sandbox.write_exit_log("strict", "HUP");
    let log = sandbox.run().join("strict.exit-log");
    fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).expect("chmod exit log");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    for file in [&log, &sandbox.run().join("strict.exit-log.rated")] {
        let mode = fs::metadata(file).expect("stat log").permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o644,
            "rating must not lock {} away from its readers, but left mode {mode:o}:\n{}{}",
            file.display(),
            report.stdout,
            report.stderr
        );
    }
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
fn a_rating_that_was_overtaken_is_not_believed() {
    let sandbox = Sandbox::new(COREDUMPS_CONFIG);
    sandbox.set_service_analysis(CRASH_RATED_SERVICE);
    let log = sandbox.run().join("picky.exit-log");
    fs::write(
        &log,
        r#"{ "service": "picky", "infos": [ { "exitstatus": "clean" } ] }"#,
    )
    .expect("write exit log");

    let signal = sandbox.root().join("has-read");
    let release = sandbox.root().join("may-write");
    let mut stalled = sandbox.spawn_stalling_rater(&signal, &release);
    sandbox.await_signal(&signal, &mut stalled, &release);

    // the service crashes and its StopPost hook rates the crash, all while the
    // first rater still holds the picture of a healthy service
    fs::write(
        &log,
        r#"{ "service": "picky", "infos": [ { "exitstatus": "clean" }, { "exitstatus": "SEGV" } ] }"#,
    )
    .expect("record the crash");
    let crash = sandbox.run_script("omnect_health__services.sh", &["check"]);
    assert_eq!(
        crash.ratings(),
        [("[RED]", "picky")],
        "the crash must be rated when it is recorded:\n{}{}",
        crash.stdout,
        crash.stderr
    );

    // only now the first rater gets to publish what it saw before the crash
    fs::write(&release, "").expect("release the stalled rater");
    stalled.wait().expect("await stalled rater");

    let report = sandbox.run_script("omnect_health__services.sh", &["check"]);

    assert_eq!(
        report.ratings(),
        [("[RED]", "picky")],
        "a rating overtaken by a newer one must not bury the crash:\n{}{}",
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
    let calls = fs::read_to_string(&counter)
        .unwrap_or_default()
        .lines()
        .count();
    assert!(
        calls <= 2,
        "reading 11 check entries must not start {calls} jq processes:\n{}{}",
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
