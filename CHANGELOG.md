# Changelog

`tpi`, the command line for a Turing Pi 2 BMC, as built by this fork.
Upstream's history is in the git log; this file starts where the fork
diverges, at 1.0.7.

The tool ships twice from one source: as release binaries for five targets, and
as a Buildroot package inside the firmware (`--features localhost,native-tls`),
where it talks to `127.0.0.1` without credentials. The two float
independently, so being a release behind the board it points at is normal and
every fork command says so rather than failing with the daemon's parameter
error.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [1.2.0] — 2026-09-09

### Added

- **`tpi hostname [<name>]`** (SQU-138). Prints the live name, and the one that
  takes effect at the next boot when the two disagree — which happens when
  somebody has run `hostname` by hand. Setting it says, *before* it happens,
  that the metrics `instance` label moves with the name and a Prometheus
  history will not follow the board across the rename. That is the part nobody
  expects, and renaming back does not undo it.
- **`tpi ntp show | set <servers…>`** (SQU-167). The first server is the
  preferred one. `set` with no servers goes back to the pool the image ships
  with. `show` prints the clock's state underneath, and says outright when the
  board's firmware has no `sourcedir` line — a saved list that is silently
  never read is the one failure printing the servers cannot reveal.
- **`tpi config export [--file] [--with-secrets]` and `tpi config import <file>`**
  (SQU-142). `--with-secrets` includes the metrics token and warns **on
  stderr** that the file is now a credential; when the export goes to stdout it
  says nothing, because stdout is being piped somewhere and a stray sentence
  would land in the file.

  Import parses the file before posting it, so something that is not an export
  fails before anything on the board changes, and prints the daemon's per-field
  report. **It exits 1 if any field failed**: the import is not transactional,
  and a script that moved on from a half-configured board would be worse than
  one that stopped.
- **`tpi firmware list` says what is staged and how the last boot went.**
  Without it a shell user had no way to learn a rollback had happened at all —
  the gate rejects an image, the board reboots onto the old one, and the list
  would report the old version as running with nothing to say an install had
  been attempted and refused.

## [1.1.1] — 2026-09-09

### Fixed

- **`firmware install` could not install anything the `local` source listed**
  (SQU-168). It posted `firmware_install&source=local`, which the daemon
  refuses by design — *"a local image is installed through
  `opt=set&type=firmware` with `local=1`"* — so an image parked on the SD card
  resolved correctly and then failed with a 400. Found while flashing the
  promotion-gate test image, which had to be installed by hand.

  A local candidate now goes through the transfer endpoint with the path the
  catalogue reported, and the flash is followed to completion. That last part
  matters: the endpoint answers with a handle and writes in the background, so
  printing "staged" on the response would invite a reboot onto a half-written
  image.
- **The default host failed with an error that named the URL and not the
  problem.** `tpi about` with no `--host` died inside reqwest with *error
  sending request for url (…)*. The default is `turingpi.local`, which the
  board advertises over mDNS — and firmware v2.8.0 and earlier of this fork
  ship without an mDNS responder, so the name is silent on a board that is
  otherwise working. The name is now resolved before the request, and only
  when it is the default: an address someone typed deserves the real error,
  not a guess about DNS.

### Added

- `SourceCatalog` carries the source's `kind`, which is what decides the two
  paths above.
- `devbox.json`, `.envrc` and a `Justfile`, so `just check` runs exactly what
  CI runs. The repository had no way to build it locally at all — the
  toolchain came from whatever happened to be on the machine, and
  `--features native-tls` needs pkg-config and OpenSSL that CI's runner image
  happens to carry.

## [1.1.0] — 2026-09-08

### Added

- `firmware list`, `firmware install`, `firmware check`, `firmware sources`,
  `about`, `thermal` and `metrics token show|rotate` — every endpoint this
  fork's daemon added, which until now were reachable only from the browser.
- `firmware check` exits **10** when an upgrade exists, so
  `tpi firmware check || notify-me` works from cron without parsing output. It
  is deliberately not `1`, which stays "the command failed".
- Each command asks the board its version first and refuses with *"this board
  runs bmcd X; `…` needs Y"* rather than the daemon's *"Invalid `type`
  parameter"*, which names the query parameter and tells you nothing.

### Fixed

- The version comparison is numeric. As text `"2.10.0" < "2.9.0"`, so a string
  compare would tell a board on 2.9.0 that it satisfied a 2.10.0 requirement.
- A cursor could move right past the end of the prompt buffer.

### Changed

- The toolchain is pinned by `rust-toolchain.toml` to 1.98.1, the version the
  firmware's Buildroot builds the on-board copy with. CI used `stable`, so a
  Rust release could change what CI validated without anyone touching the repo.
- CI off the archived `actions-rs/*`. `clippy-check` posted findings as a
  GitHub Check the token could not create, so the job failed with "Resource not
  accessible by integration" while clippy itself reported no warnings.
- Releases are cut by a version tag rather than a push to the default branch.

[Unreleased]: https://github.com/excavador-turing/tpi/compare/v1.2.0...hive
[1.2.0]: https://github.com/excavador-turing/tpi/releases/tag/v1.2.0
[1.1.1]: https://github.com/excavador-turing/tpi/releases/tag/v1.1.1
[1.1.0]: https://github.com/excavador-turing/tpi/releases/tag/v1.1.0
