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

//! The fork's additions to the BMC API: the firmware catalogue, the source
//! list, the update check and the thermal sensor.
//!
//! Upstream's handler builds ONE request per command and prints its
//! `response` key. That shape does not fit here. A catalogue is a table
//! assembled from several sources; an install is a choice made against a
//! listing; and every one of these first has to establish whether the board
//! is new enough to have the endpoint at all.
//!
//! The version gate is the part that earns its place. Without it a board one
//! release behind answers
//!
//! ```text
//! Invalid `type` parameter firmware_available
//! ```
//!
//! which names the query parameter tpi sent and tells the operator nothing.
//! The board already reports its own version through `about`, so one extra
//! GET turns that into a sentence with an action in it.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::request::Request;

/// Endpoints this fork added, with the bmcd release each first answered in.
///
/// A board is normally BEHIND the CLI, not ahead: the on-board tpi ships
/// inside the firmware and is always matched to its bmcd, while a
/// workstation tpi updates on its own schedule and points at whatever is on
/// the rack. Being behind is the ordinary case, so it gets a real message
/// rather than an error path.
pub const SINCE_FIRMWARE_CATALOGUE: &str = "2.8.0";
pub const SINCE_THERMAL: &str = "2.5.0";
/// The hostname and the time servers arrived together.
pub const SINCE_HOSTNAME: &str = "2.13.0";
pub const SINCE_NTP: &str = "2.13.0";
pub const SINCE_CONFIG: &str = "2.14.0";
/// `health` has been in the daemon since 2.15.0.
pub const SINCE_HEALTH: &str = "2.15.0";
/// The microSD listing arrived with 2.34.0.
pub const SINCE_SDCARD_FILES: &str = "2.34.0";

/// What the board says about itself.
#[derive(Debug, Clone, Deserialize)]
pub struct About {
    #[serde(default)]
    pub bmcd_version: String,
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub buildroot: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub board_model: String,
}

/// How a candidate compares with what is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Relation {
    Current,
    Newer,
    Older,
    Unknown,
}

impl Relation {
    /// A single character, so a listing stays readable at a glance without
    /// colour -- this runs over SSH into a rack as often as in a terminal
    /// that can render it.
    pub fn marker(self) -> char {
        match self {
            Relation::Current => '=',
            Relation::Newer => '^',
            Relation::Older => 'v',
            Relation::Unknown => '?',
        }
    }
}

/// How much is known about an image's integrity. Three genuinely different
/// things, and a listing that renders them alike is worse than one that
/// omits them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// A checksum published by the release and verified on download.
    Verified,
    /// TLS only: the publisher ships no checksums at all.
    Tls,
    /// A local file, whose provenance is whatever put it there.
    Unverified,
}

impl Trust {
    pub fn label(self) -> &'static str {
        match self {
            Trust::Verified => "verified",
            Trust::Tls => "tls-only",
            Trust::Unverified => "unverified",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Candidate {
    pub version: String,
    pub relation: Relation,
    #[serde(default)]
    pub prerelease: bool,
    pub trust: Trust,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceCatalog {
    pub id: String,
    pub label: String,
    /// How the board reaches this source. It decides how an install is posted
    /// -- a file already on the board is staged through the transfer
    /// endpoint, not through `firmware_install` -- so it is read, not
    /// decorative. Optional because a daemon older than the catalogue would
    /// omit it; the version gate makes that unreachable, and `None` then
    /// means "not local", which is the safe reading.
    #[serde(default)]
    pub kind: Option<SourceKind>,
    pub location: String,
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    /// A source that could not be reached. Reported per source, never as a
    /// whole-request failure: one unreachable mirror must not hide the
    /// versions the other sources can offer.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    /// The previous answers, with a re-poll running behind them. Absent from
    /// a settled catalogue and from any daemon older than 2.11.0.
    #[serde(default)]
    pub refreshing: bool,
    #[serde(default)]
    pub checked_at: String,
    /// How long ago the daemon last polled the sources. It sends this; we
    /// ignored it, which is why `firmware list` could print a listing half an
    /// hour old and look exactly like a fresh one.
    #[serde(default)]
    pub age_seconds: u64,
    #[serde(default)]
    pub running: String,
    #[serde(default)]
    pub sources: Vec<SourceCatalog>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Github,
    Http,
    Local,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Github => "github",
            SourceKind::Http => "http",
            SourceKind::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub kind: SourceKind,
    pub label: String,
    pub location: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sources {
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateCheck {
    #[serde(default)]
    pub running: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Compares two dotted versions numerically.
///
/// String comparison gets this wrong at exactly the point it matters:
/// "2.10.0" sorts BEFORE "2.9.0" as text, so a board on 2.9.0 would be told
/// it satisfies a 2.10.0 requirement. Non-numeric components compare as 0,
/// which is right for the shapes bmcd emits and harmless otherwise.
fn version_at_least(have: &str, want: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };

    let (h, w) = (parts(have), parts(want));
    for i in 0..h.len().max(w.len()) {
        let (a, b) = (
            h.get(i).copied().unwrap_or(0),
            w.get(i).copied().unwrap_or(0),
        );
        if a != b {
            return a > b;
        }
    }

    true
}

/// Refuses a command the board is too old to serve, and says what to do.
pub fn require(about: &About, command: &str, since: &str) -> Result<()> {
    if about.bmcd_version.is_empty() {
        // An `about` without a version is an old board too -- the field
        // predates the fork, but a board that does not answer it at all is
        // not one that will answer the catalogue.
        bail!(
            "this board did not report a bmcd version, so it is older than {since}; \
             `tpi {command}` needs bmcd {since} or newer"
        );
    }

    if !version_at_least(&about.bmcd_version, since) {
        bail!(
            "this board runs bmcd {}; `tpi {command}` needs {since} or newer -- \
             upgrade the firmware first (`tpi firmware check`)",
            about.bmcd_version
        );
    }

    Ok(())
}

/// One GET against the legacy API, returning the `response` payload.
///
/// Every fork endpoint is a GET with `opt` and `type`, so this is the whole
/// transport. `Request::send` consumes itself, which is why each call builds
/// a fresh one rather than reusing the handler's.
pub async fn get(
    request: &Request,
    client: &Client,
    pairs: &[(&str, &str)],
) -> Result<serde_json::Value> {
    let mut req = request.to_get()?;
    {
        let url = req.url_mut();
        let mut q = url.query_pairs_mut();
        q.append_pair("opt", "get");
        for (k, v) in pairs {
            q.append_pair(k, v);
        }
    }

    unwrap_response(req, client, pairs).await
}

/// One `opt=set` against the legacy API.
pub async fn set(
    request: &Request,
    client: &Client,
    pairs: &[(&str, &str)],
) -> Result<serde_json::Value> {
    let mut req = request.to_get()?;
    {
        let url = req.url_mut();
        let mut q = url.query_pairs_mut();
        q.append_pair("opt", "set");
        for (k, v) in pairs {
            q.append_pair(k, v);
        }
    }

    unwrap_response(req, client, pairs).await
}

async fn unwrap_response(
    req: Request,
    client: &Client,
    pairs: &[(&str, &str)],
) -> Result<serde_json::Value> {
    let what = pairs
        .iter()
        .find(|(k, _)| *k == "type")
        .map(|(_, v)| *v)
        .unwrap_or("request");

    let resp = req.send(client.clone()).await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;

    let body: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "{what}: {} returned something that is not JSON:\n{}",
            status,
            String::from_utf8_lossy(&bytes)
        )
    })?;

    if !status.is_success() {
        // bmcd puts its refusal in the body; the status alone ("400 Bad
        // Request") is never the useful half.
        //
        // The refusal arrives in the same wrapper as a success --
        // `{"response":[{"result":"the message"}]}` -- and reading `response`
        // as a string missed that, so every refusal printed as raw JSON with
        // the message buried and escaped inside it. Dig through the wrapper,
        // and fall back to the whole body only when the shape is unfamiliar.
        let detail = body
            .get("response")
            .and_then(|r| r.as_array())
            .and_then(|items| items.first())
            .and_then(|first| first.get("result").or(Some(first)))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .or_else(|| {
                body.get("response")
                    .and_then(|r| r.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| body.to_string());
        bail!("{what}: {detail}");
    }

    // The legacy API wraps an answer as `{"response":[{"result": ...}]}`, and
    // it is the `result` a caller wants. Returning the array instead was a
    // real bug and a quiet one: serde will deserialise a struct from a
    // sequence, taking the elements as the fields in order, so `About` read
    // the single `{"result": …}` map as its first field and failed with
    // "invalid type: map, expected a string". Every fork command begins with
    // the version gate, which reads `about`, so every one of them failed
    // against a real board -- and none of them had been run against one.
    //
    // Not every endpoint uses the wrapper: the transfer endpoint answers with
    // a bare `{"handle": N}`. So unwrap when the shape is there and pass the
    // body through when it is not, rather than assuming either.
    let Some(response) = body.get("response") else {
        return Ok(body);
    };

    match response.as_array().and_then(|items| items.first()) {
        Some(first) => Ok(first
            .get("result")
            .cloned()
            // An entry with no `result` is the shape `opt=set` returns for a
            // plain acknowledgement; hand back the entry rather than nothing.
            .unwrap_or_else(|| first.clone())),
        // `response` present but not an array of objects -- a refusal body,
        // or a string acknowledgement.
        None => Ok(response.clone()),
    }
}

pub async fn about(request: &Request, client: &Client) -> Result<About> {
    let v = get(request, client, &[("type", "about")]).await?;
    serde_json::from_value(v).context("parsing the board's `about` payload")
}

pub async fn catalog(request: &Request, client: &Client, refresh: bool) -> Result<Catalog> {
    let mut pairs: Vec<(&str, &str)> = vec![("type", "firmware_available")];
    if refresh {
        pairs.push(("refresh", "1"));
    }
    let v = get(request, client, &pairs).await?;
    serde_json::from_value(v).context("parsing the firmware catalogue")
}

/// The A/B slots, as raw JSON.
///
/// Untyped on purpose: `firmware list` reads two fields out of it to say what
/// is staged and how the last boot went, and giving those a type here would
/// mean a struct that has to track every field the daemon adds in order to
/// keep reading the two that matter.
pub async fn slots(request: &Request, client: &Client) -> Result<serde_json::Value> {
    get(request, client, &[("type", "firmware_slots")]).await
}

pub async fn sources(request: &Request, client: &Client) -> Result<Sources> {
    let v = get(request, client, &[("type", "firmware_sources")]).await?;
    serde_json::from_value(v).context("parsing the firmware source list")
}

/// The stable channel's update check.
///
/// The daemon answers per channel -- `{checked_at, error, stable: {…}, edge:
/// {…}}` -- and this used to deserialise the outer object straight into
/// `UpdateCheck`, whose fields are all `#[serde(default)]`. So it "succeeded"
/// with everything empty and `firmware check` printed " is current": a
/// missing version rendered as a blank, and no error anywhere. `default` on
/// every field is what turned a shape mismatch into silence.
///
/// Stable is the channel a board follows unless told otherwise; `edge` is
/// read only to report it when the two disagree.
pub async fn update_check(request: &Request, client: &Client) -> Result<UpdateCheck> {
    let v = get(request, client, &[("type", "update_check")]).await?;

    // A whole-request error sits outside the channels.
    if let Some(error) = v.get("error").and_then(|e| e.as_str()) {
        return Ok(UpdateCheck {
            error: Some(error.to_string()),
            ..Default::default()
        });
    }

    let channel = v
        .get("stable")
        .or_else(|| v.get("edge"))
        .ok_or_else(|| anyhow::anyhow!("the update check named no channel"))?;

    serde_json::from_value(channel.clone()).context("parsing the update check")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comparison that string ordering gets wrong, and the reason this
    /// is not a `>=` on `&str`: as text "2.10.0" < "2.9.0", so a board on
    /// 2.9.0 would be told it satisfied a 2.10.0 requirement and would then
    /// fail with the unreadable error the gate exists to replace.
    #[test]
    fn two_ten_is_newer_than_two_nine() {
        assert!(version_at_least("2.10.0", "2.9.0"));
        assert!(!version_at_least("2.9.0", "2.10.0"));
        assert!("2.10.0" < "2.9.0", "the string comparison this replaces");
    }

    #[test]
    fn equal_versions_satisfy_the_requirement() {
        assert!(version_at_least("2.9.0", "2.9.0"));
        assert!(version_at_least("v2.9.0", "2.9.0"));
    }

    #[test]
    fn shorter_versions_pad_with_zero() {
        assert!(version_at_least("3", "2.9.0"));
        assert!(!version_at_least("2.9", "2.9.1"));
        assert!(version_at_least("2.9.0", "2.9"));
    }

    fn board(version: &str) -> About {
        About {
            bmcd_version: version.to_string(),
            kernel: String::new(),
            version: String::new(),
            buildroot: String::new(),
            hostname: String::new(),
            board_model: String::new(),
        }
    }

    /// The message has to name the board's version, the requirement and the
    /// next action -- that is the whole reason the gate exists rather than
    /// letting bmcd answer "Invalid `type` parameter".
    #[test]
    fn an_old_board_is_refused_with_something_actionable() {
        let err = require(&board("2.7.0"), "firmware list", SINCE_FIRMWARE_CATALOGUE)
            .expect_err("2.7.0 must not satisfy 2.8.0");
        let text = err.to_string();

        assert!(
            text.contains("2.7.0"),
            "must name what the board runs: {text}"
        );
        assert!(text.contains("2.8.0"), "must name what is required: {text}");
        assert!(
            text.contains("firmware list"),
            "must name the command: {text}"
        );
        assert!(text.contains("upgrade"), "must say what to do: {text}");
    }

    #[test]
    fn a_new_enough_board_passes() {
        assert!(require(&board("2.9.0"), "firmware list", SINCE_FIRMWARE_CATALOGUE).is_ok());
        assert!(require(&board("2.8.0"), "firmware list", SINCE_FIRMWARE_CATALOGUE).is_ok());
    }

    /// A board too old to report a version is too old for every one of these
    /// commands, and saying so beats a confusing parse error.
    #[test]
    fn a_board_with_no_version_is_refused() {
        assert!(require(&board(""), "firmware list", SINCE_FIRMWARE_CATALOGUE).is_err());
    }
}
