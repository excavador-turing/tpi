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

## [1.6.0] — 2026-09-10

### Changed

- **`firmware install` no longer resolves the version before posting it.** The
  cached listing now informs the confirmation line; it cannot refuse.

  The refusal existed because of a comment claiming the board "would accept a
  source/version pair that offers nothing and fail later, in the middle of a
  download". Measured on a board: the daemon answers **400 in 0.48 s** with
  `tpi-selfupdate: cannot fetch SHA256SUMS for v9.9.9`, before any download,
  staging nothing. The refusal protected against nothing.

  It cost something real, though. The listing is a cache that lags a release by
  up to half an hour, and `firmware check` reads a different cache with its own
  timing, so `check` printed `install it with: tpi firmware install <v>` and
  the next command rejected it. That is now impossible, because install has no
  opinion to disagree with.

  1.5.0 and 1.5.1 both tried to fix this by making the stale listing fresh —
  1.5.0 inertly, 1.5.1 by waiting up to three minutes. Both were the wrong
  shape. **The board is the authority on what the board can fetch.**

  When the listing does not carry the version, the source is chosen in order:
  `--source`; the source the running firmware came from; the only remote
  source. A local source is never guessed — a version absent from the listing
  cannot be the file on the SD card. Ambiguity asks.

### Fixed

- **`firmware list --refresh` could tell you a stale listing was current.** It
  waited 30 attempts of 2 s, then gave up and printed the previous listing with
  nothing said, indistinguishable from a fresh one. Polls timed on a board took
  74 s, 78 s and 140 s, so a minute was short of even the ordinary case.

  The bound is four minutes, it says so when it gives up, and it names the
  timestamp of what it is showing.

- **`firmware list` never showed how old the listing was.** It does now, on
  every run, refreshed or not. The daemon had always sent the age; the client
  discarded it.

## [1.5.1] — 2026-09-09

### Fixed

- **1.5.0's fix did not work; this one does** (SQU-184). 1.5.0 asked the board
  to re-poll its sources and resolved against the answer. But
  `firmware_available&refresh=1` **does not fetch** — the daemon spawns the
  poll and returns the cached list immediately, with `refreshing` set. So the
  second resolution ran against exactly the same stale list as the first, and
  the change was inert.

  Confirmed on the board: a forced request came back in one second carrying the
  same `checked_at` it had before the call.

  `install` now waits for the poll to land before re-resolving. The predicate
  is `checked_at`, which advances exactly once a poll completes, so it can
  actually be false while the poll runs — which is what 1.5.0 never checked.
  The wait is bounded at 180 s; a refusal that hits that bound says so rather
  than claiming no source has the version.

  Measured on the board, same command, same question, a version that exists
  nowhere:

  | build | elapsed | `checked_at` |
  |---|---|---|
  | 1.5.0 | 1 s | unchanged |
  | 1.5.1 | 58 s | 20:50:57 → 20:52:11 |

  The deadline is a measurement too: a poll over four sources took 78 s, where
  the daemon's own comment says 16. There is a test asserting the bound clears
  it, so shrinking it cannot silently bring the wrong refusal back.

## [1.5.0] — 2026-09-09

> **This release does not do what the entry below says.** The retry it added is
> inert, for the reason in 1.5.1. Kept as written, because the mistake is more
> useful on the record than tidied away.



### Fixed

- **`firmware install` no longer refuses the command `firmware check` just
  printed** (SQU-184). The two read different caches with independent timing:
  `check` asks the board's update checker, which reaches GitHub, while
  `install` resolved the requested version against the firmware catalogue,
  whose entries are fresh for half an hour. For up to that long after a
  release, one could see it and the other could not.

  Reproduced on a board at 18:57:03 UTC, twenty-seven seconds after v2.16.0
  published: `check` printed `install it with: tpi firmware install v2.16.0`
  and exited 10; the very next command answered `no source offers v2.16.0`
  and exited 1.

  `install` now re-resolves against a forced poll before believing that no
  source has the version. The successful path is unchanged and pays nothing;
  only the request that was about to be refused waits for the fan-out, which
  is nothing against a firmware download. Aligning the two cache lifetimes was
  considered and rejected: the checker saw the release immediately despite a
  nominally longer cache, so their states are independent and no ordering
  between them can be assumed.

  The refusal itself stays. Posting an unresolved version to the board would
  fail in the middle of a download instead of before it starts.

## [1.4.0] — 2026-09-09

### Removed

- **`tpi metrics show | rotate`** (SQU-178). The metrics token is gone from
  the daemon: `/metrics` moved to its own listener on port 9110 that takes no
  credential, so there is nothing left to show or rotate. A script that calls
  this now gets a clear "unrecognized subcommand" from clap rather than a
  request that quietly does nothing.

- **`tpi config export --with-secrets`**, and the warnings that went with it.
  The export carried exactly one secret, the metrics token, so with that gone
  the document is no longer a credential and the flag had nothing to include.

## [1.3.0] — 2026-09-09

### Added

- **`tpi cooling set <device> <speed> --hold`** holds the fan at a step by
  pausing the zone's governor, and **`--auto`** hands it back (SQU-170).
  Without `--hold` the command means what it always did: write the step and let
  the governor take it back a few seconds later. `--override` is accepted as an
  alias for `--hold`.

- **`tpi cooling status` has a Governor column**, reading the `overridden`
  field bmcd 2.21 added. A daemon that does not send it prints `-` rather than
  `running`, because "the governor is running" is the one fact the column
  exists to report and an older daemon has not reported it.

## [1.2.3] — 2026-09-09

### Fixed

- **Releases were green and empty.** The release job was upstream's, written
  for a push to master: read the version from `Cargo.toml`, and if a tag for it
  already exists, abort. This fork changed the trigger to the tag itself, so
  the job ran *because* the tag was pushed, found it, and skipped every step
  that publishes anything — while reporting success.

  The result is that **1.1.0, 1.1.1, 1.2.0, 1.2.1 and 1.2.2 have no release
  page and no binaries**, on a tool the documentation tells people to install.
  This is the first release of this fork that actually ships something.

  The job now publishes from the tag it was triggered by, and two guards stop
  the failure recurring quietly: the tag must agree with `Cargo.toml`, and
  there must be at least one artifact to attach. Both were checked in each
  direction before this was tagged. The actions it calls are pinned by commit,
  matching the other repositories here.


## [1.2.2] — 2026-09-09

### Fixed

- **`firmware install <version>` had never worked in any release.** The
  positional was named `version`, and the `--version` flag clap generates for
  every subcommand has the same id. clap's debug assertion reports the
  collision by panicking; a release build skips the assertion and gets
  undefined argument handling instead. Every install this fork ever performed
  went through the web interface, `curl`, or the on-board updater. Found by
  using the command as the flash tool for v2.9.2 — which is what it exists for
  — and verified by doing exactly that: the install went through the transfer
  endpoint, the board staged it, `firmware list` said so, and the reboot
  promoted it.
- **`firmware list --refresh` printed the previous list.** The daemon answers
  a refresh at once and re-polls the sources behind itself (bmcd 2.11.0). That
  is right for a page, which draws a spinner, and wrong for a shell command
  that asked for a fresh answer. It now waits for the daemon to finish,
  bounded at a minute, and says *polling the sources…* on stderr so the wait is
  not mistaken for a hang.

## [1.2.1] — 2026-09-09

Five bugs, all found by running the tool against a board for the first time.
Every fork command was broken; none of them had ever been executed against
real hardware, and the examples in the documentation had never been produced.

### Fixed

- **Every fork command failed with "invalid type: map, expected a string."**
  `unwrap_response` returned `body["response"]` — the *array* — instead of the
  `result` inside it. Serde will deserialise a struct from a sequence, taking
  the elements as fields in order, so `About` read the single `{"result": …}`
  map as its first `String` field. Every command begins with the version gate,
  which reads `about`, so **every one of them failed**. The unwrapping now digs
  through the wrapper and passes a bare body through unchanged, because the
  transfer endpoint answers `{"handle": N}` with no wrapper at all.
- **`firmware check` printed " is current"** with no version. The daemon
  answers per channel — `{checked_at, error, stable: {…}, edge: {…}}` — and
  this deserialised the *outer* object into a struct whose every field is
  `#[serde(default)]`. So it "succeeded" with everything empty. `default` on
  every field is what turned a shape mismatch into silence; it now reads the
  stable channel and fails loudly if no channel is there.
- **`thermal` printed raw JSON.** Its formatter expected an array of
  `{name, temp}` in millidegrees — a shape the daemon has never sent. It now
  renders the sensors with the trip that is governing the fan, and the fan's
  step with the duty that step commands.
- **Every refusal printed as raw JSON.** A refusal arrives in the same wrapper
  as a success, and the error path read `response` as a string, so the message
  came out buried and escaped inside `{"response":[{"result":"…"}]}`.
- **`firmware list --all` panicked.** `-a` was claimed by both `--all` and the
  global `--api-version`, which clap's own debug assertion catches by
  panicking. `--all` is long-only now.

### Changed

- The rename warning prints **after** the board accepts. Printed first, it
  announced a rename that the next line then refused, and a warning about a
  consequence that did not happen is worse than no warning.

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

[Unreleased]: https://github.com/excavador-turing/tpi/compare/v1.2.2...hive
[1.2.2]: https://github.com/excavador-turing/tpi/releases/tag/v1.2.2
[1.2.1]: https://github.com/excavador-turing/tpi/releases/tag/v1.2.1
[1.2.0]: https://github.com/excavador-turing/tpi/releases/tag/v1.2.0
[1.1.1]: https://github.com/excavador-turing/tpi/releases/tag/v1.1.1
[1.1.0]: https://github.com/excavador-turing/tpi/releases/tag/v1.1.0
