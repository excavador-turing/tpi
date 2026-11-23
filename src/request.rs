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

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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

        let resp = loop {
            let mut builder =
                RequestBuilder::from_parts(client.clone(), self.inner.try_clone().unwrap());

            if authenticated {
                let token = self.get_bearer_token(&client).await?;
                builder = builder.bearer_auth(token);
            }

            if let Some(form) = self.multipart.take() {
                builder = builder.multipart(form);
            }

            let resp = builder.send().await?;
            if resp.status() == StatusCode::UNAUTHORIZED {
                delete_cached_token(&self.host, self.ver);
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

        // Else, try retrieving cached token from a file. A token is never
        // read for plain HTTP: the cache is per https host, and an http
        // request must not present it.
        if let Some(token) = get_cached_token(&self.host, self.ver) {
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

/// `scheme://host:port`, default port included, IPv6 bracketed.
fn token_endpoint(host: &str, ver: ApiVersion) -> String {
    match url_from_host(host, ver.scheme()) {
        Ok(url) => {
            let port = url.port_or_known_default().unwrap_or(0);
            let name = url.host_str().unwrap_or(host);
            if name.contains(':') {
                format!("{}://[{name}]:{port}", ver.scheme())
            } else {
                format!("{}://{name}:{port}", ver.scheme())
            }
        }
        Err(_) => format!("{}://{host}", ver.scheme()),
    }
}

fn get_cached_token(host: &str, ver: ApiVersion) -> Option<String> {
    if ver == ApiVersion::V1 {
        return None;
    }
    let path = token_file(host, ver)?;
    fs::read_to_string(path).ok()
}

fn delete_cached_token(host: &str, ver: ApiVersion) {
    if let Some(path) = token_file(host, ver) {
        let _ = fs::remove_file(path);
    }
}

/// None when there is no cache directory. The token is not written to `.`.
fn token_file(host: &str, ver: ApiVersion) -> Option<PathBuf> {
    dirs::cache_dir().map(|root| token_file_at(&root, &token_endpoint(host, ver)))
}

fn token_file_at(root: &Path, key: &str) -> PathBuf {
    root.join("tpi").join("tokens").join(hex::encode(key))
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
                if let Err(e) = cache_token(host, ver, &token) {
                    println!("Warning: the token was not cached: {e:#}");
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

fn cache_token(host: &str, ver: ApiVersion, token: &str) -> Result<()> {
    cache_token_in(
        dirs::cache_dir().as_deref(),
        ver,
        &token_endpoint(host, ver),
        token,
    )
}

fn cache_token_in(root: Option<&Path>, ver: ApiVersion, key: &str, token: &str) -> Result<()> {
    if ver == ApiVersion::V1 {
        return Ok(());
    }
    let Some(root) = root else {
        anyhow::bail!("no cache directory, so the token was not written");
    };
    cache_token_at(root, key, token)
}

/// The single token file of earlier versions, sent to whatever --host was in
/// use. Removed once a per-host token has been written in its place.
const LEGACY_TOKEN_FILE: &str = "tpi_token";

fn cache_token_at(root: &Path, key: &str, token: &str) -> Result<()> {
    let path = token_file_at(root, key);
    // The cache directory itself is the system's, and may not exist yet.
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let tpi = root.join("tpi");
    private_dir(&tpi)?;
    private_dir(&tpi.join("tokens"))?;
    write_private(&path, token.as_bytes())?;
    let _ = fs::remove_file(root.join(LEGACY_TOKEN_FILE));
    Ok(())
}

/// Create `path` mode 0700, or tighten it to 0700 if it is already a
/// directory. A symbolic link is refused, not followed.
fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path)
                .with_context(|| format!("checking {}", path.display()))?;
            if !meta.is_dir() {
                bail!(
                    "{} is not a directory (a symbolic link is not followed)",
                    path.display()
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("restricting {}", path.display()))?;
            }
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("creating {}", path.display())),
    }
}

/// Write mode 0600 to a fresh temporary name, then rename it over `path`.
/// `create_new` refuses an existing name, link or not, and the rename
/// replaces a link at `path` rather than writing through it.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?;
    let temp = path.with_file_name(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_file(&temp);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temp, path));
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(error).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_keyed_by_scheme_host_and_port() {
        assert_eq!(
            token_endpoint("board.local", ApiVersion::V1_1),
            "https://board.local:443"
        );
        assert_ne!(
            token_endpoint("board.local", ApiVersion::V1_1),
            token_endpoint("other.local", ApiVersion::V1_1)
        );
        assert_ne!(
            token_endpoint("board.local", ApiVersion::V1_1),
            token_endpoint("board.local:8443", ApiVersion::V1_1)
        );
    }

    #[test]
    fn a_cached_token_is_mode_0600_in_a_directory_mode_0700() {
        let root = std::env::temp_dir().join(format!("tpi-token-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let key = token_endpoint("board.local", ApiVersion::V1_1);
        cache_token_at(&root, &key, "secret").unwrap();
        let path = token_file_at(&root, &key);
        assert_eq!(fs::read_to_string(&path).unwrap(), "secret");
        let other = token_file_at(&root, &token_endpoint("other.local", ApiVersion::V1_1));
        assert!(!other.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&root.join("tpi")), 0o700);
            assert_eq!(mode(&root.join("tpi").join("tokens")), 0o700);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_first_per_host_token_removes_the_old_single_file() {
        let root = std::env::temp_dir().join(format!("tpi-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // The cache directory need not exist yet; then there is nothing to remove.
        let key = token_endpoint("board.local", ApiVersion::V1_1);
        cache_token_at(&root, &key, "first").unwrap();
        fs::write(root.join(LEGACY_TOKEN_FILE), "old").unwrap();
        cache_token_at(&root, &key, "secret").unwrap();
        assert!(!root.join(LEGACY_TOKEN_FILE).exists());
        assert_eq!(
            fs::read_to_string(token_file_at(&root, &key)).unwrap(),
            "secret"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_at_the_token_path_is_replaced_and_not_written_through() {
        let root = std::env::temp_dir().join(format!("tpi-token-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let key = token_endpoint("board.local", ApiVersion::V1_1);
        cache_token_at(&root, &key, "first").unwrap();
        let path = token_file_at(&root, &key);
        fs::remove_file(&path).unwrap();
        let victim = root.join("victim");
        fs::write(&victim, "untouched").unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        cache_token_at(&root, &key, "secret").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(!fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&path).unwrap(), "secret");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn http_never_reads_or_writes_a_token_and_no_cache_dir_means_no_file() {
        assert!(get_cached_token("board.local", ApiVersion::V1).is_none());
        assert!(cache_token_in(None, ApiVersion::V1, "http://board.local:80", "secret").is_ok());
        let error = cache_token_in(None, ApiVersion::V1_1, "https://board.local:443", "secret")
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("no cache directory"),
            "{error:#}"
        );
        assert!(!PathBuf::from("tpi_token").exists());
    }
}
