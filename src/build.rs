use std::process::Command;

fn main() {
    let git_short_rev = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let git_short_rev = git_short_rev.trim();

    println!("cargo:rustc-env=GIT_SHORT_REV={git_short_rev}");
}
