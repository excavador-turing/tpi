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

`tpi` binaries can be downloaded from various channels depending on
which OS you are running. Choose one of the following:

### pre build 

prebuild binaries can be downloaded from the following URL:
https://firmware.turingpi.com/tpi

We have binaries for Windows, Mac and Linux

### Debian

Add turing-machines debian repository to your sources.list once:

```shell
deb https://firmware.turingpi.com/tpi/debian ./ >> /etc/apt/sources.list
sudo apt-get update
```

You can now install the tpi package with command:

```shell
sudo apt-get install tpi
```

### Cargo

Rust users can install from source using the following command:

```shell
cargo install tpi
```

### Arch User Repository (AUR)

The package is available in the [AUR](https://aur.archlinux.org/packages/tpi).
Use your favourite AUR helper to install:

e.g. with yay:

```shell
yay -S tpi
```

### NixOS

The package is available in [Nixpkgs](https://search.nixos.org/packages?show=tpi). To try it out, you can run `tpi` in a nix-shell:

```
nix-shell -p tpi
```

### getting Master

The GitHub action runner builds Windows, Linux and macOS packages on every
commit to `master`. Take a look at the artifacts of any [workflow
runs](https://github.com/turing-machines/tpi/actions/workflows/build.yml) to
download them.

