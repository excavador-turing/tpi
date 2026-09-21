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
use reqwest::{Client, Method};

use crate::cli::{
    AddressApplyArgs, AddressCmd, ConfigCmd, HostnameArgs, InstallArgs, ListArgs, NtpCmd,
    SecondUplinkArg, SourceAddArgs, SourceKindArg, SourcesCmd, SwitchApplyArgs, SwitchCmd,
    SwitchPresetArg, TlsCmd,
};
use crate::fork::{
    self, About, Relation, Source, SourceKind, Sources, SINCE_ADDRESS, SINCE_CONFIG,
    SINCE_FIRMWARE_CATALOGUE, SINCE_HOSTNAME, SINCE_NTP, SINCE_THERMAL,
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

    // This used to warn that the rename split the board's metrics history.
    // It does not: bmcd's exposition carries no `instance` and no `hostname`
    // label -- measured on a board, 190 lines, the name appears nowhere.
    // `instance` is assigned by whatever scrapes it, so renaming the board
    // moves nothing and editing the scrape config moves everything.
    //
    // A confident, specific warning that is false is worse than none. It
    // makes an operator hesitate over a rename that is safe, and worse, it
    // implies the inverse -- that leaving the name alone protects the
    // history -- when the scrape config is the only thing that decides.
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

    // What chrony thinks of each source. "NOT synchronised" alone sent a
    // user to Discord; this is the table that says why.
    if let Some(sources) = value.get("sources").and_then(|v| v.as_array()) {
        if !sources.is_empty() {
            println!();
            println!(
                "{:<28} {:<12} {:>7} {:>5} {:>8}",
                "source", "state", "stratum", "reach", "offset"
            );
            for source in sources {
                let name = source.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let state = source.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                let stratum = source.get("stratum").and_then(|v| v.as_u64()).unwrap_or(0);
                let reach = source.get("reach").and_then(|v| v.as_u64()).unwrap_or(0);
                let offset = source
                    .get("offset_seconds")
                    .and_then(|v| v.as_f64())
                    .map(|o| format!("{:+.1}ms", o * 1000.0))
                    .unwrap_or_default();
                let configured = if source.get("configured").and_then(|v| v.as_bool()) == Some(true)
                {
                    "configured"
                } else {
                    ""
                };
                println!(
                    "{name:<28} {state:<12} {stratum:>7} {reach:>3}/8 {offset:>8}  {configured}"
                );
            }
            let selected = sources
                .iter()
                .any(|s| s.get("state").and_then(|v| v.as_str()) == Some("selected"));
            if !selected {
                println!();
                println!(
                    "no source is selected. `unresolved` means the board could not look the name up \
                     -- it has no working resolver (`tpi network address show`), or use the server's \
                     address instead of its name; `unreachable` never answered (address, firewall, a \
                     router that does not serve NTP); `falseticker` or stratum 16 is a server that \
                     reports itself unsynchronised, which chrony will not take time from."
                );
            }
        }
    }
    Ok(0)
}

fn address_apply_body(args: &AddressApplyArgs) -> Result<serde_json::Value> {
    let mut body = match (&args.dhcp, &args.static_addr) {
        (true, None) => serde_json::json!({ "mode": "dhcp" }),
        (false, Some(cidr)) => {
            let (address, prefix) = cidr.split_once('/').with_context(|| {
                format!("{cidr}: give the address with its prefix, like 192.168.1.20/24")
            })?;
            let address: std::net::Ipv4Addr = address
                .parse()
                .with_context(|| format!("{address} is not an IPv4 address"))?;
            let prefix: u8 = prefix
                .parse()
                .with_context(|| format!("/{prefix} is not a prefix length"))?;
            let mut doc = serde_json::json!({
                "mode": "static",
                "address": address,
                "prefix": prefix,
                "dns": args.dns,
            });
            if let Some(gw) = args.gateway {
                doc["gateway"] = serde_json::json!(gw);
            }
            if let Some(search) = &args.search {
                doc["search"] = serde_json::json!(search);
            }
            doc
        }
        (false, None) => bail!("give --dhcp or --static ADDRESS/PREFIX."),
        (true, Some(_)) => bail!("--dhcp and --static are alternatives."),
    };
    if let Some(window) = args.window {
        body["window_s"] = serde_json::json!(window);
    }
    Ok(body)
}

/// `tpi network address ...`
pub async fn address_cmd(
    request: &Request,
    client: &Client,
    cmd: &AddressCmd,
    json: bool,
) -> Result<u8> {
    gate(request, client, "network address", SINCE_ADDRESS).await?;
    const PATH: &str = "network/address";

    let value = match cmd {
        AddressCmd::Show => {
            fork::call_path(request, client, Method::GET, PATH, None, "address show").await?
        }
        AddressCmd::Apply(args) => {
            let body = address_apply_body(args)?;
            fork::call_path(
                request,
                client,
                Method::PUT,
                PATH,
                Some(&body),
                "address apply",
            )
            .await?
        }
        AddressCmd::Confirm(args) => {
            let body = serde_json::json!({ "token": args.token });
            fork::call_path(
                request,
                client,
                Method::POST,
                "network/address/confirm",
                Some(&body),
                "address confirm",
            )
            .await?
        }
        AddressCmd::Revert => {
            fork::call_path(
                request,
                client,
                Method::POST,
                "network/address/revert",
                None,
                "address revert",
            )
            .await?
        }
    };

    if json {
        return emit_json(&value).map(|_| 0);
    }
    match cmd {
        AddressCmd::Apply(_) => print_address_pending(&value),
        _ => print_address_state(&value),
    }
    Ok(0)
}

const CONFIRM_ADDRESS_FROM_HERE: &str =
    "Confirm from a machine that reaches the board at the NEW address, or from the board's \
     interface opened there. A confirmation sent from a shell on the board itself is refused: it \
     never used the address, so it would prove nothing.";

/// One line for a document: `dhcp`, or the static address with its parts.
fn address_words(document: &serde_json::Value) -> String {
    match document.get("mode").and_then(serde_json::Value::as_str) {
        Some("dhcp") => "dhcp".to_string(),
        Some("static") => {
            let mut out = format!(
                "static {}/{}",
                document
                    .get("address")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?"),
                document.get("prefix").and_then(|v| v.as_u64()).unwrap_or(0)
            );
            if let Some(gw) = document.get("gateway").and_then(|v| v.as_str()) {
                out.push_str(&format!(" via {gw}"));
            }
            let dns: Vec<&str> = document
                .get("dns")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|d| d.as_str()).collect())
                .unwrap_or_default();
            if !dns.is_empty() {
                out.push_str(&format!(", dns {}", dns.join(" ")));
            }
            out
        }
        _ => "?".to_string(),
    }
}

fn print_address_state(value: &serde_json::Value) {
    if let Some(running) = value.get("running") {
        println!("running:    {}", address_words(running));
    }
    match value.get("configured") {
        Some(serde_json::Value::Null) | None => {
            println!("configured: nothing this tool can read -- a reboot comes back to whatever the file says");
        }
        Some(configured) => println!(
            "configured: {}  (what a reboot comes back to)",
            address_words(configured)
        ),
    }
    if let Some(live) = value.get("live") {
        let address = live
            .get("address")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        let mode = live.get("mode").and_then(|v| v.as_str()).unwrap_or("?");
        let gw = live
            .get("gateway")
            .and_then(|v| v.as_str())
            .map(|g| format!(" via {g}"))
            .unwrap_or_default();
        println!("live:       {address}{gw}  ({mode})");
    }
    match value.get("file").and_then(|v| v.as_str()) {
        Some("hand_edited") => println!("the interfaces file was written by hand; a confirmed change replaces it"),
        Some("unreadable") => println!("the interfaces file was written by hand and this tool cannot read it; a confirmed change replaces it whole"),
        _ => {}
    }

    if let Some(pending) = value.get("pending").filter(|p| !p.is_null()) {
        let token = pending.get("token").and_then(|v| v.as_str()).unwrap_or("?");
        let window = pending
            .get("window_s")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!();
        println!(
            "A CHANGE IS WAITING: {}, with a {window}s window running.",
            pending
                .get("document")
                .map(address_words)
                .unwrap_or_default()
        );
        println!("Confirm it with:  tpi network address confirm {token}");
        println!("{CONFIRM_ADDRESS_FROM_HERE}");
    }
    if let Some(revert) = value.get("last_revert").filter(|r| !r.is_null()) {
        let reason = revert.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
        println!();
        println!(
            "last revert: {reason} ({})",
            revert
                .get("document")
                .map(address_words)
                .unwrap_or_default()
        );
    }
}

fn print_address_pending(value: &serde_json::Value) {
    let token = value.get("token").and_then(|v| v.as_str()).unwrap_or("?");
    let window = value.get("window_s").and_then(|v| v.as_u64()).unwrap_or(0);
    println!("Applied, and NOT yet kept.");
    println!();
    println!(
        "The board is now on {}. It will put the previous address back in {window}s unless you \
         confirm -- at the new address:",
        value.get("document").map(address_words).unwrap_or_default()
    );
    println!();
    println!("  tpi --host <new address> network address confirm {token}");
    println!();
    println!("{CONFIRM_ADDRESS_FROM_HERE}");
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
/// `tpi health` -- what the board says about itself.
///
/// Uptime, load, memory, NAND wear and whether the clock is actually
/// disciplined. Every field is optional on the daemon's side, so an older
/// board produces a shorter table rather than an error; printing "unknown"
/// for something the board never claimed would be inventing a reading.
pub async fn health(request: &Request, client: &Client, json: bool) -> Result<u8> {
    gate(request, client, "health", fork::SINCE_HEALTH).await?;
    let value = fork::get(request, client, &[("type", "health")]).await?;

    if json {
        return emit_json(&value).map(|_| 0);
    }

    let row = |label: &str, v: Option<String>| {
        if let Some(v) = v {
            println!("{label:<22} {v}");
        }
    };

    let n = |k: &str| value.get(k).and_then(serde_json::Value::as_f64);

    row(
        "uptime",
        n("uptime_seconds").map(|v| format_seconds(v as u64)),
    );

    // `load` is an object with the three windows, not a bare number. Read off
    // the board rather than guessed: a key that does not exist reads as absent
    // and prints nothing, which would have hidden this silently.
    if let Some(load) = value.get("load") {
        let l = |k: &str| load.get(k).and_then(serde_json::Value::as_f64);
        if let (Some(a), Some(b), Some(c)) =
            (l("one_minute"), l("five_minutes"), l("fifteen_minutes"))
        {
            println!("{:<22} {a:.2}  {b:.2}  {c:.2}", "load 1/5/15m");
        }
    }

    if let Some(mem) = value.get("memory") {
        let m = |k: &str| mem.get(k).and_then(|v| v.as_f64());
        if let (Some(total), Some(avail)) = (m("total_bytes"), m("available_bytes")) {
            println!(
                "{:<22} {} of {} available",
                "memory",
                format_bytes(avail as u64),
                format_bytes(total as u64)
            );
        }
        row(
            "  daemon resident",
            m("self_resident_bytes").map(|v| format_bytes(v as u64)),
        );
    }

    if let Some(nand) = value.get("nand") {
        let m = |k: &str| nand.get(k).and_then(serde_json::Value::as_f64);
        // `available_eraseblocks`, not `free_*`. Five of them is the real
        // number on these boards, which is why nothing large is ever written
        // to the overlay.
        if let (Some(avail), Some(bad)) = (m("available_eraseblocks"), m("bad_eraseblocks")) {
            println!(
                "{:<22} {} eraseblocks available, {} bad",
                "nand", avail as u64, bad as u64
            );
        }
    }

    if let Some(clock) = value.get("clock") {
        let synced = clock
            .get("synchronised")
            .and_then(serde_json::Value::as_bool);
        let source = clock
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("no source");
        // The distinction worth printing: a board can have time and not be
        // disciplined to anything, which is how a certificate looks expired
        // and a log looks out of order.
        println!(
            "{:<22} {}",
            "clock",
            match synced {
                Some(true) => format!("synchronised to {source}"),
                Some(false) => format!("NOT synchronised ({source})"),
                None => "state not reported".to_string(),
            }
        );
        if let Some(offset) = clock
            .get("offset_seconds")
            .and_then(serde_json::Value::as_f64)
        {
            println!("{:<22} {:.3} ms", "  offset", offset * 1000.0);
        }
    }

    Ok(0)
}

/// `tpi sdcard [path]` -- what is on the card, and what can be written to a
/// module.
///
/// The companion to `tpi flash --local`, which until now required the operator
/// to know a path and type it correctly for the most destructive thing this
/// board does.
pub async fn sdcard(
    request: &Request,
    client: &Client,
    path: Option<&str>,
    json: bool,
) -> Result<u8> {
    gate(request, client, "sdcard", fork::SINCE_SDCARD_FILES).await?;

    let mut params: Vec<(&str, &str)> = vec![("type", "sdcard_files")];
    if let Some(p) = path {
        params.push(("path", p));
    }
    let value = fork::get(request, client, &params).await?;

    if json {
        return emit_json(&value).map(|_| 0);
    }

    let Some(entries) = value.as_array() else {
        println!("the board did not return a listing");
        return Ok(1);
    };

    if entries.is_empty() {
        println!("nothing here");
        return Ok(0);
    }

    for e in entries {
        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let dir = e
            .get("directory")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let size = e
            .get("size")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let flashable = e
            .get("flashable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let reason = e.get("reason").and_then(|v| v.as_str());

        if dir {
            println!("  {:<44} {:>10}  directory", format!("{name}/"), "");
        } else {
            // A non-candidate is shown WITH its reason, not hidden. Somebody
            // who cannot see the file they just copied concludes the tool is
            // broken, not that the file was too small.
            let note = if flashable {
                "flashable".to_string()
            } else {
                reason.unwrap_or("not an OS image").to_string()
            };
            println!("  {name:<44} {:>10}  {note}", format_bytes(size));
        }
    }

    Ok(0)
}

/// Bytes, at one decimal, in the unit a person would use.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Seconds as days/hours/minutes, dropping the parts that are zero.
fn format_seconds(seconds: u64) -> String {
    let d = seconds / 86_400;
    let h = (seconds % 86_400) / 3_600;
    let m = (seconds % 3_600) / 60;
    match (d, h, m) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
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

/// The two mistakes worth catching before a round-trip: the files swapped,
/// and one file holding both halves.
///
/// Not validation -- the board does that properly, and refuses before it
/// writes anything. This is about the message. A swapped pair rejected by the
/// board says "not a PEM private key", which is true and unhelpful when the
/// real problem is that `--cert` and `--key` are the wrong way round.
///
/// A certificate file that also holds the key is the more dangerous one: it
/// is what `openssl req` writes when told to put both in one place, and
/// sending it would put the private key in a field the board treats as public
/// and echoes back in `GET`.
fn check_pem_shapes(cert: &str, key: &str, cert_path: &str, key_path: &str) -> Result<()> {
    if !cert.contains("BEGIN CERTIFICATE") {
        bail!(
            "{cert_path} does not look like a PEM certificate. If you passed a DER or \
             PKCS#12 file, convert it first."
        );
    }
    if cert.contains("PRIVATE KEY") {
        bail!(
            "{cert_path} contains a private key as well as a certificate. Split them: \
             the key goes to --key, and only the key."
        );
    }
    if !key.contains("PRIVATE KEY") {
        bail!("{key_path} does not look like a PEM private key.");
    }
    Ok(())
}

/// `tpi tls show | install | reset` -- the certificate the board serves.
///
/// These are the one family of commands that do not go through the legacy
/// dispatcher, and that is the daemon's decision rather than a preference:
/// it writes every mutating legacy query to the audit log in full, so a
/// private key in one would be recorded in clear on the board.
pub async fn tls_cmd(request: &Request, client: &Client, cmd: &TlsCmd, json: bool) -> Result<u8> {
    const PATH: &str = "tls/certificate";

    let value = match cmd {
        TlsCmd::Show => {
            fork::call_path(request, client, Method::GET, PATH, None, "tls show").await?
        }

        TlsCmd::Install(args) => {
            // Read both before either is sent, so a missing key is reported
            // as a missing key rather than as a half-finished install. The
            // board validates the pair as well and refuses before writing
            // anything; this is only about the message you get for a typo.
            let cert = std::fs::read_to_string(&args.cert)
                .with_context(|| format!("reading {}", args.cert.display()))?;
            let key = std::fs::read_to_string(&args.key)
                .with_context(|| format!("reading {}", args.key.display()))?;

            check_pem_shapes(
                &cert,
                &key,
                &args.cert.display().to_string(),
                &args.key.display().to_string(),
            )?;

            let body = serde_json::json!({ "certificate": cert, "private_key": key });
            fork::call_path(
                request,
                client,
                Method::PUT,
                PATH,
                Some(&body),
                "tls install",
            )
            .await?
        }

        TlsCmd::Reset => {
            fork::call_path(request, client, Method::DELETE, PATH, None, "tls reset").await?
        }
    };

    if json {
        return emit_json(&value).map(|_| 0);
    }

    let s = |k: &str| value.get(k).and_then(serde_json::Value::as_str);

    println!(
        "{:<12} {}",
        "subject",
        s("subject").unwrap_or("(unreadable)")
    );
    println!("{:<12} {}", "issuer", s("issuer").unwrap_or("(unreadable)"));
    println!("{:<12} {}", "valid from", s("not_before").unwrap_or("?"));
    println!("{:<12} {}", "valid until", s("not_after").unwrap_or("?"));
    if let Some(key) = s("key") {
        println!("{:<12} {key}", "key");
    }

    // Spelled out rather than printed as the label, because "self-signed" and
    // "installed" are shorthand for a difference that matters: only one of
    // them renews itself.
    match s("source") {
        Some("self-signed") => println!(
            "{:<12} issued by the board, and renewed by it 30 days before expiry",
            "source"
        ),
        Some("installed") => println!(
            "{:<12} installed; the board will not renew it for you",
            "source"
        ),
        Some(other) => println!("{:<12} {other}", "source"),
        None => {}
    }

    if let Some(names) = value.get("names").and_then(serde_json::Value::as_array) {
        let names: Vec<&str> = names.iter().filter_map(serde_json::Value::as_str).collect();
        if !names.is_empty() {
            println!("{:<12} {}", "names", names.join(", "));
        }
    }

    println!(
        "{:<12} {}",
        "fingerprint",
        s("fingerprint").unwrap_or("(unreadable)")
    );

    Ok(0)
}

#[cfg(test)]
mod tls_tests {
    use super::check_pem_shapes;

    const CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    const KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIGH\n-----END PRIVATE KEY-----\n";

    #[test]
    fn a_proper_pair_passes() {
        assert!(check_pem_shapes(CERT, KEY, "cert.pem", "key.pem").is_ok());
    }

    /// The commonest typo, and the one whose server-side message is least
    /// helpful.
    #[test]
    fn the_two_files_swapped_are_caught_here() {
        let error = check_pem_shapes(KEY, CERT, "key.pem", "cert.pem")
            .expect_err("a swapped pair must be refused");
        assert!(
            format!("{error}").contains("does not look like a PEM certificate"),
            "{error}"
        );
    }

    /// One file holding both halves would send the private key in the field
    /// the board treats as public and echoes back.
    #[test]
    fn a_combined_file_is_refused_before_it_is_sent() {
        let both = format!("{CERT}{KEY}");
        let error = check_pem_shapes(&both, KEY, "both.pem", "key.pem")
            .expect_err("a combined file must be refused");
        assert!(format!("{error}").contains("Split them"), "{error}");
    }

    #[test]
    fn a_key_that_is_not_a_key_is_refused() {
        let error = check_pem_shapes(CERT, CERT, "cert.pem", "also-cert.pem")
            .expect_err("a certificate in --key must be refused");
        assert!(
            format!("{error}").contains("does not look like a PEM private key"),
            "{error}"
        );
    }
}

/// Build the JSON body an apply takes, or say why the arguments do not make
/// one.
///
/// Separate from the command so the argument combinations -- which are the
/// part a person gets wrong -- can be tested without a board.
fn switch_apply_body(args: &SwitchApplyArgs) -> Result<serde_json::Value> {
    let mut body = match (&args.preset, &args.table) {
        (Some(preset), None) => match preset {
            SwitchPresetArg::Flat => serde_json::json!({ "preset": "flat" }),
            SwitchPresetArg::Split => serde_json::json!({ "preset": "split" }),
            SwitchPresetArg::Trunk => {
                // Trunk is the only preset with numbers in it, and they are
                // the operator's: the router on the other end has to agree,
                // and this tool has no way to know what is free there.
                let (Some(mgmt), Some(node)) = (args.mgmt_vid, args.node_vid) else {
                    bail!(
                        "trunk needs --mgmt-vid and --node-vid. They are the VLANs your router \
                         uses for this board and its modules, so only you know them."
                    );
                };
                if mgmt == node {
                    bail!(
                        "--mgmt-vid and --node-vid are both {mgmt}. They have to differ, or the \
                         two networks are one network."
                    );
                }
                let second = match args.second_uplink.unwrap_or(SecondUplinkArg::Redundant) {
                    SecondUplinkArg::Redundant => "redundant",
                    SecondUplinkArg::Off => "off",
                };
                serde_json::json!({
                    "preset": "trunk",
                    "management_vid": mgmt,
                    "node_vid": node,
                    "second_uplink": second,
                })
            }
        },
        (None, Some(path)) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("{} is not a switch document", path.display()))?
        }
        (None, None) => bail!("give either --preset or --table."),
        // clap's `conflicts_with` already refuses this; the arm is here so the
        // function is total rather than relying on an argument parser for
        // correctness.
        (Some(_), Some(_)) => bail!("--preset and --table are alternatives."),
    };

    if let Some(window) = args.window {
        body["window_s"] = serde_json::json!(window);
    }
    Ok(body)
}

/// `tpi network switch ...`
pub async fn switch_cmd(
    request: &Request,
    client: &Client,
    cmd: &SwitchCmd,
    json: bool,
) -> Result<u8> {
    const PATH: &str = "network/switch";

    let value = match cmd {
        SwitchCmd::Show(_) => {
            fork::call_path(request, client, Method::GET, PATH, None, "switch show").await?
        }
        SwitchCmd::Presets => {
            fork::call_path(
                request,
                client,
                Method::GET,
                "network/switch/presets",
                None,
                "switch presets",
            )
            .await?
        }
        SwitchCmd::Apply(args) => {
            let body = switch_apply_body(args)?;
            fork::call_path(
                request,
                client,
                Method::PUT,
                PATH,
                Some(&body),
                "switch apply",
            )
            .await?
        }
        SwitchCmd::Confirm(args) => {
            let body = serde_json::json!({ "token": args.token });
            fork::call_path(
                request,
                client,
                Method::POST,
                "network/switch/confirm",
                Some(&body),
                "switch confirm",
            )
            .await?
        }
        SwitchCmd::Revert => {
            fork::call_path(
                request,
                client,
                Method::POST,
                "network/switch/revert",
                None,
                "switch revert",
            )
            .await?
        }
    };

    if json {
        return emit_json(&value).map(|_| 0);
    }

    // `show --table` prints one thing: the running document, ready to be
    // edited and handed back to `apply --table`. Not the view around it, and
    // not a heading -- anything else on stdout would have to be deleted by
    // hand before the file could be used, and a round trip that needs editing
    // twice is one people stop using.
    if let SwitchCmd::Show(args) = cmd {
        if args.table {
            return emit_json(running_document(&value)?).map(|_| 0);
        }
    }

    match cmd {
        SwitchCmd::Presets => print_switch_presets(&value),
        SwitchCmd::Apply(_) => print_switch_pending(&value),
        _ => print_switch_state(&value),
    }
    Ok(0)
}

/// The document the board is running, out of the view around it.
///
/// An error rather than an empty object when it is missing: a file written
/// from nothing would apply nothing, and `apply --table` would accept it --
/// the board would read an empty `ports` map as every port left at whatever
/// it happened to be. Better to fail here, where there is something to say.
fn running_document(value: &serde_json::Value) -> Result<&serde_json::Value> {
    value
        .get("running")
        .filter(|running| running.get("ports").is_some())
        .context(
            "this board did not answer with a switch document. It is probably running a firmware              from before the switch configuration existed.",
        )
}

/// What the board says after an apply, and what this says before it.
///
/// The same sentence as the daemon's refusal and the interface's, deliberately
/// -- somebody who meets it in one place should recognise it in the next.
const CONFIRM_FROM_HERE: &str =
    "Confirm from this machine, or from the board's interface. A confirmation sent from a shell      on the board itself is refused: it crossed no switch port, so it would prove nothing.";

/// A VLAN as a person should read it: the number, and the word for it if the
/// document carries one.
///
/// The number always comes first and is never replaced. It is what `bridge
/// vlan show` prints, what the router is configured with, and what somebody
/// will be typing into another machine; a name that hid it would make this
/// output impossible to check anything against.
fn vlan_label(names: Option<&serde_json::Map<String, serde_json::Value>>, vid: u64) -> String {
    let name = names
        .and_then(|names| names.get(&vid.to_string()))
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty());
    match name {
        Some(name) => format!("{vid} ({name})"),
        None => vid.to_string(),
    }
}

/// One line per port: what it is untagged in, what it carries tagged.
fn print_switch_ports(document: &serde_json::Value) {
    let filtering = document
        .get("vlan_filtering")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !filtering {
        println!("  one network; the switch does not look at VLANs");
        return;
    }
    let names = document.get("names").and_then(serde_json::Value::as_object);
    let Some(ports) = document.get("ports").and_then(serde_json::Value::as_object) else {
        return;
    };
    for (name, config) in ports {
        let untagged = config
            .get("untagged")
            .and_then(serde_json::Value::as_u64)
            .map(|vid| vlan_label(names, vid))
            .unwrap_or_else(|| "-".to_string());
        let tagged: Vec<String> = config
            .get("tagged")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_u64)
                    .map(|vid| vlan_label(names, vid))
                    .collect()
            })
            .unwrap_or_default();
        let tagged = if tagged.is_empty() {
            String::new()
        } else {
            format!("  tagged {}", tagged.join(", "))
        };
        println!("  {name:<8} untagged {untagged}{tagged}");
    }
    if let Some(stp) = document.get("stp").and_then(serde_json::Value::as_bool) {
        println!("  {:<8} {}", "stp", if stp { "on" } else { "off" });
    }
}

fn print_switch_state(value: &serde_json::Value) {
    if let Some(running) = value.get("running") {
        println!("running:");
        print_switch_ports(running);
    }
    match value.get("confirmed") {
        Some(serde_json::Value::Null) | None => {
            println!("confirmed: nothing yet -- a reboot comes back to the board's default");
        }
        Some(_) => println!("confirmed: yes -- a reboot comes back to this"),
    }

    if let Some(pending) = value.get("pending").filter(|p| !p.is_null()) {
        let token = pending
            .get("token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let window = pending
            .get("window_s")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        println!();
        match pending.get("counting_from") {
            Some(serde_json::Value::Null) | None => println!(
                "A CHANGE IS WAITING. The {window}s window has not started: the uplink is not \
                 forwarding yet."
            ),
            Some(_) => println!("A CHANGE IS WAITING, with a {window}s window already running."),
        }
        println!("Confirm it with:  tpi network switch confirm {token}");
        println!("{CONFIRM_FROM_HERE}");
    }

    if let Some(revert) = value.get("last_revert").filter(|r| !r.is_null()) {
        let reason = revert
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        println!();
        println!("last revert: {reason}");
    }
}

fn print_switch_pending(value: &serde_json::Value) {
    let token = value
        .get("token")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let window = value
        .get("window_s")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    println!("Applied, and NOT yet kept.");
    println!();
    println!(
        "The board will put the previous configuration back in {window}s unless you confirm. \
         The countdown starts when the uplink forwards, not now."
    );
    println!();
    println!("  tpi network switch confirm {token}");
    println!();
    println!("{CONFIRM_FROM_HERE}");
}

fn print_switch_presets(value: &serde_json::Value) {
    let Some(presets) = value.get("presets").and_then(serde_json::Value::as_array) else {
        return;
    };
    for preset in presets {
        let name = preset
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let summary = preset
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        println!("{name}");
        println!("  {summary}");
        if let Some(document) = preset.get("document") {
            print_switch_ports(document);
        }
        println!();
    }
}

#[cfg(test)]
mod switch_table_tests {
    use super::{running_document, vlan_label};
    use serde_json::json;

    /// The round trip this exists for: what `show --table` prints has to be
    /// something `apply --table` would send back unchanged.
    #[test]
    fn the_running_document_comes_out_whole() {
        let view = json!({
            "running": {
                "vlan_filtering": true,
                "stp": false,
                "ports": { "bmc": { "untagged": 10, "tagged": [] } },
                "names": { "10": "management" }
            },
            "confirmed": null,
            "pending": null,
            "default_window_s": 30
        });
        let document = running_document(&view).expect("a board with a switch");
        assert_eq!(document, &view["running"]);
        assert!(
            document.get("confirmed").is_none(),
            "the view around it must not come with: {document}"
        );
    }

    /// An older daemon answers an unrouted path with 200 and `index.html`, so
    /// a client that trusted the status would write an HTML page into the file
    /// somebody is about to edit and apply. The shape is the check.
    #[test]
    fn a_board_that_answers_without_a_document_is_an_error() {
        for answer in [
            json!({}),
            json!({ "running": null }),
            json!("<!doctype html>"),
        ] {
            let error = running_document(&answer).expect_err("{answer} is not a document");
            assert!(format!("{error}").contains("before the switch configuration existed"));
        }
    }

    /// The number is what somebody types into a router. A name is added
    /// beside it and never in place of it.
    #[test]
    fn a_named_vlan_still_shows_its_number_first() {
        let names = json!({ "20": "nodes", "30": "" });
        let names = names.as_object();
        assert_eq!(vlan_label(names, 20), "20 (nodes)");
        assert_eq!(vlan_label(names, 10), "10", "unnamed is just the number");
        assert_eq!(vlan_label(names, 30), "30", "an empty name is no name");
        assert_eq!(vlan_label(None, 20), "20", "an older board sends no names");
    }
}

#[cfg(test)]
mod switch_tests {
    use super::switch_apply_body;
    use crate::cli::{SecondUplinkArg, SwitchApplyArgs, SwitchPresetArg};

    fn args() -> SwitchApplyArgs {
        SwitchApplyArgs {
            preset: None,
            table: None,
            mgmt_vid: None,
            node_vid: None,
            second_uplink: None,
            window: None,
        }
    }

    #[test]
    fn a_preset_with_no_numbers_needs_nothing_else() {
        for (preset, name) in [
            (SwitchPresetArg::Flat, "flat"),
            (SwitchPresetArg::Split, "split"),
        ] {
            let body = switch_apply_body(&SwitchApplyArgs {
                preset: Some(preset),
                ..args()
            })
            .expect("no numbers required");
            assert_eq!(body["preset"], name);
        }
    }

    /// Trunk's VLAN identifiers are the operator's, because the router on the
    /// other end has to agree and this tool cannot know what is free there.
    /// Guessing would produce a board that is configured and unreachable.
    #[test]
    fn trunk_without_its_vlans_is_refused_before_the_board_is_asked() {
        let error = switch_apply_body(&SwitchApplyArgs {
            preset: Some(SwitchPresetArg::Trunk),
            ..args()
        })
        .expect_err("trunk needs numbers");
        assert!(format!("{error}").contains("--mgmt-vid and --node-vid"));
    }

    #[test]
    fn trunk_with_one_vlan_for_both_is_refused() {
        let error = switch_apply_body(&SwitchApplyArgs {
            preset: Some(SwitchPresetArg::Trunk),
            mgmt_vid: Some(10),
            node_vid: Some(10),
            ..args()
        })
        .expect_err("one VLAN is not two networks");
        assert!(format!("{error}").contains("the two networks are one network"));
    }

    #[test]
    fn trunk_defaults_its_second_uplink_to_redundant() {
        let body = switch_apply_body(&SwitchApplyArgs {
            preset: Some(SwitchPresetArg::Trunk),
            mgmt_vid: Some(10),
            node_vid: Some(20),
            ..args()
        })
        .expect("valid");
        assert_eq!(body["second_uplink"], "redundant");
    }

    #[test]
    fn the_second_uplink_can_be_turned_off() {
        let body = switch_apply_body(&SwitchApplyArgs {
            preset: Some(SwitchPresetArg::Trunk),
            mgmt_vid: Some(10),
            node_vid: Some(20),
            second_uplink: Some(SecondUplinkArg::Off),
            ..args()
        })
        .expect("valid");
        assert_eq!(body["second_uplink"], "off");
    }

    #[test]
    fn a_window_is_passed_through_and_omitted_when_absent() {
        let with = switch_apply_body(&SwitchApplyArgs {
            preset: Some(SwitchPresetArg::Flat),
            window: Some(90),
            ..args()
        })
        .expect("valid");
        assert_eq!(with["window_s"], 90);

        let without = switch_apply_body(&SwitchApplyArgs {
            preset: Some(SwitchPresetArg::Flat),
            ..args()
        })
        .expect("valid");
        assert!(
            without.get("window_s").is_none(),
            "absent means the board's default, which only the board knows"
        );
    }

    #[test]
    fn neither_a_preset_nor_a_table_is_refused() {
        let error = switch_apply_body(&args()).expect_err("nothing to apply");
        assert!(format!("{error}").contains("--preset or --table"));
    }

    #[test]
    fn a_table_that_is_not_a_document_names_the_file() {
        let dir = std::env::temp_dir().join(format!("tpi-switch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, b"{ not json").unwrap();

        let error = switch_apply_body(&SwitchApplyArgs {
            table: Some(path.clone()),
            ..args()
        })
        .expect_err("that is not a document");
        assert!(format!("{error}").contains("is not a switch document"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
