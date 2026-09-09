// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod cli;
mod fork;
mod fork_cmd;
mod legacy_handler;
mod prompt;
mod request;

use crate::legacy_handler::LegacyHandler;
use clap::{CommandFactory, Parser};
use clap_complete::generate;
use cli::Cli;
use std::{io, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(shell) = cli.gencompletion {
        generate(
            shell,
            &mut Cli::command(),
            env!("CARGO_PKG_NAME"),
            &mut io::stdout(),
        );
        return ExitCode::SUCCESS;
    }

    match execute_cli_command(&cli).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            // stderr, not stdout: an error printed to stdout is swallowed by
            // `tpi --json ... | jq` and shows up as a parse failure instead.
            if let Some(error) = e.downcast_ref::<reqwest::Error>() {
                eprintln!("{error}");
            } else {
                eprintln!("{:#}", e);
            }
            ExitCode::FAILURE
        }
    }
}

async fn execute_cli_command(cli: &Cli) -> anyhow::Result<u8> {
    let command = cli.command.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "subcommand must be specified!\n\n{}",
            Cli::command().render_long_help()
        )
    })?;

    let host = url::Host::parse(cli.host.as_ref().expect("host has a default set"))
        .map_err(|_| anyhow::anyhow!("please enter a valid hostname"))?;
    let mut host = host.to_string();

    // A name that does not resolve is the likeliest reason a first run fails,
    // and reqwest reports it as "error sending request for url (...)", which
    // names the URL and not the problem. Check it here, and only for the
    // default: an address the operator typed deserves the real error, not a
    // guess about DNS.
    //
    // The name is silent on firmware v2.8.0 and earlier of this fork -- the
    // rootfs-headroom pass dropped avahi and nothing replaced it until v2.8.1
    // -- so this fires most often against a board that is running perfectly.
    if host == cli::DEFAULT_HOST_NAME
        && tokio::net::lookup_host((host.as_str(), 443u16))
            .await
            .is_err()
    {
        anyhow::bail!(
            "{host} did not resolve.\n\n\
             It is the default because the board advertises it over mDNS, but \
             firmware v2.8.0 and earlier of this fork ship without an mDNS \
             responder; v2.8.1 restores it.\n\n\
             Give the address instead: `--host <address>`, or set TPI_HOSTNAME."
        );
    }

    // connect to specific port if specified.
    if let Some(port) = cli.port {
        host.push_str(&format!(":{}", port));
    }

    LegacyHandler::new(host, cli)?.handle_cmd(command).await
}
