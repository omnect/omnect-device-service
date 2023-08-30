use std::process::Command;

fn main() {
    #[cfg(not(any(
        feature = "bootloader_grub",
        feature = "bootloader_uboot",
        feature = "mock"
    )))]
    compile_error!(
        "Either feature 'bootloader_grub' xor 'bootloader_uboot' xor 'mock' must be enabled."
    );
    #[cfg(all(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    compile_error!(
        "Either feature 'bootloader_grub' xor 'bootloader_uboot' xor 'mock' must be enabled."
    );
    #[cfg(all(feature = "bootloader_grub", feature = "mock"))]
    compile_error!(
        "Either feature 'bootloader_grub' xor 'bootloader_uboot' xor 'mock' must be enabled."
    );
    #[cfg(all(feature = "bootloader_uboot", feature = "mock"))]
    compile_error!(
        "Either feature 'bootloader_grub' xor 'bootloader_uboot' xor 'mock' must be enabled."
    );

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
