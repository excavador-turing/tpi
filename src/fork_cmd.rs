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
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::Client;

use crate::cli::{
    ConfigCmd, HostnameArgs, InstallArgs, ListArgs, NtpCmd, SourceAddArgs, SourceKindArg,
    SourcesCmd,
};
use crate::fork::{
    self, About, Relation, Source, SourceKind, Sources, SINCE_CONFIG, SINCE_FIRMWARE_CATALOGUE,
    SINCE_HOSTNAME, SINCE_NTP, SINCE_THERMAL,
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
    let mut catalog = fork::catalog(request, client, args.refresh).await?;

    // The daemon answers a refresh at once and re-polls the sources behind
    // itself. Right for a page, which draws a spinner; wrong for a shell
    // command that asked for a fresh answer and would otherwise print the
    // previous one. Wait for it, bounded, and say so on stderr so the wait is
    // not mistaken for a hang.
    if args.refresh && catalog.refreshing {
        eprintln!("polling the sources...");
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            catalog = fork::catalog(request, client, false).await?;
            if !catalog.refreshing {
                break;
            }
        }
    }

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

/// How long to wait for the board to re-poll its firmware sources.
///
/// Measured on a board on 2026-09-09: a poll spawned at 19:07:10 landed with a
/// `checked_at` of 19:08:28, so **78 seconds** over four sources -- where
/// `firmware_catalog`'s own comment says 16. `firmware.turingpi.com` is the
/// slow one. Three minutes leaves headroom without hanging a terminal.
const POLL_WAIT: Duration = Duration::from_secs(180);

/// How often to ask whether the poll has landed. Cheap: it reads the cache.
const POLL_STEP: Duration = Duration::from_secs(3);

/// Whether a poll started when the catalogue read `before` has landed.
///
/// `checked_at` advances exactly once, when a poll completes, which makes this
/// falsifiable: before the poll lands it still equals `before`. An empty
/// `before` means the daemon does not report the field at all, and then there
/// is nothing to watch -- say so rather than wait out the deadline.
fn poll_landed(before: &str, now: &str) -> bool {
    !before.is_empty() && now != before
}

/// Wait for a re-poll started with `refresh=1` to finish, and answer with what
/// it found.
///
/// **`refresh=1` does not fetch.** The daemon spawns the poll and returns the
/// cached list immediately, with `refreshing` set -- confirmed on a board,
/// where a forced request came back in one second carrying the same
/// `checked_at` it had before. So asking again with `refresh=1` and resolving
/// against the answer resolves against the same stale list, which is exactly
/// the bug this was meant to fix (SQU-184; tpi 1.5.0 shipped that mistake).
///
/// Returns the catalogue and whether the poll actually landed, so a refusal
/// can say which kind of refusal it is.
async fn await_poll(
    request: &Request,
    client: &Client,
    before: &str,
    started: fork::Catalog,
) -> Result<(fork::Catalog, bool)> {
    if before.is_empty() {
        return Ok((started, false));
    }
    let mut catalog = started;
    let deadline = Instant::now() + POLL_WAIT;
    loop {
        if poll_landed(before, &catalog.checked_at) {
            return Ok((catalog, true));
        }
        if Instant::now() >= deadline {
            return Ok((catalog, false));
        }
        tokio::time::sleep(POLL_STEP).await;
        catalog = fork::catalog(request, client, false).await?;
    }
}

/// Which (source, candidate) positions in `catalog` answer to what was asked
/// for.
///
/// Positions rather than references, so the caller can fetch the catalogue
/// again and re-run this against the new one without fighting the borrow the
/// first result would hold.
///
/// A leading `v` is optional on both sides: `firmware list` prints `v2.16.0`
/// and people type either.
fn matching(catalog: &fork::Catalog, args: &InstallArgs) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    for (si, s) in catalog.sources.iter().enumerate() {
        if let Some(want) = &args.source {
            if &s.id != want {
                continue;
            }
        }
        for (ci, c) in s.candidates.iter().enumerate() {
            if c.version == args.target
                || c.version.trim_start_matches('v') == args.target.trim_start_matches('v')
            {
                hits.push((si, ci));
            }
        }
    }
    hits
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
    // Resolve the version against the listing rather than posting it blind:
    // the board would accept a source/version pair that offers nothing and
    // fail later, in the middle of a download.
    let mut catalog = fork::catalog(request, client, false).await?;
    let mut hits = matching(&catalog, args);

    // The cached listing can be up to half an hour behind the sources, and
    // `firmware check` reads a different cache with its own timing -- so
    // `check` prints "install it with: tpi firmware install <v>" and this
    // resolution refuses the very command it printed. Ask for a re-poll and
    // WAIT for it before believing no source has the version. Only the path
    // that was about to fail wrongly pays for the wait.
    let mut polled = true;
    if hits.is_empty() {
        let before = catalog.checked_at.clone();
        if !json {
            println!(
                "{} is not in the board's cached listing; asking it to re-poll its sources \
                 (about a minute)",
                args.target
            );
        }
        // This starts the poll. It does NOT return its result -- see await_poll.
        catalog = fork::catalog(request, client, true).await?;
        (catalog, polled) = await_poll(request, client, &before, catalog).await?;
        hits = matching(&catalog, args);
    }

    let (source, candidate) = match hits.len() {
        0 if !polled => bail!(
            "no source offers {}, but the board's re-poll did not finish within {} s, so that \
             answer may be stale. Try `tpi firmware list --refresh --all`, then again.",
            args.target,
            POLL_WAIT.as_secs()
        ),
        0 => bail!(
            "no source offers {}. `tpi firmware list --all` shows what this board can install",
            args.target
        ),
        1 => {
            let (si, ci) = hits[0];
            (&catalog.sources[si], &catalog.sources[si].candidates[ci])
        }
        _ => {
            let ids: Vec<&str> = hits
                .iter()
                .map(|(si, _)| catalog.sources[*si].id.as_str())
                .collect();
            bail!(
                "{} is offered by several sources ({}); choose one with --source",
                args.target,
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

    // `{"sensors":[…],"cooling":[…]}`. This used to expect an array of
    // `{name, temp}` in millidegrees -- a shape the daemon has never sent --
    // and printed the raw JSON instead. It was never noticed because the
    // response unwrapping was broken too, so no fork command reached its
    // formatter at all.
    let sensors = value
        .get("sensors")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let cooling = value
        .get("cooling")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if sensors.is_empty() && cooling.is_empty() {
        // A v2.4 board has no fan, and an image without the sensor in its
        // device tree has no zone. That is a fact about the board, not a
        // failure, and it is different from a reading of zero.
        println!("this board reports no temperature sensors and no fan");
        return Ok(0);
    }

    for sensor in &sensors {
        let name = sensor
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("sensor");
        let Some(celsius) = sensor.get("temperature_c").and_then(|v| v.as_f64()) else {
            println!("{name:<14} could not be read");
            continue;
        };
        println!("{name:<14} {celsius:.1} C");

        // The governor is step_wise, so the fan's step follows the highest
        // `active` trip the board is above. Printing the trips without saying
        // which one is in force would leave the reader to work it out.
        let mut governing: Option<f64> = None;
        for trip in sensor
            .get("trips")
            .and_then(|v| v.as_array())
            .unwrap_or(&vec![])
        {
            if trip.get("kind").and_then(|v| v.as_str()) != Some("active") {
                continue;
            }
            let Some(at) = trip.get("temperature_c").and_then(|v| v.as_f64()) else {
                continue;
            };
            if celsius >= at && governing.is_none_or(|best| at > best) {
                governing = Some(at);
            }
        }
        match governing {
            Some(at) => println!("{:<14} above the {at:.0} C trip", ""),
            None => println!("{:<14} below every trip", ""),
        }
    }

    for cooler in &cooling {
        let name = cooler.get("name").and_then(|v| v.as_str()).unwrap_or("fan");
        let cur = cooler.get("cur_state").and_then(|v| v.as_u64());
        let max = cooler.get("max_state").and_then(|v| v.as_u64());
        match (cur, max) {
            (Some(cur), Some(max)) => {
                // Steps are an index, not a percentage: 4 of 6 is the fifth
                // of seven settings. The duty is what that step commands, out
                // of the top step's duty -- read from the board's own table,
                // because it is not linear.
                let duty = cooler
                    .get("levels")
                    .and_then(|v| v.as_array())
                    .and_then(|levels| levels.get(cur as usize))
                    .and_then(|v| v.as_u64())
                    .zip(cooler.get("max_level").and_then(|v| v.as_u64()))
                    .filter(|(_, top)| *top > 0)
                    .map(|(at, top)| (at as f64 / top as f64) * 100.0);
                match duty {
                    Some(pct) => println!("{name:<14} step {cur} of {max}  ({pct:.0}% duty)"),
                    None => println!("{name:<14} step {cur} of {max}"),
                }
            }
            _ => println!("{name:<14} could not be read"),
        }
    }

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

    let value = fork::set(request, client, &[("type", "hostname"), ("name", name)]).await?;
    if json {
        return emit_json(&value).map(|_| 0);
    }

    // Said after the board has accepted, not before. Printed first, it
    // announced a rename that the very next line then refused -- and a
    // warning about a consequence that did not happen is worse than no
    // warning. The consequence is still the point: by the time this prints
    // the series has already split, and renaming back does not rejoin it.
    println!("renamed to {name}");
    println!(
        "the metrics `instance` label changed with it, so a Prometheus history \
         will not follow this board across the rename"
    );
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
        ConfigCmd::Export { file } => {
            let value = fork::get(request, client, &[("type", "config")]).await?;
            let document = serde_json::to_string_pretty(&value)?;

            match file {
                Some(path) => {
                    std::fs::write(path, format!("{document}\n"))
                        .with_context(|| format!("cannot write {}", path.display()))?;
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

    fn catalog_of(sources: Vec<SourceCatalog>) -> crate::fork::Catalog {
        crate::fork::Catalog {
            refreshing: false,
            checked_at: String::new(),
            running: "v2.15.0".to_string(),
            sources,
        }
    }

    fn install_args(target: &str, source: Option<&str>) -> InstallArgs {
        InstallArgs {
            target: target.to_string(),
            source: source.map(str::to_string),
            yes: true,
            force: false,
        }
    }

    /// The catalogue is what decides whether an install is refused, so what
    /// counts as a match is worth pinning: either spelling of the leading `v`,
    /// and `--source` narrows rather than being advisory.
    #[test]
    fn a_version_is_found_with_or_without_its_v() {
        let mut s = source("fork", None);
        s.candidates = vec![candidate("v2.16.0", None)];
        let cat = catalog_of(vec![s]);

        assert_eq!(matching(&cat, &install_args("v2.16.0", None)), vec![(0, 0)]);
        assert_eq!(matching(&cat, &install_args("2.16.0", None)), vec![(0, 0)]);
        assert!(matching(&cat, &install_args("v2.15.0", None)).is_empty());
    }

    #[test]
    fn source_narrows_the_match() {
        let mut a = source("fork", None);
        a.candidates = vec![candidate("v2.16.0", None)];
        let mut b = source("mirror", None);
        b.candidates = vec![candidate("v2.16.0", None)];
        let cat = catalog_of(vec![a, b]);

        assert_eq!(matching(&cat, &install_args("v2.16.0", None)).len(), 2);
        assert_eq!(
            matching(&cat, &install_args("v2.16.0", Some("mirror"))),
            vec![(1, 0)]
        );
    }

    /// SQU-184, the second attempt. 1.5.0 asked for a re-poll and resolved
    /// against the answer, which is the *cached* list -- the daemon spawns the
    /// poll and returns at once. So the wait is the fix, and this is its
    /// predicate: it must stay false until `checked_at` moves.
    #[test]
    fn a_poll_has_not_landed_until_checked_at_moves() {
        let before = "2026-09-09T19:50:40Z";

        assert!(
            !poll_landed(before, before),
            "the same timestamp means the poll is still running; \
             treating it as landed is exactly the 1.5.0 bug"
        );
        assert!(poll_landed(before, "2026-09-09T19:59:43Z"));
    }

    /// A daemon that does not report `checked_at` gives nothing to watch, so
    /// the wait must give up at once rather than burn the whole deadline.
    #[test]
    fn nothing_to_watch_is_not_a_landed_poll() {
        assert!(!poll_landed("", ""));
        assert!(!poll_landed("", "2026-09-09T19:59:43Z"));
    }

    /// The deadline is a measurement, not a guess: a poll over four sources
    /// took 78 s on the board on 2026-09-09, against the 16 s
    /// `firmware_catalog` claims. Anyone shrinking this below what the board
    /// actually needs reintroduces the wrong refusal, silently.
    #[test]
    fn the_deadline_clears_the_measured_fan_out() {
        assert!(
            POLL_WAIT.as_secs() >= 120,
            "measured 78 s over four sources; leave real headroom"
        );
        assert!(POLL_STEP < POLL_WAIT);
    }

    /// SQU-184: a stale catalogue made `firmware install` refuse the command
    /// `firmware check` had just printed. The fix re-resolves against a forced
    /// poll, so what matters is that the same question asked of a fresher
    /// catalogue gives a different answer -- and that the positions returned
    /// index the catalogue they were computed from, since the caller swaps it.
    #[test]
    fn a_refreshed_catalogue_resolves_what_the_stale_one_could_not() {
        let args = install_args("v2.16.0", None);

        let mut stale_source = source("fork", None);
        stale_source.candidates = vec![candidate("v2.15.0", None)];
        let stale = catalog_of(vec![stale_source]);
        assert!(
            matching(&stale, &args).is_empty(),
            "the stale catalogue is what produced the wrong refusal"
        );

        let mut fresh_source = source("fork", None);
        fresh_source.candidates = vec![candidate("v2.15.0", None), candidate("v2.16.0", None)];
        let fresh = catalog_of(vec![fresh_source]);

        let hits = matching(&fresh, &args);
        assert_eq!(hits, vec![(0, 1)]);
        let (si, ci) = hits[0];
        assert_eq!(fresh.sources[si].candidates[ci].version, "v2.16.0");
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
