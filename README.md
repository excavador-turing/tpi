# `tpi` command-line tool

This is a tool to control your Turing Pi board. It can be used from within the
BMC (for example, via SSH) or outside, like your personal computer. In the
latter case, it will try to connect to your board at the default hostname of
`turingpi.local` unless `--host` is specified. For more information and usage,
see `--help`.

## Upgrading: certificate verification

From tpi 2.0.0, `tpi` checks the board's certificate. Before, it accepted any
certificate, so anything that answered to the board's name was given the
password and the login token. A Turing Pi board serves a self-signed
certificate, so **after the upgrade every call to a board fails once, until
you tell tpi which certificate is yours.** It is one step per board, per
computer:

1. Run any command. tpi refuses and prints the certificate it was shown,
   with its SHA-256 fingerprint.
2. Compare that fingerprint with the board's own. It is in BMC-UI under
   Security, Certificates, and on the board's serial console it is
   `openssl x509 -in /etc/ssl/certs/bmcd_cert.pem -noout -fingerprint -sha256`.
   Do not take it from the refusal alone: the point is to compare it with a
   second source.
3. If they are the same, run the command again with the fingerprint:

   ```shell
   tpi --host turingpi.local --pin 74:41:FC:E8:... info
   ```

   or `TPI_PIN=74:41:FC:E8:... tpi info`. tpi accepts the certificate only if
   it has exactly that fingerprint, and then remembers the board's public key
   for this host and port. A wrong or mistyped fingerprint is refused and
   nothing is remembered.

Later calls need no `--pin`. Firmware that keeps its key when it renews its
own certificate keeps the pin valid; older firmware makes a new key at each
renewal, and then you pin again. Scripts need the one pin per board
before they run; a script that pins as its first step can use `TPI_PIN`.

Things to know:

- **The pin is strict.** A stored pin wins over a certificate signed by a
  certification authority. After `tpi tls install`, or after a firmware that
  makes a new key when it renews, tpi says the certificate "no longer matches
  its pin" and prints the pin file. Check the new fingerprint on the board and
  pass `--pin` again, which replaces the pin.
- **To forget a pin**, delete its file. Every refusal prints the path; it is
  in `tpi/pins` under the cache directory (`~/.cache` on Linux,
  `~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows), one file per
  scheme, host and port. A board with a CA-signed certificate and no pin file
  is checked against the usual CA roots.
- **`--insecure`** (or `TPI_INSECURE=1`) skips the check as before. It is
  there for a lab, and not meant to stay in scripts: with it, the password
  goes to whoever answers.
- Over plain `http` (an old board) there is no certificate, so `--pin` does
  nothing and nothing is remembered.

## Installation

`tpi` is built and released by this fork. Its releases are at
<https://github.com/excavador-turing/tpi/releases>.

### Prebuilt binaries

Each release carries an archive per platform and a `SHA256SUMS` file:

- Linux: `tpi-x86_64-unknown-linux-gnu.tar.gz` and
  `tpi-aarch64-unknown-linux-gnu.tar.gz`. Unpack with
  `sudo tar -xzf <archive> -C /` to install under `/usr`, or take
  `usr/bin/tpi` out of it.
- macOS: `tpi-aarch64-apple-darwin.tar.gz` (Apple silicon) and
  `tpi-x86_64-apple-darwin.tar.gz` (Intel).
- Windows: `tpi.exe`.

Check what you downloaded against `SHA256SUMS` before you run it.

### Debian and Ubuntu

Each release has a `.deb` for `amd64` (`tpi-<version>-x86_64-linux.deb`) and
for `arm64` (`tpi-<version>-aarch64-linux.deb`):

```shell
sudo apt install ./tpi-<version>-x86_64-linux.deb
```

There is no apt repository for this fork.

### Cargo

Rust users can build and install from source:

```shell
cargo install --git https://github.com/excavador-turing/tpi
```

### Packages that are not this fork's

The `tpi` packages on the AUR, on crates.io (`cargo install tpi`), in
Nixpkgs and at `firmware.turingpi.com` (the prebuilt binaries and the apt
repository) are upstream's. They install upstream's `tpi`, not this fork's (the
AUR and crates.io are at 1.0.7). It lacks this fork's changes, such as
certificate checking and POST state changes, and it cannot change anything on a
board running bmcd 2.49.0 or later: that daemon refuses a state change over
GET.

### Building from source

```shell
git clone https://github.com/excavador-turing/tpi
cd tpi
cargo build --release
```

The binary is `target/release/tpi`.

