# ArcBox engineering docs

`AGENTS.md` files hold the rules an agent or engineer must follow while
editing a directory. This tree holds everything else worth keeping: how a
subsystem works, what was decided and why, and what was measured.

## Layout

| Directory | Holds | Naming |
|---|---|---|
| `docs/*.md` | Reference guides: how a subsystem works today and its measured limits (`daemon-lifecycle.md`, `data-directories.md`, `fs-perf-limits.md`, …). Kept current in the change that alters the behavior. | `<topic>.md` |
| `docs/adr/` | Architecture Decision Records: one decision per file, immutable once accepted; a change of mind is a new ADR that supersedes the old one. | `NNNN-<slug>.md`, numbered in order of acceptance |
| `docs/logs/` | Development and experiment logs: what was asked, what was run, the numbers, the findings, what changed because of it. Append-only; a later correction is a new entry that links back. | `YYYY-MM-DD-<slug>.md` |
| `docs/architecture/` | Long-form designs that span many crates and outlive any one change (charter, stack designs). Carry a `Status:` line. | `<topic>.md` |
| `docs/plans/` | Execution plans for a piece of work: scope, steps, acceptance. Carry a `Status:` line and are closed (not deleted) when done, with a pointer to the log entry or ADR that records the outcome. | `<topic>.md` |

Templates: `docs/adr/TEMPLATE.md`, `docs/logs/TEMPLATE.md`.

## Which one to write

- You changed behavior or a public contract → update the reference guide,
  and the `AGENTS.md` of the directory if a rule changed.
- You ran an experiment, a benchmark, a bisect, or an investigation that
  produced numbers or a verdict → a `docs/logs/` entry, even when the
  verdict is "nothing to do". Check the probes into `tests/bench/` or an
  e2e target so the entry can be rerun.
- You chose between designs, retired a mechanism, or set a rule that a
  future change must not undo silently → an ADR. It cites the log entries
  that carry the evidence; it does not repeat their tables.
- You are about to start a multi-step piece of work → a plan, with the
  decisions already locked (root `AGENTS.md` "Planning").

A log entry is written in the same commit series as the work it records,
not afterwards from memory.

## Index

### Decisions

- [0001 — On the HV backend, asserting the SPI is the whole wake](adr/0001-hv-spi-is-the-whole-wake.md) (2026-09-30)

### Logs

- [2026-09-30 — How a guest vCPU actually gets woken on HV](logs/2026-09-30-hv-wake-path-experiments.md)
- [2026-09-29 — vsock RX: drain a stream per round, aim the kick](logs/2026-09-29-vsock-rx-round-and-targeted-kick.md)

### Reference guides

- [Daemon lifecycle](daemon-lifecycle.md) — startup pipeline, lock/handoff, residual state
- [Data directories](data-directories.md) — every path the daemon writes
- [Boot assets](boot-assets.md) — the kernel/rootfs bundle and its pin
- [Disk reclaim](disk-reclaim.md) — how freed guest space returns to the host
- [VirtioFS performance and limits](fs-perf-limits.md)
- [Network datapath performance and limits](net-perf-limits.md)
- [Host tunnel proof: `tun_proxy`](surge-tun-proxy.md)
- [arcbox-helper](helper.md) — the privileged helper
- [Code signing](code-signing-troubleshooting.md)
- [macOS guest VMs](macos-guest.md)
- [Sandbox gRPC API](sandbox-api.md)
- [Coding agents in a sandbox](agent-sandbox.md)
- Historical: [VirtIO improvements plan](virtio-improvements-plan.md) (2026-04), [VirtIO queue convergence](virtio-queue-convergence.md) (2026-06, its `Status: Planned` and target trait are stale — see `virt/AGENTS.md`)
