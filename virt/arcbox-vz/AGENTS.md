# arcbox-vz Agent Guidance

The VZ (Virtualization.framework) backend. All framework interaction happens
in **ArcBoxVZShim**, a SwiftPM static library under `shim/` compiled and
linked by `build.rs`. There is no ObjC runtime interop in Rust — no
`msg_send`, no hand-rolled blocks, no `objc2` dependency. VZ is the oracle
backend (`virt/AGENTS.md`): keep it boring and correct.

## The C ABI boundary (the one contract that must never drift)

- `shim/Sources/ArcBoxVZShim/Exports.swift` (`@_cdecl`) and `src/shim_ffi.rs`
  (extern declarations) mirror each other in a **normative symbol order** —
  review side by side. A symbol lands in the same PR as its Rust caller.
- The shim is statically linked from this same source tree in the same build,
  so version skew is impossible and there is **no runtime ABI-version
  handshake**. The drift defense is `link_coverage` (a `SYMBOLS` address
  table + `EXPECTED_SYMBOL_COUNT`): a renamed or dropped `@_cdecl` export
  fails `cargo test -p arcbox-vz` at **link time**. Adding a symbol updates
  that table and the count; the C ABI does no signature checking across the
  boundary, so keep the two files' declarations literally aligned.
- Conventions (headers of Errors.swift / shim_ffi.rs are authoritative):
  strings crossing out of Swift are strdup'd and freed by Rust
  (`abx_string_free` / `take_string` / `take_error_string`); handles are
  `Unmanaged` object pointers at +1 released via `abx_object_release`;
  borrows never consume the +1. Callbacks are C fn pointers + ctx, invoked
  **exactly once** from the VM's dispatch queue; the Rust trampoline consumes
  the boxed sender and must clean up undeliverable resources (see
  `vsock_trampoline`'s fd close and `object_trampoline`'s handle release).
- ObjC exceptions are **not caught anywhere**: every throwing VZ call site is
  precondition-guarded (`validate` before build, `can_stop` before stop, fd
  pre-checks). An NSException is a programmer error and must crash loudly
  rather than unwind into Rust frames. Do not add a catch helper.

## Queue affinity lives in Swift

`VZVirtualMachine` and its device objects are queue-affine
(`dispatch_assert_queue` aborts on violation — the historical idle-balloon
crash class). The shim owns the per-VM serial queue inside `ABXVMBox` and
queue-syncs every access; the box types (`ABXVMBox`, `ABXSocketDeviceBox`,
`ABXBalloonBox`, `ABXInstallerBox`) pair object + queue precisely so no raw
VZ object can escape without its queue. Never return a bare VZ object across
the ABI.

## build.rs landmines (each was hit once; comments at the sites)

- Swift invocations go through absolute `/usr/bin/xcrun` with
  `SDKROOT`/`DEVELOPER_DIR` scrubbed: the devenv nix SDK is
  SwiftPM-incompatible and devenv's PATH shadows `xcrun` with xcbuild's fake.
- rustc-driven links ignore static-archive autolink hints: build.rs parses
  `otool -l` and forwards `-lswift*`/`-lobjc` explicitly
  (`swiftCompatibility*` skipped — toolchain-static, irrelevant at the
  macOS 13 floor).
- The Swift runtime stub search path must match the SDK the **final linker**
  uses (SDKROOT when set, toolchain default otherwise); Swift ABI stability
  makes the compiler/linker SDK mix sound.
- The published crate ships `shim/**` (Cargo.toml `include`); verify packaging
  with `cargo package -p arcbox-vz --no-verify` after touching the file set.

## Validation

1. `cargo test -p arcbox-vz` — boundary tests run **unsigned** (entitlement is
   enforced at VM init, not config alloc); keep new smoke tests
   entitlement-free or they break CI.
2. `xcrun swift-format lint --strict --parallel --recursive virt/arcbox-vz/shim`
   (CI-enforced; `swift-format format --in-place` fixes).
3. e2e: `cargo test -p arcbox-e2e --test boot_assets -- --ignored` with
   `ARCBOX_VM_BACKEND=vz`; `backend_matrix` for the VZ↔HV oracle split
   (see `virt/AGENTS.md`).

## Known non-obvious semantics

- Lifecycle ops resolve on VZ completion handlers — there is no state
  polling; do not reintroduce it.
- **Balloon inflation is a host-side no-op** (measured 2026-07-29, macOS
  26.4): Apple neither deallocates nor `madvise`s pages the guest gives up
  — host `phys_footprint` stays at the configured memory size from boot,
  and under host memory pressure the kernel compresses ballooned pages as
  live data. `set_target_memory_size` only resizes what the *guest* may
  use. This is why the idle balloon never engages on VZ, and why VZ can
  never be made to release: the guest's RAM is Apple's, not the daemon's.
  HV returns idle memory on its own through free page reporting and needs
  no host-side target either — see `app/AGENTS.md` and
  `virt/arcbox-vmm/AGENTS.md` "Releasing guest RAM".
- **Only the System VM asks for nested virtualization**
  (`VmConfig::nested_virt`, set in `engine/.../vm_lifecycle/boot.rs`).
  Hypervisor.framework backs each nested-capable VM with its own guest
  hypervisor address space and the host has about a dozen (measured
  2026-09-28, M5 Max, macOS 26.4: 12); the 13th `hv_vm_create` asserts in
  `GuestHypervisorSpaceManager::create`, the VZ helper dies with SIGTRAP,
  and the daemon reports `Internal Virtualization error` on every VM
  start. Plain VMs share one space (28 more fit beside 12 nested). Never
  enable it on a user machine. The reason shows up in the crash report,
  not the unified log:
  `~/Library/Logs/DiagnosticReports/com.apple.Virtualization.VirtualMachine-*.ips`.
- **A VZ console pipe with no reader wedges the whole VM.**
  `VZFileHandleSerialPortAttachment` writes guest output into a host pipe;
  when it fills, the guest's virtio-console write never completes, every
  vCPU spins at 100% in `hv_vcpu_run`, and the VM's vsock stops answering
  (exec/ssh/stop all time out). `MachineManager::start` therefore starts
  `machine/serial.rs` for every machine, not only the System VM — one
  `AsyncFd` task per port on `DarwinVm::dup_serial_readers`, reading on
  readiness, never on a timer — and stops it when the machine stops; never
  add a VZ console the daemon does not drain. The pipe is not a fixed
  64 KiB: XNU sizes pipe buffers to what it can spare, and under host
  pipe-memory pressure (measured 2026-09-29 with ~4300 open pipes) a fresh
  pipe holds 512 bytes, which is why the drain must not be a poll (one pipe
  per poll interval was 650 KB/s at 64 KiB and ~5 KB/s at 512 B, with the
  guest spinning for the whole write). The pipe never delivers EOF while
  the VM is configured — the host keeps the write end it handed to VZ — so
  the drain ends on the manager's cancellation, not on `read` returning 0.
  Reproduce the wedge with 200 KB of text to `/dev/hvc0` (the
  `machine_console` e2e); do not use a 2 MB flood on a machine you care
  about: on kernel 6.18.38-arcbox it corrupted guest memory (oops / btrfs /
  `Bad rss-counter`) in 5 of 12 systemd machines, and no-flood controls did
  not.
- **A distro's own `console_loglevel` decides how fast it fills that pipe.**
  Every machine boots `console=hvc0` (`engine/.../machine.rs`), so all
  kernel `printk` lands in the pipe above. A distro whose kernel default is
  the noisier `7` — Debian — streams every `info`-level record there; the
  loudest steady source is the audit subsystem, one `audit:` line per
  systemd unit start/stop (~285 KiB/day on an idle machine, measured
  2026-09-29), which crosses the 64 KiB pipe within a day. Ubuntu ships
  `console_loglevel=4` in `sysctl.d` and is silent after boot; the other
  mirrored distros sit between. The machine cmdline now pins `loglevel=4`
  (`QUIET_KERNEL_CONSOLE` in `engine/.../machine.rs`) so every distro
  matches Ubuntu regardless of its own default — `err` and above still
  reach the console. The drain above is the correctness backstop; this cap
  keeps a distro's `info` chatter out of the daemon log.
- `VZLinuxRosettaAvailability` raw values are notSupported=0, notInstalled=1,
  installed=2 (a hand-written mapping once had 1 and 2 swapped; the shim now
  returns raw values and Rust maps them — keep them aligned with the SDK).
- `MacAuxiliaryStorage::open` does not verify the file; VZ checks at
  configuration-validate time.
- Installer progress is a Rust-side 2s poll of `fractionCompleted`; the
  installer is constructed on the VM queue (its initializer asserts).
