//! Files tpi keeps per endpoint under the user's cache directory: the token
//! (request.rs) and the certificate pin (tls_verify.rs). Both are private to
//! the user and keyed the same way.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// `https://host:port`, with a bracketed IPv6 host. The default port is filled
/// in, so `turingpi.local` and `turingpi.local:443` share one pin.
pub(crate) fn endpoint_key(host: &str, scheme: &str) -> String {
    match url::Url::parse(&format!("{scheme}://{host}/")) {
        Ok(url) => {
            let port = url.port_or_known_default().unwrap_or(0);
            let name = url.host_str().unwrap_or(host);
            if name.contains(':') {
                format!("{scheme}://[{name}]:{port}")
            } else {
                format!("{scheme}://{name}:{port}")
            }
        }
        Err(_) => format!("{scheme}://{host}"),
    }
}

/// Create `path` mode 0700, or tighten it to 0700 if it is already a
/// directory. A symbolic link is refused, not followed.
pub(crate) fn private_dir(path: &Path) -> Result<()> {
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
                anyhow::bail!(
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
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
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
