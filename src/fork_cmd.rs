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

use anyhow::{bail, Result};
use reqwest::Client;

use crate::cli::{InstallArgs, ListArgs, MetricsCmd, SourceAddArgs, SourceKindArg, SourcesCmd};
use crate::fork::{
    self, About, Relation, Source, SourceKind, Sources, SINCE_FIRMWARE_CATALOGUE,
    SINCE_METRICS_TOKEN, SINCE_THERMAL,
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
    let mut matches: Vec<(&str, &crate::fork::Candidate)> = Vec::new();
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
                matches.push((&s.id, c));
            }
        }
    }

    let (source_id, candidate) = match matches.len() {
        0 => bail!(
            "no source offers {}. `tpi firmware list --all` shows what this board can install",
            args.version
        ),
        1 => matches[0],
        _ => {
            let ids: Vec<&str> = matches.iter().map(|(id, _)| *id).collect();
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
            source_id,
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

    let mut pairs: Vec<(&str, &str)> = vec![
        ("type", "firmware_install"),
        ("source", source_id),
        ("version", &candidate.version),
    ];
    if args.force {
        pairs.push(("force", "1"));
    }

    let response = fork::set(request, client, &pairs).await?;
    if json {
        return emit_json(&response).map(|_| 0);
    }

    println!("staged {}; reboot to take it", candidate.version);
    Ok(0)
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
