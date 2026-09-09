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
        let mut landed = false;
        for _ in 0..POLL_ATTEMPTS {
            tokio::time::sleep(POLL_STEP).await;
            catalog = fork::catalog(request, client, false).await?;
            if !catalog.refreshing {
                landed = true;
                break;
            }
        }
        // Saying nothing here is how `--refresh` came to be able to lie: the
        // loop gave up and printed the previous listing, indistinguishable
        // from a current one.
        if !landed {
            eprintln!(
                "the board is still polling after {} s; the listing below is as of {}",
                (POLL_ATTEMPTS * POLL_STEP.as_secs()),
                catalog.checked_at
            );
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
    println!("listing checked {}", age_phrase(catalog.age_seconds));

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

/// How long `--refresh` waits for the board's poll, and how often it looks.
///
/// The bound was 30 attempts of 2 s. Polls timed on a board on 2026-09-09 took
/// 74 s, 78 s and 140 s, so a minute was short of the ordinary case and less
/// than half the worst -- and exhausting it printed the stale listing with
/// nothing said. Four minutes clears every figure measured; the loop says so
/// when it gives up.
const POLL_ATTEMPTS: u64 = 120;
const POLL_STEP: std::time::Duration = std::time::Duration::from_secs(2);

/// Which source to ask when the listing does not carry the version.
///
/// In order: what `--source` said; the source the running firmware came from,
/// since that is where this board's images have been coming from; the only
/// remote source, if there is just one. A local source is never chosen -- an
/// image absent from the listing is by definition not the file sitting on the
/// SD card.
fn blind_source(catalog: &fork::Catalog, args: &InstallArgs) -> Result<String> {
    if let Some(want) = &args.source {
        return Ok(want.clone());
    }
    let remote: Vec<&fork::SourceCatalog> = catalog
        .sources
        .iter()
        .filter(|s| s.kind != Some(SourceKind::Local))
        .collect();

    if let Some(s) = remote
        .iter()
        .find(|s| s.candidates.iter().any(|c| c.version == catalog.running))
    {
        return Ok(s.id.clone());
    }
    match remote.as_slice() {
        [only] => Ok(only.id.clone()),
        [] => bail!("no remote firmware source is configured; `tpi firmware sources` shows them"),
        many => {
            let ids: Vec<&str> = many.iter().map(|s| s.id.as_str()).collect();
            bail!(
                "{} is not in the board's cached listing, and several sources could have it \
                 ({}); name one with --source",
                args.target,
                ids.join(", ")
            )
        }
    }
}

/// The install query for a version the listing does not carry.
///
/// Always the remote form: the local one needs a file path, which only the
/// listing could supply.
fn blind_request(source_id: &str, version: &str, force: bool) -> Vec<(&'static str, String)> {
    let mut pairs = vec![
        ("type", "firmware_install".to_string()),
        ("source", source_id.to_string()),
        ("version", version.to_string()),
    ];
    if force {
        pairs.push(("force", "1".to_string()));
    }
    pairs
}

/// How old a listing is, in words, for a line a person reads.
fn age_phrase(seconds: u64) -> String {
    match seconds {
        0..=90 => "just now".to_string(),
        s if s < 5400 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
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
    // The listing informs; it does not decide.
    //
    // It used to REFUSE a version it did not carry, on the grounds that the
    // board "would accept a source/version pair that offers nothing and fail
    // later, in the middle of a download". Measured on a board on 2026-09-09,
    // that is simply untrue: the daemon answers **400 in 0.48 s** with
    // `tpi-selfupdate: cannot fetch SHA256SUMS for v9.9.9`, before any
    // download, and stages nothing.
    //
    // So the refusal protected against nothing and cost something real: the
    // listing is a cache that can lag a release by half an hour, and
    // `firmware check` reads a different cache with its own timing, so
    // `check` would print "install it with: tpi firmware install <v>" and
    // this resolution would reject the very command it printed. The board is
    // the authority on what the board can fetch. Ask it.
    let catalog = fork::catalog(request, client, false).await?;
    let hits = matching(&catalog, args);

    let resolved = match hits.len() {
        0 => None,
        1 => Some(hits[0]),
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

    let (pairs, local, summary, staged_version) = if let Some((si, ci)) = resolved {
        let source = &catalog.sources[si];
        let candidate = &source.candidates[ci];

        if candidate.relation == Relation::Current && !args.force {
            println!("{} is already running; nothing to do", candidate.version);
            return Ok(0);
        }

        let direction = match candidate.relation {
            Relation::Newer => "upgrade",
            Relation::Older => "DOWNGRADE",
            Relation::Current => "reinstall",
            Relation::Unknown => "install (not comparable with the running build)",
        };
        (
            install_request(source, candidate, args.force)?,
            source.kind == Some(SourceKind::Local),
            format!(
                "{direction} {} -> {} from {} ({})",
                catalog.running,
                candidate.version,
                source.id,
                candidate.trust.label()
            ),
            candidate.version.clone(),
        )
    } else {
        // Not in the listing. That is usually a cache older than the release,
        // so say which source is being asked and let the board answer -- it
        // rejects a version nobody has in under a second.
        let source_id = blind_source(&catalog, args)?;
        (
            blind_request(&source_id, &args.target, args.force),
            false,
            format!(
                "install {} from {} -- not in the board's cached listing (checked {}), \
                 so the board is being asked directly",
                args.target,
                source_id,
                age_phrase(catalog.age_seconds)
            ),
            args.target.clone(),
        )
    };

    if !args.yes && !json {
        println!("{summary}");

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

    println!("staged {staged_version}; reboot to take it");
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
            age_seconds: 0,
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

    /// The refusal this used to make is gone, so what matters now is which
    /// source gets asked when the listing cannot say.
    #[test]
    fn an_explicit_source_wins() {
        let cat = catalog_of(vec![source("fork", None), source("mirror", None)]);
        let picked = blind_source(&cat, &install_args("v9.9.9", Some("mirror"))).expect("picks");
        assert_eq!(picked, "mirror");
    }

    /// With several to choose from, the one the running firmware came from is
    /// the honest guess -- that is where this board's images have come from.
    #[test]
    fn the_running_firmwares_source_is_preferred() {
        let mut fork_src = source("fork", None);
        fork_src.candidates = vec![candidate("v2.15.0", None)];
        let mut mirror = source("mirror", None);
        mirror.candidates = vec![candidate("v2.1.0", None)];
        let cat = catalog_of(vec![mirror, fork_src]);

        let picked = blind_source(&cat, &install_args("v9.9.9", None)).expect("picks");
        assert_eq!(picked, "fork", "v2.15.0 is what catalog_of says is running");
    }

    /// A local source is never guessed at: a version absent from the listing
    /// cannot be the file on the SD card, and the local endpoint needs a path
    /// only the listing could give.
    #[test]
    fn the_sd_card_is_never_guessed() {
        let mut local = source("local", Some(SourceKind::Local));
        local.candidates = vec![candidate("v2.15.0", Some("/mnt/sdcard/x.tpu"))];
        let cat = catalog_of(vec![local, source("fork", None)]);

        assert_eq!(
            blind_source(&cat, &install_args("v9.9.9", None)).expect("picks"),
            "fork"
        );
    }

    /// Ambiguity asks rather than guesses.
    #[test]
    fn several_candidates_for_the_guess_means_ask() {
        let cat = catalog_of(vec![source("a", None), source("b", None)]);
        let err = blind_source(&cat, &install_args("v9.9.9", None)).expect_err("refuses");
        assert!(err.to_string().contains("--source"), "{err}");
    }

    /// The blind request is the remote form, never the local one.
    #[test]
    fn a_blind_request_never_uses_the_local_endpoint() {
        let pairs = blind_request("fork", "v9.9.9", false);
        assert!(pairs.contains(&("type", "firmware_install".to_string())));
        assert!(pairs.iter().all(|(k, _)| *k != "local" && *k != "file"));
        assert!(blind_request("fork", "v9.9.9", true).contains(&("force", "1".to_string())));
    }

    /// The bound has to clear a poll. Measured at 74 s, 78 s and 140 s on the
    /// board; the old one was 60 s and gave up silently.
    #[test]
    fn the_refresh_bound_clears_a_measured_poll() {
        assert!(
            POLL_ATTEMPTS * POLL_STEP.as_secs() >= 200,
            "polls have taken 140 s; leave headroom"
        );
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
