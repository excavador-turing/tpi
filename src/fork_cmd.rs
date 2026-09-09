// Copyright 2026 excavador
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

//! Commands for the fork's endpoints.
//!
//! These do not fit upstream's one-request-per-command shape: each first
//! asks the board what version it is (so a board a release behind gets a
//! sentence instead of `Invalid \`type\` parameter`), and several then make
//! a second call or assemble a table from a list.

use std::io::{IsTerminal, Write};

use anyhow::{bail, Context, Result};
use reqwest::Client;

use crate::cli::{
    ConfigCmd, HostnameArgs, InstallArgs, ListArgs, MetricsCmd, NtpCmd, SourceAddArgs,
    SourceKindArg, SourcesCmd,
};
use crate::fork::{
    self, About, Relation, Source, SourceKind, Sources, SINCE_CONFIG, SINCE_FIRMWARE_CATALOGUE,
    SINCE_HOSTNAME, SINCE_METRICS_TOKEN, SINCE_NTP, SINCE_THERMAL,
};
use crate::request::Request;

/// `tpi firmware check` exits with this when an upgrade exists, so
/// `tpi firmware check || notify-me` works from cron without parsing text.
/// Distinct from 1, which stays "the command failed".
pub const EXIT_UPDATE_AVAILABLE: u8 = 10;

impl From<SourceKindArg> for SourceKind {
    fn from(k: SourceKindArg) -> Self {
        match k {
            SourceKindArg::Github => SourceKind::Github,
            SourceKindArg::Http => SourceKind::Http,
            SourceKindArg::Local => SourceKind::Local,
        }
    }
}

async fn gate(request: &Request, client: &Client, command: &str, since: &str) -> Result<About> {
    let about = fork::about(request, client).await?;
    fork::require(&about, command, since)?;
    Ok(about)
}

fn emit_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub async fn list(request: &Request, client: &Client, args: &ListArgs, json: bool) -> Result<u8> {
    gate(request, client, "firmware list", SINCE_FIRMWARE_CATALOGUE).await?;
    let catalog = fork::catalog(request, client, args.refresh).await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::to_value(serde_json::json!({
                "checked_at": catalog.checked_at,
                "running": catalog.running,
                "sources": catalog.sources.iter().map(|s| serde_json::json!({
                    "id": s.id, "label": s.label, "location": s.location,
                    "error": s.error,
                    "candidates": s.candidates.iter().map(|c| serde_json::json!({
                        "version": c.version, "relation": format!("{:?}", c.relation).to_lowercase(),
                        "trust": c.trust.label(), "prerelease": c.prerelease,
                        "size_bytes": c.size_bytes, "file": c.file,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            }))?)?
        );
        return Ok(0);
    }

    println!("running {}", catalog.running);

    // What the board is about to do, and what it last did.
    //
    // Without this a shell user has no way to learn that a rollback happened
    // at all: the gate rejects an image, the board reboots onto the old one,
    // and `firmware list` would cheerfully report the old version as running
    // with nothing to say an install had been attempted and refused.
    if let Ok(slots) = fork::slots(request, client).await {
        if let Some(staged) = slots
            .get("staged")
            .and_then(|s| s.get("version"))
            .and_then(|v| v.as_str())
        {
            println!("staged  {staged}  (reboot to take it)");
        }
        if let Some(promotion) = slots.get("last_promotion") {
            let message = promotion
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // The gate writes its refusals with this prefix, and a refusal is
            // the line worth putting in front of somebody.
            if message.starts_with("FAILED") || message.contains("rolling back") {
                println!("last boot  {message}");
            }
        }
    }

    let mut rows: Vec<[String; 4]> = Vec::new();
    let mut hidden = 0usize;

    for source in &catalog.sources {
        if let Some(err) = &source.error {
            // Per source, never fatal: one unreachable mirror must not hide
            // the versions the others can still offer.
            println!("  {:<14} unreachable: {}", source.id, err);
            continue;
        }

        for c in &source.candidates {
            let interesting = matches!(c.relation, Relation::Newer | Relation::Current);
            if !args.all && !interesting {
                hidden += 1;
                continue;
            }

            let mut version = format!("{} {}", c.relation.marker(), c.version);
            if c.prerelease {
                version.push_str(" (pre)");
            }

            rows.push([
                version,
                source.id.clone(),
                c.trust.label().to_string(),
                c.size_bytes
                    .map(|b| format!("{:.1} MB", b as f64 / 1_048_576.0))
                    .or_else(|| c.file.clone())
                    .unwrap_or_default(),
            ]);
        }
    }

    if rows.is_empty() {
        println!("no installable versions found");
        if hidden > 0 {
            println!("{hidden} older or unrelated version(s) hidden; pass --all to see them");
        }
        return Ok(0);
    }

    let headers = ["VERSION", "SOURCE", "TRUST", "SIZE"];
    let mut width = headers.map(str::len);
    for r in &rows {
        for (i, cell) in r.iter().enumerate() {
            width[i] = width[i].max(cell.len());
        }
    }

    println!();
    for (i, h) in headers.iter().enumerate() {
        print!("{:<w$}  ", h, w = width[i]);
    }
    println!();
    for r in &rows {
        for (i, cell) in r.iter().enumerate() {
            print!("{:<w$}  ", cell, w = width[i]);
        }
        println!();
    }

    if hidden > 0 {
        println!("\n{hidden} older or unrelated version(s) hidden; pass --all to see them");
    }
    println!("\n^ newer   = running   v older   ? not comparable");

    Ok(0)
}

pub async fn check(request: &Request, client: &Client, json: bool) -> Result<u8> {
    gate(request, client, "firmware check", SINCE_FIRMWARE_CATALOGUE).await?;
    let check = fork::update_check(request, client).await?;

    if json {
        return emit_json(&serde_json::json!({
            "running": check.running,
            "target": check.target,
            "update_available": check.update_available,
            "repo": check.repo,
            "error": check.error,
        }))
        .map(|_| {
            if check.update_available {
                EXIT_UPDATE_AVAILABLE
            } else {
                0
            }
        });
    }

    if let Some(err) = &check.error {
        bail!("could not check for updates: {err}");
    }

    if check.update_available {
        println!(
            "{} is available; this board runs {}",
            check.target.as_deref().unwrap_or("a newer version"),
            check.running
        );
        println!(
            "install it with: tpi firmware install {}",
            check.target.as_deref().unwrap_or("<version>")
        );
        return Ok(EXIT_UPDATE_AVAILABLE);
    }

    println!("{} is current", check.running);
    Ok(0)
}

pub async fn install(
    request: &Request,
    client: &Client,
    args: &InstallArgs,
    json: bool,
) -> Result<u8> {
    gate(
        request,
        client,
        "firmware install",
        SINCE_FIRMWARE_CATALOGUE,
    )
    .await?;
    let catalog = fork::catalog(request, client, false).await?;

    // Resolve the version against the listing rather than posting it blind:
    // the board would accept a source/version pair that offers nothing and
    // fail later, in the middle of a download.
    let mut matches: Vec<(&crate::fork::SourceCatalog, &crate::fork::Candidate)> = Vec::new();
    for s in &catalog.sources {
        if let Some(want) = &args.source {
            if &s.id != want {
                continue;
            }
        }
        for c in &s.candidates {
            if c.version == args.version
                || c.version.trim_start_matches('v') == args.version.trim_start_matches('v')
            {
                matches.push((s, c));
            }
        }
    }

    let (source, candidate) = match matches.len() {
        0 => bail!(
            "no source offers {}. `tpi firmware list --all` shows what this board can install",
            args.version
        ),
        1 => matches[0],
        _ => {
            let ids: Vec<&str> = matches.iter().map(|(s, _)| s.id.as_str()).collect();
            bail!(
                "{} is offered by several sources ({}); choose one with --source",
                args.version,
                ids.join(", ")
            )
        }
    };

    if candidate.relation == Relation::Current && !args.force {
        println!("{} is already running; nothing to do", candidate.version);
        return Ok(0);
    }

    if !args.yes && !json {
        let direction = match candidate.relation {
            Relation::Newer => "upgrade",
            Relation::Older => "DOWNGRADE",
            Relation::Current => "reinstall",
            Relation::Unknown => "install (not comparable with the running build)",
        };
        println!(
            "{direction} {} -> {} from {} ({})",
            catalog.running,
            candidate.version,
            source.id,
            candidate.trust.label()
        );

        if !std::io::stdin().is_terminal() {
            bail!("refusing to install without confirmation; pass --yes for a non-interactive run");
        }

        print!("continue? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("cancelled");
            return Ok(0);
        }
    }

    let pairs = install_request(source, candidate, args.force)?;
    let local = source.kind == Some(SourceKind::Local);

    let response = fork::set(
        request,
        client,
        &pairs
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect::<Vec<_>>(),
    )
    .await?;
    if json {
        return emit_json(&response).map(|_| 0);
    }

    // The transfer endpoint answers with a handle and writes in the
    // background. Without following it, this would print "staged" while the
    // write was still running, and a reboot would land on a half-written
    // image. `firmware_install` blocks until the updater is done, so it needs
    // none of this.
    if local {
        let handle = response
            .get("handle")
            .and_then(serde_json::Value::as_u64)
            .context("the board accepted the image but named no transfer to follow")?;
        crate::legacy_handler::LegacyHandler::watch_flash_progress(request, client, handle)
            .await
            .context("waiting for the flash to finish")?;
    }

    println!("staged {}; reboot to take it", candidate.version);
    Ok(0)
}

/// Builds the query for installing one resolved candidate.
///
/// Split out so the choice between the two endpoints is testable without a
/// board. It is not a detail: an image already on the board is not fetched, so
/// it does not go through `firmware_install`, and the daemon refuses that pair
/// by design -- *"a local image is installed through opt=set&type=firmware
/// with local=1"*. Posting the wrong one is a 400 at the end of a resolution
/// that otherwise worked, which is exactly what 1.1.0 did.
fn install_request(
    source: &crate::fork::SourceCatalog,
    candidate: &crate::fork::Candidate,
    force: bool,
) -> Result<Vec<(&'static str, String)>> {
    let mut pairs: Vec<(&'static str, String)> = if source.kind == Some(SourceKind::Local) {
        let Some(file) = candidate.file.as_deref() else {
            bail!(
                "{} is listed by {} but the board did not say which file it is",
                candidate.version,
                source.id
            )
        };
        vec![
            ("type", "firmware".to_string()),
            ("local", "1".to_string()),
            ("file", file.to_string()),
        ]
    } else {
        vec![
            ("type", "firmware_install".to_string()),
            ("source", source.id.clone()),
            ("version", candidate.version.clone()),
        ]
    };

    // Both endpoints refuse when something is already staged, for the same
    // reason -- the updater writes into the volume nextboot points at -- and
    // both take the same escape.
    if force {
        pairs.push(("force", "1".to_string()));
    }
    Ok(pairs)
}

pub async fn sources(
    request: &Request,
    client: &Client,
    cmd: &SourcesCmd,
    json: bool,
) -> Result<u8> {
    gate(
        request,
        client,
        "firmware sources",
        SINCE_FIRMWARE_CATALOGUE,
    )
    .await?;
    let mut current = fork::sources(request, client).await?;

    match cmd {
        SourcesCmd::List => {
            if json {
                return emit_json(&current).map(|_| 0);
            }
            print_sources(&current);
            return Ok(0);
        }
        SourcesCmd::Add(add) => add_source(&mut current, add)?,
        SourcesCmd::Remove { id } => {
            let before = current.sources.len();
            current.sources.retain(|s| &s.id != id);
            if current.sources.len() == before {
                bail!("no source called {id:?}");
            }
        }
        SourcesCmd::Enable { id } => set_enabled(&mut current, id, true)?,
        SourcesCmd::Disable { id } => set_enabled(&mut current, id, false)?,
    }

    let body = serde_json::to_string(&current)?;
    let response = fork::set(
        request,
        client,
        &[("type", "firmware_sources"), ("sources", &body)],
    )
    .await?;

    if json {
        return emit_json(&response).map(|_| 0);
    }

    print_sources(&current);
    Ok(0)
}

fn add_source(current: &mut Sources, add: &SourceAddArgs) -> Result<()> {
    if current.sources.iter().any(|s| s.id == add.id) {
        bail!(
            "a source called {:?} already exists; remove it first, or pick another id",
            add.id
        );
    }

    current.sources.push(Source {
        id: add.id.clone(),
        kind: add.kind.into(),
        label: add.label.clone().unwrap_or_else(|| add.id.clone()),
        location: add.location.clone(),
        enabled: true,
    });

    Ok(())
}

fn set_enabled(current: &mut Sources, id: &str, enabled: bool) -> Result<()> {
    let Some(s) = current.sources.iter_mut().find(|s| s.id == id) else {
        bail!("no source called {id:?}");
    };
    s.enabled = enabled;
    Ok(())
}

fn print_sources(sources: &Sources) {
    let width = sources
        .sources
        .iter()
        .map(|s| s.id.len())
        .max()
        .unwrap_or(2)
        .max(2);

    for s in &sources.sources {
        println!(
            "{:<width$}  {:<7} {:<9} {}",
            s.id,
            s.kind.label(),
            if s.enabled { "enabled" } else { "disabled" },
            s.location,
            width = width
        );
    }
}

/// What is actually on this board.
///
/// `tpi info` reports the running system's storage and interfaces; this
/// reports the software identity -- which firmware, which bmcd, which
/// kernel. The kernel line in particular had no answer before bmcd added
/// it: you had to SSH in and run `uname -r`.
pub async fn about(request: &Request, client: &Client, json: bool) -> Result<u8> {
    let about = fork::about(request, client).await?;

    if json {
        return emit_json(&serde_json::json!({
            "hostname": about.hostname,
            "board_model": about.board_model,
            "firmware": about.version,
            "buildroot": about.buildroot,
            "bmcd": about.bmcd_version,
            "kernel": about.kernel,
        }))
        .map(|_| 0);
    }

    let row = |k: &str, v: &str| {
        if !v.is_empty() {
            println!("{k:<12} {v}");
        }
    };

    row("hostname", &about.hostname);
    row("board", &about.board_model);
    row("firmware", &about.version);
    row("buildroot", &about.buildroot);
    row("bmcd", &about.bmcd_version);
    row("kernel", &about.kernel);

    Ok(0)
}

pub async fn thermal(request: &Request, client: &Client, json: bool) -> Result<u8> {
    gate(request, client, "thermal", SINCE_THERMAL).await?;
    let value = fork::get(request, client, &[("type", "thermal")]).await?;

    if json {
        return emit_json(&value).map(|_| 0);
    }

    match &value {
        serde_json::Value::Array(items) => {
            for item in items {
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("sensor");
                let temp = item.get("temp").and_then(|v| v.as_f64());
                match temp {
                    // bmcd reports millidegrees, as the kernel does.
                    Some(t) => println!("{name:<12} {:.1} C", t / 1000.0),
                    None => println!("{name:<12} unknown"),
                }
            }
        }
        other => println!("{other}"),
    }

    Ok(0)
}

pub async fn metrics_cmd(
    request: &Request,
    client: &Client,
    cmd: &MetricsCmd,
    json: bool,
) -> Result<u8> {
    gate(request, client, "metrics", SINCE_METRICS_TOKEN).await?;

    let value = match cmd {
        MetricsCmd::Show => fork::get(request, client, &[("type", "metrics_token")]).await?,
        MetricsCmd::Rotate => fork::set(request, client, &[("type", "metrics_token")]).await?,
    };

    if json {
        return emit_json(&value).map(|_| 0);
    }

    let token = value
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if token.is_empty() {
        println!("{value}");
        return Ok(0);
    }

    // The username is not a Unix account and people guess it wrong, so it is
    // printed beside the token rather than left to the docs.
    println!("username  metrics");
    println!("token     {token}");
    Ok(0)
}

/// `tpi hostname [<name>]`
///
/// Printing and setting are one command because they are one question, and a
/// separate `hostname show` would be a subcommand whose only job is to be
/// typed.
pub async fn hostname_cmd(
    request: &Request,
    client: &Client,
    args: &HostnameArgs,
    json: bool,
) -> Result<u8> {
    gate(request, client, "hostname", SINCE_HOSTNAME).await?;

    let Some(name) = args.name.as_deref() else {
        let value = fork::get(request, client, &[("type", "hostname")]).await?;
        if json {
            return emit_json(&value).map(|_| 0);
        }
        let live = value.get("hostname").and_then(|v| v.as_str());
        let next = value.get("on_next_boot").and_then(|v| v.as_str());
        println!("{}", live.unwrap_or("(unreadable)"));
        // Only when they disagree, which happens when someone has run
        // `hostname` by hand. Printing it always would be noise on every board
        // that is fine.
        if let (Some(live), Some(next)) = (live, next) {
            if live != next {
                println!("after the next reboot: {next}");
            }
        }
        return Ok(0);
    };

    // Said before it happens, not after: the series break is the part nobody
    // expects, and it is not undone by renaming the board back.
    if !json {
        println!(
            "renaming to {name}. The metrics `instance` label changes with it,              so a Prometheus history will not follow the board across the rename."
        );
    }

    let value = fork::set(request, client, &[("type", "hostname"), ("name", name)]).await?;
    if json {
        return emit_json(&value).map(|_| 0);
    }
    println!("renamed to {name}");
    Ok(0)
}

/// `tpi ntp show | set <servers...>`
pub async fn ntp_cmd(
    request: &Request,
    client: &Client,
    cmd: Option<&NtpCmd>,
    json: bool,
) -> Result<u8> {
    gate(request, client, "ntp", SINCE_NTP).await?;

    if let Some(NtpCmd::Set { servers }) = cmd {
        let joined = servers.join(",");
        let value = fork::set(request, client, &[("type", "ntp"), ("servers", &joined)]).await?;
        if json {
            return emit_json(&value).map(|_| 0);
        }
        if servers.is_empty() {
            println!("cleared; the board is back to the pool its image ships with");
        } else {
            println!(
                "{} server(s) set; {} is preferred",
                servers.len(),
                servers[0]
            );
        }
        return Ok(0);
    }

    let value = fork::get(request, client, &[("type", "ntp")]).await?;
    if json {
        return emit_json(&value).map(|_| 0);
    }

    // A saved list that is never read is the one failure this cannot show by
    // printing the servers, so it is said outright.
    if value
        .get("configurable")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
    {
        println!(
            "this board's firmware cannot take a server list: its chrony config              has no `sourcedir` line. Upgrade the firmware first."
        );
    }

    match value.get("servers").and_then(|v| v.as_array()) {
        Some(servers) if !servers.is_empty() => {
            for (index, server) in servers.iter().enumerate() {
                let name = server.as_str().unwrap_or_default();
                if index == 0 {
                    println!("{name}  (preferred)");
                } else {
                    println!("{name}");
                }
            }
        }
        _ => println!("no servers configured; the image's own pool is the only source"),
    }

    if let Some(clock) = value.get("clock") {
        let synchronised = clock
            .get("synchronised")
            .and_then(serde_json::Value::as_bool);
        let source = clock.get("source").and_then(|v| v.as_str());
        let stratum = clock.get("stratum").and_then(serde_json::Value::as_u64);
        match (synchronised, source) {
            (Some(true), Some(source)) => {
                let stratum = stratum
                    .map(|s| format!(", stratum {s}"))
                    .unwrap_or_default();
                println!("\nsynchronised to {source}{stratum}");
            }
            (Some(false), _) => println!("\nNOT synchronised"),
            _ => println!("\nthe clock's state could not be read"),
        }
    }
    Ok(0)
}

/// `tpi config export | import`
pub async fn config_cmd(
    request: &Request,
    client: &Client,
    cmd: &ConfigCmd,
    json: bool,
) -> Result<u8> {
    gate(request, client, "config", SINCE_CONFIG).await?;

    match cmd {
        ConfigCmd::Export { file, with_secrets } => {
            let mut pairs: Vec<(&str, &str)> = vec![("type", "config")];
            if *with_secrets {
                pairs.push(("secrets", "1"));
            }
            let value = fork::get(request, client, &pairs).await?;
            let document = serde_json::to_string_pretty(&value)?;

            match file {
                Some(path) => {
                    std::fs::write(path, format!("{document}\n"))
                        .with_context(|| format!("cannot write {}", path.display()))?;
                    // Said on the way out, because the file is now a
                    // credential and nothing about its name says so.
                    if *with_secrets {
                        eprintln!(
                            "{} contains the metrics token: treat it as a credential",
                            path.display()
                        );
                    }
                    if !json {
                        println!("written to {}", path.display());
                    }
                }
                // No warning line here: stdout is very likely being piped
                // somewhere, and a stray sentence would land in the file.
                None => println!("{document}"),
            }
            Ok(0)
        }

        ConfigCmd::Import { file, yes } => {
            let document = std::fs::read_to_string(file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            // Parsed here rather than posted blind, so a file that is not an
            // export fails before anything on the board changes.
            let parsed: serde_json::Value = serde_json::from_str(&document)
                .with_context(|| format!("{} is not a config export", file.display()))?;

            if !yes && !json {
                let from = parsed
                    .get("exported_from")
                    .and_then(|o| o.get("hostname"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("an unnamed board");
                let at = parsed
                    .get("exported_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("an unknown time");
                println!("applying the settings exported from {from} at {at}");
                if parsed
                    .get("contains_secrets")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                {
                    println!("this export carries the metrics token; it will replace this board's");
                }
                if !std::io::stdin().is_terminal() {
                    bail!("refusing to import without confirmation; pass --yes for a non-interactive run");
                }
                print!("continue? [y/N] ");
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    println!("cancelled");
                    return Ok(0);
                }
            }

            let value = fork::set(
                request,
                client,
                &[("type", "config"), ("config", &document)],
            )
            .await?;
            if json {
                return emit_json(&value).map(|_| 0);
            }

            // Per field, because the import is not transactional and a single
            // "done" would hide a hostname that took and sources that did not.
            let mut failed = false;
            for (key, label) in [
                ("applied", "applied"),
                ("skipped", "skipped"),
                ("failed", "FAILED"),
            ] {
                if let Some(items) = value.get(key).and_then(|v| v.as_array()) {
                    for item in items {
                        println!("{label:>8}  {}", item.as_str().unwrap_or_default());
                    }
                    if key == "failed" && !items.is_empty() {
                        failed = true;
                    }
                }
            }
            // A partial apply is not a success. Exiting 0 would let a script
            // move on from a board that is half configured.
            Ok(if failed { 1 } else { 0 })
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fork::{Candidate, SourceCatalog, Trust};

    fn source(id: &str, kind: Option<SourceKind>) -> SourceCatalog {
        SourceCatalog {
            id: id.to_string(),
            label: id.to_string(),
            kind,
            location: "somewhere".to_string(),
            candidates: Vec::new(),
            error: None,
        }
    }

    fn candidate(version: &str, file: Option<&str>) -> Candidate {
        Candidate {
            version: version.to_string(),
            relation: Relation::Newer,
            prerelease: false,
            trust: Trust::Unverified,
            file: file.map(str::to_string),
            size_bytes: None,
        }
    }

    /// The bug in 1.1.0: an image already on the board was posted to
    /// `firmware_install`, which the daemon refuses for a local source. The
    /// resolution succeeded and the install failed with a 400.
    #[test]
    fn a_local_image_goes_through_the_transfer_endpoint() {
        let s = source("local", Some(SourceKind::Local));
        let c = candidate(
            "v2.8.1-rc1",
            Some("/mnt/sdcard/firmware/tp2-bmc-firmware-ota-v2.8.1-rc1.tpu"),
        );
        let pairs = install_request(&s, &c, false).expect("builds");

        assert_eq!(pairs[0], ("type", "firmware".to_string()));
        assert_eq!(pairs[1], ("local", "1".to_string()));
        assert_eq!(
            pairs[2],
            (
                "file",
                "/mnt/sdcard/firmware/tp2-bmc-firmware-ota-v2.8.1-rc1.tpu".to_string()
            )
        );
        assert!(
            !pairs.iter().any(|(k, _)| *k == "version"),
            "the transfer endpoint takes a path, not a version: {pairs:?}"
        );
    }

    #[test]
    fn a_remote_release_still_goes_through_firmware_install() {
        let s = source("fork", Some(SourceKind::Github));
        let c = candidate("v2.9.0", None);
        let pairs = install_request(&s, &c, false).expect("builds");

        assert_eq!(pairs[0], ("type", "firmware_install".to_string()));
        assert_eq!(pairs[1], ("source", "fork".to_string()));
        assert_eq!(pairs[2], ("version", "v2.9.0".to_string()));
        assert!(!pairs.iter().any(|(k, _)| *k == "local"));
    }

    /// A daemon too old to report `kind` cannot be a local source anyway --
    /// the catalogue that introduced local sources introduced the field with
    /// it -- so absent must read as "not local" rather than as a guess.
    #[test]
    fn an_unstated_kind_is_not_treated_as_local() {
        let s = source("fork", None);
        let c = candidate("v2.9.0", None);
        let pairs = install_request(&s, &c, false).expect("builds");
        assert_eq!(pairs[0], ("type", "firmware_install".to_string()));
    }

    /// Both endpoints refuse when an image is already staged, and both take
    /// the same escape. Forgetting it on one of them would make `--force`
    /// silently mean nothing for parked images.
    #[test]
    fn force_reaches_both_endpoints() {
        let local = source("local", Some(SourceKind::Local));
        let remote = source("fork", Some(SourceKind::Github));
        let c = candidate("v2.9.0", Some("/mnt/sdcard/firmware/x.tpu"));

        for s in [&local, &remote] {
            let pairs = install_request(s, &c, true).expect("builds");
            assert_eq!(
                pairs.last(),
                Some(&("force", "1".to_string())),
                "force missing for {}",
                s.id
            );
        }
    }

    /// A local candidate the board listed without a path cannot be installed,
    /// and saying so beats posting a request with an empty `file`.
    #[test]
    fn a_local_candidate_with_no_path_is_refused_by_name() {
        let s = source("local", Some(SourceKind::Local));
        let c = candidate("v2.8.1-rc1", None);
        let err = install_request(&s, &c, false).expect_err("must refuse");
        let message = format!("{err}");
        assert!(message.contains("v2.8.1-rc1"), "{message}");
        assert!(message.contains("local"), "{message}");
    }
}
