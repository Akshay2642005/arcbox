# 2026-10-03 — Can a machine's root be served to the Mac by its agent, read-write, fast enough to work in?

- Type: experiment
- Area: `guest/arcbox-agent/src/machine_export/`, `app/arcbox-daemon/src/machine_mount/`
- Outcome: ADR 0002; the `feat/machine-file-sharing` series
- Probes: `tests/e2e/tests/machine.rs` (`machine_root_mounted_on_the_host`) for the round trip; the throughput commands below are shell one-liners against a dev daemon, recorded here with their exact form
- Host / guest: Apple M5 Max (18 cores, 128 GiB), macOS 26.4 (25E246), VZ backend, boot bundle 0.8.8; daemon and agent built from this branch in release mode; machine images from the local distro mirror

## Question

OrbStack mounts every machine's filesystem under `~/OrbStack/<machine>`.
Can ArcBox do the same without changing boot-assets, with a userspace
NFSv3 server inside the machine's agent, reached over the machine's bridge
NIC — and is that path usable for a developer workflow: reading and
writing files, `ls` of a large directory, `git status`, and a 1 GiB file
in each direction?

## Hypotheses

1. The macOS NFS client mounts `nfs3_server` with `vers=3,tcp,port=P,mountport=P`
   over the vmnet bridge, and reads `/etc/os-release` of alpine, ubuntu and
   fedora machines through it.
2. A file written on either side is visible on the other immediately
   (within the attribute cache window on the Mac).
3. Bulk throughput through the userspace server and its loopback relay is
   within the same order as the `~/ArcBox` NFSv4 export (kernel nfsd over a
   vsock relay), and `ls`/`git status` complete in well under a second.
4. Larger `rsize`/`wsize` than the macOS default (32 KiB) raises bulk
   throughput measurably.

## Method

Isolated VZ dev daemon from this worktree (`dev-daemon.sh`, machine images
from the local mirror). For each of `alpine 3.24`, `ubuntu noble`,
`fedora 44`: `abctl machine create` + `start`, then on the Mac, with
`M=$ARCBOX_MACHINE_MOUNT_DIR/<name>`:

```sh
cat $M/etc/os-release | head -2
echo from-host > $M/root/from-host; abctl machine exec <name> -- cat /root/from-host
abctl machine exec <name> -- sh -c 'echo from-machine > /root/from-machine'; cat $M/root/from-machine
abctl machine exec <name> -- sh -c 'mkdir -p /root/many && cd /root/many && touch $(seq 1 1000)'
time ls $M/root/many | wc -l
time dd if=/dev/zero of=$M/root/1g bs=1m count=1024          # write
abctl machine exec <name> -- sh -c 'echo 3 > /proc/sys/vm/drop_caches'
time dd if=$M/root/1g of=/dev/null bs=1m                     # read
```

Git through the mount: this repository (1 396 tracked files) cloned from
the Mac with `time git clone <worktree> $M/root/repo`, then
`time git -C $M/root/repo status` twice (cold, warm), `git fsck`, and
inside the machine `find /root/repo -name '._*'` and `git status`.
Extended attributes: `xattr -w user.note "from the mac" $M/root/x`, read
back with `xattr -p` on the Mac, and `ls -a /root` inside the machine.
Admission: from a second machine, a TCP connection to the first machine's
export port. Then `abctl machine stop` and `abctl machine remove --force`,
checking `mount` and `pgrep -fl mount_nfs` after each, and a daemon stop
(`dev-daemon-stop.sh`) checking that the process exits.

## Results

| Machine | `machine start` (wall) | `ls` 1 000 entries, cold / warm | 1 GiB write | 1 GiB read (guest caches dropped) |
|---|---|---|---|---|
| alpine 3.24 | 3.1 s | 16 ms / 5 ms | 4.39 s (245 MB/s) | 3.22 s (333 MB/s) |
| ubuntu noble | 7.0 s | 21 ms / 8 ms | 6.20 s (173 MB/s) | 4.37 s (246 MB/s) |
| fedora 44 | 4.3 s | 16 ms / 10 ms | 4.28 s (251 MB/s) | 3.68 s (292 MB/s) |

- `/etc/os-release` read through the mount on all three; a file written on
  the Mac read back by `machine exec` and the reverse, on all three. The
  mount followed the machine's readiness event by 25–50 ms (daemon log).
  The guest saw the full 1 GiB (`stat -c %s`).
- Files created from the Mac are `root:root` in the machine; `chmod`,
  `ln -s`, `mv` and `rmdir` from the Mac behave; `/arcbox` is neither
  listed nor found.
- Git through the mount (ubuntu, with a cargo build running on the host):
  the clone of this repository took 15.9 s; `git status` on it 0.32 s
  cold and 0.08 s warm, with no errors. `git fsck` printed the same two
  commit-graph messages a local clone of the same worktree prints, so
  they are the source's, not the mount's.
- Sidecars: `xattr -w` from the Mac succeeded, read back, and survived a
  `mv` of the file on the Mac; neither `ls -a` on the Mac nor in the
  machine showed a `._x`, and `find / -xdev -name '._*'` in the machine
  found nothing after the `dd` and the clone, both of whose files carry
  `com.apple.provenance` on the Mac.
- `tests/e2e/tests/machine.rs` passes in 31 s without `KEEP_TEST_DIR`:
  the mount appears, both directions read back, the mount point is gone
  after `stop`, and the daemon exits on SIGTERM inside the harness's 15 s
  grace, so the temp dir is removed.
- Admission: a connection from the alpine machine to the ubuntu machine's
  export port was refused; the ubuntu agent logged
  `refused a peer that is not the host peer=192.168.64.3:36627`.
- `machine stop` (graceful) unmounted at the `MachineStopping` edge and
  returned in 3.5 s; `machine remove --force` left the unmount to the
  fallback path (`umount`, then `umount -f`), about 15 s. After both:
  no `mount_nfs` process, mount directory removed. The daemon stopped in
  8 s with `ArcBox daemon stopped` and no mount left under its data
  directory.

## Findings

- H1, H2 confirmed. H3: 173–251 MB/s writing and 246–333 MB/s reading
  through the userspace server and its loopback relay; `ls` and
  `git status` are far under a second. The `~/ArcBox` NFSv4 export was not
  re-measured side by side in this run. H4 not measured: the mount uses the
  client's default `rsize`/`wsize`.
- The macOS NFS client cannot store extended attributes on NFSv3, so it
  writes `._<name>` AppleDouble siblings — and recent macOS stamps
  `com.apple.provenance` on every file a downloaded app's process creates,
  so `dd` left `._1g` and a clone left `._pack-*.idx` in the machine, which
  git then read as a pack index (`non-monotonic index`). The agent now
  keeps `._` files the Mac creates in memory and out of directory listings
  (`vfs/sidecar.rs`). In memory but listed was not enough: git on the Mac
  reads any `._pack-*.idx` it lists as a pack index and the clone still
  failed. Unlisted, the clone is clean on both sides; see the sidecar line
  in Results.
- A forced remove kills the VM before the unmount on the stopping edge
  finishes, so the mount goes through the slow path. Making `remove` wait
  for the unmount the way a graceful `stop` effectively does is an engine
  change, left open.
- The e2e harness exposed a shutdown hang behind the `~/ArcBox` fix: a
  fresh mount under `/var/folders` answered `umount` with "Resource busy",
  and after the VM stopped the second cleanup pass `stat`'d the mount
  point through `canonicalize`, which a dead NFS server never answers.
  `current_mount_info` now resolves a mount point without `stat`'ing it,
  and the shutdown unmount escalates to `umount -f`.

## Decisions taken / open

- ADR 0002: userspace NFSv3 in the agent over the bridge NIC, with the
  relay as the admission control.
- Open: the mount point's name once `~/ArcBox` is reorganized; a `LINK`
  implementation; evicting ids the Mac has not touched for a long time;
  `remove --force` waiting for the unmount; Finder's `.DS_Store` files,
  which are ordinary files and land in the machine as on any network
  mount.

## Addendum 2026-10-04 — the Mac's sidecars become the files' own extended attributes

- Outcome: ADR 0002 point 7 revised; the `feat/machine-export-xattrs` series
- Probes: the `machine_export::vfs::sidecar` unit tests (`tests.rs`, against a real directory, with the `._` files macOS 26 wrote on a FAT volume as fixtures in `sidecar/testdata/`); the shell checks below against a dev daemon
- Host / guest: as above; ubuntu noble and alpine 3.24 machines on VZ, boot bundle 0.8.8; the machine's data disk is btrfs with 16 KiB nodes under an overlayfs root

### Question

The first version kept the Mac's `._<name>` AppleDouble sidecars in a
table in the agent's memory: 4 096 at most, gone with the machine. Can
the sidecar's content be translated into the target file's Linux
extended attributes instead — surviving a stop, unbounded, visible in the
machine — and where does a value too big for the filesystem go?

### Method

- The filesystem's limit: `setxattr` with a growing value on a fresh file
  (python3 on ubuntu, a `setfattr` loop on alpine), binary search on the
  size, for a 6-byte and a 27-byte name; then the same against an
  attribute that already exists, growing it from 1 000 bytes; the node
  size from `/sys/fs/btrfs/<uuid>/nodesize`.
- What the Mac writes and how: `._x` captured on a FAT disk image after
  `xattr -w user.note`, a Finder tag, Finder Info and a resource fork
  (the fixtures), and the NFS operation sequence recorded with temporary
  tracing in the agent for `xattr -w`, a resource fork, `mv` and `rm`.
- Acceptance, on each machine: `xattr -w user.note hi x` and `getfattr -d
  -m - /root/x` in the machine; a Finder tag (`com.apple.metadata:_kMDItemUserTags`);
  `cp -p` of a file with two attributes; a 266 KB
  `com.apple.ResourceFork` read back with `cmp`; `mv` within and across
  directories and `rm`, watching `.arcbox-xattrs/`; an attribute grown
  from 12 000 to 16 000 bytes; `xattr -d`; an attribute set inside the
  machine read from the Mac; `cp -R` from a FAT volume holding a real
  `._b` written before `b`; `machine stop` and `start`; `git clone` of
  this repository through the mount and `find / -xdev -name '._*'` in the
  machine; the 1 GiB `dd` lines from the method above, run the same day
  before and after the change.

### Results

| | fresh attribute, name + value | growing an existing attribute from 1 000 bytes |
|---|---|---|
| ubuntu noble, btrfs nodesize 16384 | 16 228 (`ENOSPC` past it; 16 222 for `user.p`, 16 201 for `user.com.apple.ResourceFork`) | 11 030, then `ENOSPC`; remove and insert at 16 222 succeeds |
| alpine 3.24, same disk layout | 16 228 | 8 854 (the first probe, which grew one attribute in place) |

16 228 is `nodesize - 101 - 25 - 30`: one leaf item, header, item and
`dir_item` deducted. Growing in place fails as soon as the leaf the item
sits in has no room; a fresh insert gets a leaf of its own. So the store
tries the inode up to 16 228, retries a refused replacement as remove and
insert, and sends the rest to the side entry.

What the Mac writes, from the fixtures and the trace (macOS 26.4):

- A sidecar is a 4 096-byte file: header, two entries, 32 bytes of Finder
  Info, the `ATTR` header at offset 84, entries from 120, attribute data
  from `data_start`, slack, and the 286-byte "intentionally left blank"
  resource fork at 3 810. A resource fork is written at 3 810 and the file
  grows.
- Creating a file stamps `com.apple.provenance`: `CREATE ._x`, one 4 096-byte
  `WRITE`, `COMMIT`. `xattr -w` then rewrites only the header area (189
  bytes at offset 0) and commits. A resource fork is `SETATTR` of the
  size, a read of the header, an 84-byte `FILE_SYNC` write of the
  AppleDouble header, 32 KiB `UNSTABLE` chunks in any order, `COMMIT`.
- `mv x y` is `RENAME x y`, then provenance stamped under the stale name —
  `LOOKUP ._x` (gone), `CREATE ._x`, a 4 096-byte write, `COMMIT` — then
  `LOOKUP ._y` and `RENAME ._x ._y`. On a FAT volume the stamp is added to
  the old sidecar, which the rename then carries; with the sidecar
  following the inode, the fresh `._x` carries only provenance, so an
  image the Mac *created* has to be merged into the target's attributes,
  not applied over them. The first implementation applied it over them
  and lost `user.note` and the fork on every `mv`.
- The client looks up `._<name>` asynchronously during every create,
  remove and rename and caches a negative answer; a later `setxattr`
  then sends `CREATE` for a sidecar the server already answers, which is
  the second reason a `CREATE` means "add", not "replace".
- A removal of the last attribute does not remove the `._` file: on FAT
  the empty sidecar stays.

Acceptance (both machines unless noted):

Acceptance, alpine 3.24 and ubuntu noble alike, the daemon and agent
built from the rebased branch (ADR 0003's `~/ArcBox/machines/<name>`
mount point):

- `xattr -w user.note hi x`: read back on the Mac; `getfattr -d -m - /root/x`
  in the machine shows `user.user.note="hi"` next to
  `user.com.apple.provenance`; no `._x` anywhere on disk.
- A Finder tag (`com.apple.metadata:_kMDItemUserTags`, a plist): read
  back on the Mac, `user.com.apple.metadata:_kMDItemUserTags` in the
  machine. The colour in Finder's window was not checked: the Finder
  AppleScript query timed out against the mount.
- `cp -p` of a file carrying `user.a` and `kMDItemWhereFroms`: both on
  the copy, on the Mac and in the machine.
- A 266 KB `com.apple.ResourceFork`: reads back byte-identical; lives in
  `/root/.arcbox-xattrs/x`, nothing of it on the inode.
- `mv x y`, then `mv y sub/z`: note and fork intact from the Mac after
  each; the side entry moved to `/root/.arcbox-xattrs/y` and then to
  `/root/sub/.arcbox-xattrs/z`, the emptied store removed each time;
  `rm sub/z` removed the entry and the store.
- `user.big` grown from 12 000 to 16 000 bytes: on the inode, no side
  store — the replace that btrfs refused went through as remove and
  insert.
- `xattr -d user.big`: gone on both sides.
- `setfattr -n user.from_machine` on a new file inside the machine:
  `xattr -p from_machine` on the Mac shows it at once. A sidecar the Mac
  has read before shows a machine-side change when its attribute cache
  expires, which the earlier run measured at under 65 s and over 6 s.
- `cp -R` from a FAT volume holding a real `._b` written before `b`: `b`
  carries the attributes, no `._b` in the machine.
- `machine stop`, `machine start`: note, tag and fork still read back.
- `git clone` of this repository through the mount: no error, `git status`
  clean; `find / -xdev -name '._*'` in the machine finds nothing. Two
  side stores exist afterwards: `/root/.arcbox-xattrs` for the fork
  above, and `/root/repo/.claude/.arcbox-xattrs` — `.claude/skills` is a
  symlink, Linux takes no `user.*` attribute on a symlink, so the
  provenance stamp the Mac gave it went to a side entry.
- The first run against the branch found two faults the unit tests had
  not: after `mv` the fresh provenance-only `._x` renamed over `._y`
  replaced `y`'s attributes (fixed by merging an image the Mac created),
  and a sidecar synthesized without XNU's slack refused the next
  `setxattr` with `ENOATTR` (fixed by synthesizing XNU's own layout).
  Both are in the unit tests now.

Throughput, same `dd` lines, same day:

| Machine | 1 GiB write, before → after | 1 GiB read (guest caches dropped), before → after |
|---|---|---|
| ubuntu noble | 269 MB/s → 308 MB/s | 302 MB/s → 426 MB/s |
| alpine 3.24 | 277 MB/s → 267 MB/s | 370 MB/s → 412 MB/s |

Within the run-to-run spread of the host; the sidecar work is off the
data path, which only gains a name check per operation.

### Findings

- The Mac's sidecars translate into the files' own attributes: every
  acceptance item above holds on both machines, the table and its cap
  are gone, and attributes survive a stop.
- What the Mac's client does is the design's input, not an afterthought:
  it creates sidecars it believes new (after `mv`, and from a stale
  negative name-cache entry), rewrites only the header area, writes the
  fork at a fixed offset, and grows a sidecar in place — so an image the
  Mac created merges, an image it looked up replaces, and a synthesized
  image has XNU's shape.
- Costs: a `._<name>` lookup is a `stat` and a `listxattr` on the target
  (the earlier version answered from memory), and something on the Mac
  probes `._<name>` for every file it sees — about 6 000 lookups in one
  run — so a negative cache may be worth adding if that shows up; the
  Mac's attribute cache delays a machine-side change by up to 60 s; the
  hidden `.arcbox-xattrs/` directory is visible from inside the machine
  wherever a value needed it, a symlink's attributes included; the
  export root's own overflow has nowhere to go (`EFBIG`).
- Open: the Finder colour check; the `._` probing's source and whether a
  negative cache is needed; the `tests/e2e` round trip does not cover
  attributes yet.
