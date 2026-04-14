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
- **Role:** Long-running systemd daemon that bridges Azure IoT Hub (device twin, direct methods, D2C messages) with on-device operations — network configuration, firmware updates, factory reset, SSH tunnels, reboot, system info, modem info, and wifi commissioning.
- **Runtime Target:** omnect OS device (runs as `omnect_device_service` user under systemd with watchdog, socket activation, and sd-notify).

## 2. Architecture & Tech Stack
- **Language / Runtime:** Rust 1.93 (edition 2024), async via Tokio.
- **Key Frameworks:**
  - `azure-iot-sdk` (omnect fork) — IoT Hub client (module client mode), device twin, direct methods, D2C messaging.
  - `actix-web` — local HTTP web service for publish/subscribe and health endpoints.
  - `zbus` / `systemd-zbus` — D-Bus communication with systemd (unit control, networkd).
  - `dynosaur` — object-safe async trait dispatch for the `Feature` trait.
- **Notable Dependencies:**
  - `notify` / `notify-debouncer-full` — filesystem event watching (used for update validation, config changes).
  - `reqwest` + `reqwest-middleware` + `reqwest-retry` — HTTP client with exponential backoff for publish endpoints.
  - `sd-notify` — systemd readiness and watchdog notification.
  - `modemmanager` (optional, behind `modem_info` feature) — modem status via D-Bus.
  - `sysinfo` — disk, system, and component metrics.

## 3. Key Entry Points & Files
- `src/main.rs` — binary entry point; sets up logging and delegates to `Twin::run()`.
- `src/lib.rs` — crate root; declares top-level modules.
- `src/twin/mod.rs` — core orchestrator: IoT Hub client lifecycle, feature dispatch loop, signal handling.
- `src/twin/feature/mod.rs` — `Feature` trait definition (via `dynosaur`), `Command` enum routing direct methods / twin updates / file events to features.
- `src/twin/{consent,factory_reset,reboot,ssh_tunnel,network,system_info,wifi_commissioning,modem_info,provisioning_config}.rs` — individual feature implementations (each implements `Feature`).
- `src/twin/firmware_update/` — firmware update feature (multi-file: ADU types, OS version parsing, update validation state machine).
- `src/web_service.rs` — actix-web HTTP server for local publish/subscribe endpoints and status.
- `src/bootloader_env/` — bootloader variable access; dispatches to `grub.rs` or `uboot.rs` based on feature flag, with an in-memory mock for tests.
- `src/systemd/` — systemd integration: `networkd.rs` (network config), `unit.rs` (service control), `watchdog.rs` (watchdog petting).
- `src/common.rs` — shared utilities (JSON file I/O, root partition helpers).
- `src/reboot_reason.rs` — persists reboot reason to filesystem.
- `src/build.rs` — build script; enforces exactly one bootloader feature flag, embeds git rev.
- `systemd/` — systemd unit files (`.service`, `.socket`, `.timer`) and helper scripts.
- `healthcheck/` — shell-based health check scripts (coredumps, timesync, services, reboot reason).
- `sudo/` — sudoers rules and `fw_setenv_no_script.sh` (safe u-boot env writes).
- `polkit/` — polkit rules for privileged operations (reboot, networkd, systemd).

## 4. Repository-Specific Constraints
- **Mutually exclusive bootloader features:** Exactly one of `bootloader_uboot`, `bootloader_grub`, or `mock` must be enabled. The build script (`src/build.rs`) enforces this at compile time via `compile_error!`.
- **`mock` feature:** Used for testing. Swaps out the IoT Hub client (`MockMyIotHub`), the bootloader env (in-memory `HashMap`), and systemd reboot. Tests compile with `--features mock`.
- **Feature modules pattern:** Each device capability (consent, factory reset, reboot, etc.) is a separate module under `src/twin/` implementing the `Feature` trait. The `Feature` trait uses `dynosaur` for object-safe async dispatch (`DynFeature`). Features are registered by `TypeId` in a `HashMap` in `Twin::new()`.
- **Command routing:** All external inputs (direct methods, twin desired properties, file system events, intervals) are converted to a `Command` enum variant in `src/twin/feature/mod.rs`, then dispatched to the owning feature via `feature_id() -> TypeId`.
- **Privileged operations:** The service runs as unprivileged user `omnect_device_service`. Privileged actions (reboot, fw_setenv, journalctl) are executed via `sudo` with allowlists in `sudo/`. Polkit rules in `polkit/` authorize D-Bus operations (networkd, systemd).
- **Environment configuration:** Runtime config is read from `/etc/omnect/omnect-device-service.env` (via `EnvironmentFile` in the systemd unit) and env vars; `dotenvy` is used for `.env` loading in dev.
- **No `config/` directory:** Configuration lives in env vars and the systemd unit file, not in a dedicated config directory.

## 5. Local Dev Scripts
- **Build (mock/test):** `cargo build --features mock`
- **Build (uboot):** `cargo build --features bootloader_uboot`
- **Build (grub):** `cargo build --features bootloader_grub`
- **Run Tests:** `cargo test --features mock`
- **Check:** `cargo check --features mock`
- **Lint:** `cargo clippy --features mock -- -D warnings`
- **Format:** `cargo fmt -- --check`
- **Audit:** `cargo audit` (ignore list in `Cargo.audit.ignore`)

## 6. Global Rule Overrides
- **`anyhow` for error handling:** This repo uses `anyhow::Result` throughout (not custom error types). The service is a long-running daemon where error context chains matter more than typed matching at call sites. This overrides the user preference for explicit error types.
- **`unwrap()` in tests:** Use `unwrap()` in test code. Do not use `expect("reason")` in tests.
