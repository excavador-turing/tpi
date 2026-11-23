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

//! Wrapper for `reqwest::Request` that asks for authentication if needed.

use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use reqwest::header::{HeaderValue, USER_AGENT};
use reqwest::multipart::Form;
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use url::Url;

use crate::cli::ApiVersion;
use crate::prompt;

pub struct Request {
    host: String,
    ver: ApiVersion,
    creds: (Option<String>, Option<String>),
    inner: reqwest::Request,
    multipart: Option<Form>,
}

impl Request {
    pub fn new(
        host: String,
        ver: ApiVersion,
        creds: (Option<String>, Option<String>),
        user_agent: &str,
    ) -> Result<Self> {
        let url = url_from_host(&host, ver.scheme())?;
        let mut inner = reqwest::Request::new(Method::GET, url);
        inner
            .headers_mut()
            .insert(USER_AGENT, HeaderValue::from_str(user_agent)?);

        Ok(Self {
            host,
            ver,
            creds,
            inner,
            multipart: None,
        })
    }

    /// A fresh GET against the same host and credentials.
    ///
    /// `send` consumes the request, so a command that needs more than one
    /// round-trip -- the version gate, then the call it was gating -- builds
    /// each from here rather than reusing one.
    pub fn to_get(&self) -> Result<Self> {
        let url = url_from_host(&self.host, self.ver.scheme())?;
        let mut inner = reqwest::Request::new(Method::GET, url);
        if let Some(ua) = self.inner.headers().get(USER_AGENT) {
            inner.headers_mut().insert(USER_AGENT, ua.clone());
        }

        Ok(Self {
            host: self.host.clone(),
            ver: self.ver,
            creds: self.creds.clone(),
            inner,
            multipart: None,
        })
    }

    pub fn to_post(&self) -> Result<Self> {
        let url = url_from_host(&self.host, self.ver.scheme())?;
        let inner = reqwest::Request::new(Method::POST, url);

        Ok(Self {
            host: self.host.clone(),
            ver: self.ver,
            creds: self.creds.clone(),
            inner,
            multipart: None,
        })
    }

    /// A request against one of the fork's own paths, with an optional JSON
    /// body.
    ///
    /// Everything else here speaks the legacy API: one path, `/api/bmc`, with
    /// the operation in the query string. The certificate endpoints cannot,
    /// and that is deliberate on the daemon's side -- it writes every
    /// mutating legacy query to the audit log in full, so a private key in
    /// one would be recorded in clear.
    ///
    /// `path` is relative to `/api/bmc`, without a leading slash.
    pub fn to_path(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Self> {
        let mut url = url_from_host(&self.host, self.ver.scheme())?;
        url.set_path(&format!("api/bmc/{path}"));

        let mut inner = reqwest::Request::new(method, url);
        if let Some(ua) = self.inner.headers().get(USER_AGENT) {
            inner.headers_mut().insert(USER_AGENT, ua.clone());
        }
        if let Some(body) = body {
            inner.headers_mut().insert(
                reqwest::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            // Serialised into bytes rather than streamed, because `send`
            // clones the request to retry it once without a token, and a
            // streaming body cannot be cloned.
            *inner.body_mut() = Some(serde_json::to_vec(body)?.into());
        }

        Ok(Self {
            host: self.host.clone(),
            ver: self.ver,
            creds: self.creds.clone(),
            inner,
            multipart: None,
        })
    }

    pub fn set_multipart(&mut self, form: Form) {
        self.multipart = Some(form);
    }

    pub async fn send(mut self, client: Client) -> Result<Response> {
        let mut authenticated = cfg!(not(feature = "localhost"));
        let mut local_token = local_token_for(
            cfg!(feature = "localhost") && !authenticated,
            &self.host,
            self.creds.0.is_some() || self.creds.1.is_some(),
            || get_cached_token().is_some(),
            || read_local_token(Path::new(LOCAL_TOKEN_PATH)),
        );
        let as_post = sent_as_post(self.inner.method(), self.inner.url());
        let mut fell_back_to_get = false;

        let resp = loop {
            let mut inner = self.inner.try_clone().unwrap();
            if as_post && !fell_back_to_get {
                *inner.method_mut() = Method::POST;
            }
            let mut builder = RequestBuilder::from_parts(client.clone(), inner);

            if authenticated {
                let token = self.get_bearer_token(&client).await?;
                builder = builder.bearer_auth(token);
            } else if let Some(token) = &local_token {
                builder = builder.bearer_auth(token);
            }

            if let Some(form) = self.multipart.take() {
                builder = builder.multipart(form);
            }

            let resp = builder.send().await?;
            if retry_as_get(resp.status(), as_post, fell_back_to_get) {
                fell_back_to_get = true;
            } else if resp.status() == StatusCode::UNAUTHORIZED {
                delete_cached_token();
                local_token = None;
                authenticated = true;
            } else {
                break resp;
            }
        };

        Ok(resp)
    }

    async fn get_bearer_token(&mut self, client: &Client) -> Result<String> {
        // If either credentials are supplied, use them
        if self.creds.0.is_some() || self.creds.1.is_some() {
            return request_token(&self.host, self.ver, &self.creds, client).await;
        }

        // Else, try retrieving cached token from a file
        if let Some(token) = get_cached_token() {
            return Ok(token);
        }

        // If it doesn't exist, ask on an interactive prompt
        request_token(&self.host, self.ver, &self.creds, client).await
    }

    pub fn url(&self) -> &Url {
        self.inner.url()
    }

    pub fn url_mut(&mut self) -> &mut Url {
        self.inner.url_mut()
    }

    pub fn clone(&self) -> Self {
        let inner = self
            .inner
            .try_clone()
            .expect("request cannot be cloned: body is a stream");

        Self {
            host: self.host.clone(),
            ver: self.ver,
            creds: self.creds.clone(),
            inner,
            multipart: None,
        }
    }
}

impl Deref for Request {
    type Target = reqwest::Request;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Request {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

fn url_from_host(host: &str, scheme: &str) -> Result<Url> {
    let mut url = Url::parse(&format!("{}://{}", scheme, host))?;
    url.set_path("api/bmc");
    Ok(url)
}

/// The token bmcd writes at startup for clients on the board itself. It is
/// accepted only from a loopback peer, so it is never sent anywhere else.
const LOCAL_TOKEN_PATH: &str = "/run/bmcd/local-token";

/// The local token goes on the first attempt only from the localhost build,
/// to a loopback host, when no credentials were given and no token is cached.
/// The cache and the token file are not read otherwise.
fn local_token_for(
    localhost_build: bool,
    host: &str,
    has_creds: bool,
    has_cached_token: impl FnOnce() -> bool,
    read: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if !localhost_build || has_creds || !is_loopback(host) || has_cached_token() {
        return None;
    }
    read()
}

/// Absent, unreadable or empty is `None`, and the normal login follows.
fn read_local_token(path: &Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// `127.0.0.1`, `::1` or `localhost`, with or without a port.
fn is_loopback(host: &str) -> bool {
    let Ok(url) = Url::parse(&format!("https://{host}/")) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// A legacy `opt=set` changes state, so it goes as POST with its query string
/// unchanged. Requests that are already POST (the upload) and reads stay as
/// they are.
fn sent_as_post(method: &Method, url: &Url) -> bool {
    *method == Method::GET
        && url.path() == "/api/bmc"
        && url.query_pairs().any(|(k, v)| k == "opt" && v == "set")
}

/// An older bmcd routes `opt=set` on GET only and answers a POST with 405,
/// having executed nothing. The identical request goes once more as GET.
fn retry_as_get(status: StatusCode, sent_as_post: bool, already_fell_back: bool) -> bool {
    status == StatusCode::METHOD_NOT_ALLOWED && sent_as_post && !already_fell_back
}

fn get_cached_token() -> Option<String> {
    let path = get_cache_file_location();
    let file = std::fs::read_to_string(path);

    file.ok()
}

fn delete_cached_token() {
    let _ = std::fs::remove_file(get_cache_file_location());
}

fn get_cache_file_location() -> PathBuf {
    let default = PathBuf::from(".");
    let mut path = dirs::cache_dir().unwrap_or(default);

    path.push("tpi_token");

    path
}

async fn request_token(
    host: &str,
    ver: ApiVersion,
    creds: &(Option<String>, Option<String>),
    client: &Client,
) -> Result<String> {
    let mut auth_url = url_from_host(host, ver.scheme())?;

    auth_url
        .path_segments_mut()
        .expect("URL cannot be a base")
        .push("authenticate");

    // Save token to a file only if credentials weren't supplied from the command line
    let save_token = creds.0.is_none() && creds.1.is_none();

    let (username, password) = match creds.clone() {
        (Some(username), Some(password)) => (username, password),
        (Some(username), None) => {
            let password = prompt::password("Password")?;
            (username, password)
        }
        (None, Some(password)) => {
            let username = prompt::simple("User")?;
            (username, password)
        }
        (None, None) => {
            let username = prompt::simple("User")?;
            let password = prompt::password("Password")?;
            (username, password)
        }
    };

    let body = serde_json::json!({
        "username": username,
        "password": password
    });

    let resp = client.post(auth_url).json(&body).send().await?;

    match resp.status() {
        StatusCode::OK => {
            let json = resp.json::<serde_json::Value>().await?;
            let token = get_param(&json, "id");

            if save_token {
                if let Err(e) = cache_token(&token) {
                    let path = get_cache_file_location();
                    println!("Warning: failed to write to cache file {:?}: {}", path, e);
                }
            }

            Ok(token)
        }
        StatusCode::FORBIDDEN => bail!(
            "{}",
            resp.text()
                .await
                .unwrap_or("could not authenticate".to_string())
        ),
        x => bail!("Unexpected status code {x}"),
    }
}

fn get_param(results: &serde_json::Value, key: &str) -> String {
    results
        .get(key)
        .unwrap_or_else(|| panic!("API error: Expected `{key}` attribute"))
        .as_str()
        .unwrap_or_else(|| panic!("API error: `{key}` value is not a string"))
        .to_owned()
}

fn cache_token(token: &str) -> Result<()> {
    let path = get_cache_file_location();

    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?
        .write_all(token.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn a_set_goes_as_post_and_a_get_or_an_upload_does_not_change() {
        let set = url("https://board.local/api/bmc?opt=set&type=power&node1=1");
        let get = url("https://board.local/api/bmc?opt=get&type=power");
        let path = url("https://board.local/api/bmc/network/switch");
        assert!(sent_as_post(&Method::GET, &set));
        assert!(!sent_as_post(&Method::GET, &get));
        assert!(!sent_as_post(&Method::GET, &path));
        // The upload is POST already and stays so.
        assert!(!sent_as_post(&Method::POST, &set));
    }

    #[test]
    fn a_405_to_a_post_is_retried_once_as_get() {
        assert!(retry_as_get(StatusCode::METHOD_NOT_ALLOWED, true, false));
        assert!(!retry_as_get(StatusCode::METHOD_NOT_ALLOWED, true, true));
        assert!(!retry_as_get(StatusCode::METHOD_NOT_ALLOWED, false, false));
        assert!(!retry_as_get(StatusCode::OK, true, false));
    }

    #[test]
    fn only_loopback_hosts_count() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:443",
            "[::1]",
            "[::1]:8443",
            "localhost",
            "LOCALHOST:443",
        ] {
            assert!(is_loopback(host), "{host}");
        }
        for host in [
            "turingpi.local",
            "10.0.0.1",
            "127.0.0.2",
            "localhost.example",
        ] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn the_local_token_is_read_only_for_a_loopback_host_without_other_credentials() {
        let token = || Some("local".to_owned());
        assert_eq!(
            local_token_for(true, "127.0.0.1", false, || false, token),
            Some("local".into())
        );
        assert_eq!(
            local_token_for(true, "[::1]:443", false, || false, token),
            Some("local".into())
        );
        let never = || -> Option<String> { panic!("read for a host it must not go to") };
        assert_eq!(
            local_token_for(true, "turingpi.local", false, || false, never),
            None
        );
        assert_eq!(
            local_token_for(false, "127.0.0.1", false, || false, never),
            None
        );
        assert_eq!(
            local_token_for(true, "127.0.0.1", true, || false, never),
            None
        );
        assert_eq!(
            local_token_for(true, "127.0.0.1", false, || true, never),
            None
        );
    }

    #[test]
    fn a_missing_or_empty_local_token_file_means_the_normal_login() {
        let dir = std::env::temp_dir().join(format!("tpi-local-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("local-token");
        assert_eq!(read_local_token(&path), None);
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(read_local_token(&path), None);
        std::fs::write(&path, "abcDEF123\n").unwrap();
        assert_eq!(read_local_token(&path), Some("abcDEF123".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An older bmcd: `opt=set` on POST is 405, on GET it runs. Every
    /// request line is recorded; each response closes its connection.
    fn old_board() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut length = 0usize;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                let line = line.trim().to_owned();
                let (status, body) = if line.contains("/authenticate") {
                    ("200 OK", r#"{"id":"token"}"#)
                } else if line.starts_with("POST ") {
                    ("405 Method Not Allowed", "use GET")
                } else {
                    ("200 OK", r#"{"response":[{"result":"ok"}]}"#)
                };
                log.lock().unwrap().push(line);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (host, seen)
    }

    #[tokio::test]
    async fn an_older_board_gets_the_set_again_as_get_after_405() {
        let (host, seen) = old_board();
        let creds = (Some("root".to_owned()), Some("secret".to_owned()));
        let mut request = Request::new(host, ApiVersion::V1, creds, "test").unwrap();
        request
            .url_mut()
            .query_pairs_mut()
            .append_pair("opt", "set")
            .append_pair("type", "power")
            .append_pair("node1", "1");
        let response = request.send(Client::new()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let calls: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|line| !line.contains("/authenticate"))
            .cloned()
            .collect();
        assert_eq!(
            calls,
            [
                "POST /api/bmc?opt=set&type=power&node1=1 HTTP/1.1",
                "GET /api/bmc?opt=set&type=power&node1=1 HTTP/1.1",
            ]
        );
    }

    #[tokio::test]
    async fn a_get_stays_a_get() {
        let (host, seen) = old_board();
        let creds = (Some("root".to_owned()), Some("secret".to_owned()));
        let mut request = Request::new(host, ApiVersion::V1, creds, "test").unwrap();
        request
            .url_mut()
            .query_pairs_mut()
            .append_pair("opt", "get")
            .append_pair("type", "power");
        let response = request.send(Client::new()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let calls: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|line| !line.contains("/authenticate"))
            .cloned()
            .collect();
        assert_eq!(calls, ["GET /api/bmc?opt=get&type=power HTTP/1.1"]);
    }
}
