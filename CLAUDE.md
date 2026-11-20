# omnect-device-service

Systemd service on omnect OS devices. It connects Azure IoT Hub (device twin,
direct methods, D2C messages) with local device features (factory reset,
firmware update, SSH tunnel, network status, reboot, Wi-Fi commissioning,
modem info, consent). A local web service on a Unix socket publishes device
state to edge modules.

## Structure

- `src/twin/mod.rs`: event loop; creates the IoT Hub client, initializes the
  features and dispatches commands.
- `src/twin/feature/`: `Feature` trait, `Command` enum and parsing
  (`command.rs`), inotify file watching (`fs_watcher.rs`).
- `src/twin/*.rs`: one module per feature.
- `src/web_service.rs`: Unix socket API and publish endpoints.
- `src/systemd/`, `src/bootloader_env/`: systemd over D-Bus, bootloader
  variables (grub or u-boot by feature flag).
- `sudo/`, `polkit/`: the service runs unprivileged; these grant the few
  privileged operations.

## Constraints

- Exactly one of the features `bootloader_grub`, `bootloader_uboot` or `mock`
  is active. `Cargo.toml` has a self dev-dependency with `mock`, so a build
  with test targets always pulls in `mock`: pair `--all-targets` with
  `mock`, or `src/build.rs` rejects the build.
- A new feature: implement `Feature` in its own `src/twin/` module, add a
  `Command` variant, register it in `Twin::new()`. Commands from direct
  methods, desired properties, file events and intervals are routed to the
  owning feature by `TypeId`.
- Features publish state with `web_service::publish(PublishChannel, value)`.
- Unit tests live in an inline `#[cfg(test)] mod tests`. `src/twin/mod.rs`
  keeps them in `mod_test.rs`, where `Twin` uses the `mockall` mock
  `MockMyIotHub`. `tests/` is only for code without a Rust module, such as
  the scripts in `sudo/`.
- Errors use `anyhow`; the service is an application, not a library.
- `modem_info` is an opt-in feature.

## Commands

- Build: `cargo build --features bootloader_grub`
- Test: `cargo test --features mock`
- Lint: `cargo clippy --features bootloader_grub -- -D warnings` and
  `cargo clippy --features mock --all-targets -- -D warnings` (only the
  second covers test code)

## Breaking UDS API changes

Edge modules use the Unix socket API of this service. Before `gh pr create`
or a PR title change, check the PR diff for a breaking API change as defined
in [README.md, "Breaking changes"](README.md#breaking-changes). If it is
breaking:

- put `!` in the PR title (e.g. `feat!: ...`); the release notes take it
  from there,
- bump at least the minor version while on 0.x,
- describe the old and new format in the PR description.

The hook `.claude/hooks/uds-api-guard.sh` blocks the PR command until this
is decided. If the change is not breaking, prefix the command with
`UDS_API_BREAKING=no` and tell the user why.
