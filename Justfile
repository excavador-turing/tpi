# What CI runs, runnable here.
#
# Every recipe is the exact command from .github/workflows/validation.yml.
# A recipe that only approximates CI tells you nothing when it passes.
#
# The toolchain comes from rust-toolchain.toml -- 1.98.1, the same version the
# firmware's Buildroot builds the on-board copy with. Naming a version here
# would give the repository two answers.

default:
    @just --list

# Everything CI checks, in CI's order — run before pushing
check: fmt clippy test

# Formatting, as CI checks it
fmt:
    cargo fmt --all --check

# Reformat in place; `check` is what gates, this is what fixes
fix:
    cargo fmt --all

# Lints, denied as warnings. --all-features so the `localhost` build -- the
# copy that ships inside the firmware and talks to 127.0.0.1 without
# credentials -- is linted too. It is the build nobody runs by hand.
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Tests, both builds
test:
    cargo test --all-features

# Licences, bans and sources
deny:
    cargo deny check bans licenses sources

# The workstation binary, as a release would build it
build:
    cargo build --release

# The copy that ships in the firmware: no credentials, talks to loopback
build-onboard:
    cargo build --release --features localhost,native-tls
