<!--
  PROJECT CONTEXT — checked into git, shared across the team.

  PURPOSE: Describe what is UNIQUE to this repository. The AI agent already
  receives global rules for omnect product context, coding standards, and
  git/QA workflow via the template system. Do NOT repeat those here.

  WHAT BELONGS HERE:
    - Repo-specific architecture, entry points, and key file locations
    - Local development scripts and commands
    - Constraints or conventions specific to this repo (e.g., module layout)
    - Overrides of global rules — if this repo deviates from a global standard,
      state it here explicitly so the agent applies the correct rule.

  WHAT DOES NOT BELONG HERE:
    - omnect product context, Yocto/Azure/IoT Hub background
    - General coding standards (naming, error handling, formatting)
    - Git commit format or QA workflow rules
-->

# Project Context

## 1. Role & Responsibility

- **Role:** Long-running systemd service on omnect OS devices that bridges Azure IoT Hub (device twin, direct methods, D2C messages) with local device capabilities (factory reset, firmware update, SSH tunneling, network status, reboot, Wi-Fi commissioning, modem info, consent management). Also exposes a local HTTP web service for publishing device state to other on-device consumers.
- **Runtime Target:** omnect OS device (ARM/x86 Linux with systemd)

## 2. Architecture & Tech Stack

- **Language / Runtime:** Rust 1.93.0 (edition 2024), async via Tokio
- **Key Frameworks:**
  - `azure-iot-sdk` (omnect fork) — IoT Hub module client (twin, direct methods, messages)
  - `actix-web` — local HTTP web service for publish/subscribe of device state
  - `zbus` / `systemd-zbus` — D-Bus communication with systemd and networkd
  - `dynosaur` — trait-object boxing for the `Feature` trait (enables `HashMap<TypeId, Box<DynFeature>>`)
- **Notable Dependencies:**
  - `inotify` — centralized file-system watching via `FsWatcher` (triggers commands on file changes)
  - `reqwest` + `reqwest-retry` — HTTP client with retry middleware (used for publishing to endpoints)
  - `modemmanager` (optional, behind `modem_info` feature) — modem status via D-Bus
  - `sd-notify` — systemd readiness notification
  - `sysinfo` — disk, component, and system information

## 3. Key Entry Points & Files

- `src/main.rs` — binary entry point; sets up logging, delegates to `Twin::run()`
- `src/twin/mod.rs` — core event loop: creates IoT Hub client, initializes all features, dispatches commands from direct methods / desired properties / file watches / intervals
- `src/twin/feature/mod.rs` — `Feature` trait definition and `DynFeature` (re-exports from `command.rs` and `fs_watcher.rs`)
- `src/twin/feature/command.rs` — `Command` enum, `CommandRequest` types, `parse_payload` helper, `interval_stream`
- `src/twin/feature/fs_watcher.rs` — centralized `FsWatcher` (inotify-based, per-watch debounce, oneshot support)
- `src/web_service.rs` — actix-web HTTP server exposing publish channels (`/publish/v1/{channel}`) and status endpoints; manages publish endpoints and retry logic
- `src/twin/*.rs` — one module per feature: `consent`, `factory_reset`, `firmware_update/`, `modem_info`, `network`, `provisioning_config`, `reboot`, `ssh_tunnel`, `system_info`, `wifi_commissioning`
- `src/systemd/` — systemd integration: `unit.rs` (start/stop/restart units via D-Bus), `networkd.rs` (network link status), `watchdog.rs` (systemd watchdog keep-alive)
- `src/bootloader_env/` — bootloader variable get/set, dispatched by feature flag to `grub.rs` or `uboot.rs`
- `src/build.rs` — build script (compile-time metadata)
- `healthcheck/` — shell scripts for device health checks (coredumps, services, timesync, reboot reason)
- `systemd/` — unit files: `omnect-device-service.service`, `.socket`, `.timer`, plus update-validation-observer
- `sudo/` — sudoers drop-in files granting the service permission for specific privileged commands
- `polkit/` — polkit rules for D-Bus permissions
- `testfiles/` — test fixtures (positive/negative cases, Wi-Fi commissioning configs)

## 4. Repository-Specific Constraints

- **Mutually exclusive feature flags:** Exactly one of `bootloader_grub`, `bootloader_uboot`, or `mock` must be active. The `mock` feature stubs out hardware/systemd interactions for testing.
- **Feature trait pattern:** Every device capability implements the `Feature` trait (in its own `src/twin/*.rs` module). Adding a new feature means: (1) implement `Feature`, (2) add a `Command` variant, (3) register in `Twin::new()` feature map.
- **Command dispatch:** All operations flow through the `Command` enum and parsing helpers in `src/twin/feature/command.rs`, with file-watch handling in `fs_watcher.rs`. Direct methods, desired properties, file-system events, and intervals all produce `Command` values that get routed to the owning feature via `TypeId`.
- **Web service publish pattern:** Features publish state via `web_service::publish(PublishChannel, value)`. External consumers register endpoints in `/run/omnect-device-service/publish_endpoints.json`.
- **`#[cfg(test)]` IoT Hub mock:** In test builds, `Twin` uses `MockMyIotHub` (generated via `mockall`) instead of the real `IotHubClient`. See `src/twin/mod_test.rs`.
- **Privileged operations:** The service runs unprivileged but uses sudoers rules (`sudo/`) and polkit (`polkit/`) for specific operations (grub-editenv, fw_setenv, journalctl, reboot).
- **`modem_info` feature:** Opt-in via the `modem_info` Cargo feature (pulls in the `modemmanager` crate). Not active by default.

## 5. Local Dev Scripts

- **Build (non-mock):** `cargo build --features bootloader_grub`
- **Build (mock/test):** `cargo build --features mock`
- **Run Tests:** `cargo test --features mock`
- **Lint:** `cargo clippy --features bootloader_grub -- -D warnings`
- **Format:** `cargo fmt`
- **Pre-commit check:** `cargo fmt && cargo clippy --features bootloader_grub -- -D warnings` (must pass before committing)

## 6. Global Rule Overrides

- **Error handling:** Uses `anyhow` throughout (no custom error types). This is intentional — the service is a top-level application, not a library, so ergonomic error propagation is preferred over typed errors.
