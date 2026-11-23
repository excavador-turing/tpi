//! TLS for the workstation client.
//!
//! Verification is on. A board's certificate is usually self-signed, so the
//! operator compares its SHA-256 fingerprint with what the board shows and
//! passes it once as `--pin`. tpi accepts it only if the certificate presented
//! in that same handshake has that fingerprint, and then remembers the SHA-256
//! of the certificate's public key (SubjectPublicKeyInfo) for that
//! `scheme://host:port`. The board keeps its key when it renews its own
//! certificate, so the pin outlives a renewal.
//!
//! `--insecure` is the only other way to skip. The localhost build may also
//! skip, and only for `127.0.0.1` and `::1`, which is the on-board client
//! talking to itself.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
};

/// Where the board keeps the certificate it serves.
const BOARD_CERT_PATH: &str = "/etc/ssl/certs/bmcd_cert.pem";

/// A SHA-256 digest: of a certificate, or of its public key.
pub(crate) type Digest = [u8; 32];

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

pub(crate) fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(host)
    } else if let Some((name, port)) = host.rsplit_once(':') {
        if !name.is_empty() && !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) {
            name
        } else {
            host
        }
    } else {
        host
    };
    name == "127.0.0.1" || name == "::1"
}

/// `--insecure` skips for any host. A localhost build skips only loopback.
pub(crate) fn skip_verification(host: &str, insecure: bool, localhost_build: bool) -> bool {
    insecure || (localhost_build && is_loopback_host(host))
}

/// The colon-separated SHA-256 that `openssl x509 -fingerprint -sha256` and
/// the board's web interface print. Colons and spaces are optional; the case
/// does not matter.
pub(crate) fn parse_fingerprint(text: &str) -> Result<Digest> {
    let hex_digits: String = text
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != ':')
        .collect();
    let bytes = hex::decode(&hex_digits)
        .map_err(|_| anyhow!("--pin is not a SHA-256 fingerprint: {text}"))?;
    let pin: Digest = bytes.try_into().map_err(|bytes: Vec<u8>| {
        anyhow!(
            "--pin is {} hex digits, and a SHA-256 fingerprint is 64: {text}",
            bytes.len() * 2
        )
    })?;
    Ok(pin)
}

pub(crate) fn format_fingerprint(digest: &Digest) -> String {
    digest
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn sha256(bytes: &[u8]) -> Digest {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// The certificate's SHA-256 fingerprint, the one the operator compares.
pub(crate) fn cert_fingerprint(cert: &CertificateDer<'_>) -> Digest {
    sha256(cert.as_ref())
}

/// SHA-256 of the certificate's DER SubjectPublicKeyInfo, which is the pin.
pub(crate) fn spki_hash(cert: &CertificateDer<'_>) -> Result<Digest, Error> {
    let parsed = webpki::EndEntityCert::try_from(cert)
        .map_err(|_| Error::InvalidCertificate(CertificateError::BadEncoding))?;
    Ok(sha256(parsed.subject_public_key_info().as_ref()))
}

/// What is remembered for one endpoint: the public-key hash that is checked,
/// and the certificate fingerprint seen when it was pinned, kept only so a
/// mismatch can name what the operator once compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StoredPin {
    spki: Digest,
    cert: Option<Digest>,
}

impl StoredPin {
    fn to_text(self) -> String {
        let mut text = format!("spki-sha256 {}\n", format_fingerprint(&self.spki));
        if let Some(cert) = self.cert {
            text.push_str(&format!("cert-sha256 {}\n", format_fingerprint(&cert)));
        }
        text
    }

    fn from_text(text: &str) -> Option<Self> {
        let mut spki = None;
        let mut cert = None;
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("spki-sha256", value)) => spki = Some(parse_fingerprint(value).ok()?),
                Some(("cert-sha256", value)) => cert = Some(parse_fingerprint(value).ok()?),
                _ if line.trim().is_empty() => {}
                _ => return None,
            }
        }
        Some(Self { spki: spki?, cert })
    }
}

/// The endpoint being checked, and what to tell the operator about it.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    endpoint: String,
    /// `--host <name> [--port <n>]`, as the operator would type it again.
    host_args: String,
    /// The pin file; `None` without a cache directory.
    pin_file: Option<PathBuf>,
}

impl Target {
    pub(crate) fn new(host: &str, scheme: &str, typed_host: &str, port: Option<u16>) -> Self {
        let endpoint = endpoint_key(host, scheme);
        let pin_file = dirs::cache_dir().map(|root| pin_file(&root, &endpoint));
        Self::with_pin_file(endpoint, typed_host, port, pin_file)
    }

    fn with_pin_file(
        endpoint: String,
        typed_host: &str,
        port: Option<u16>,
        pin_file: Option<PathBuf>,
    ) -> Self {
        let mut host_args = format!("--host {typed_host}");
        if let Some(port) = port {
            host_args.push_str(&format!(" --port {port}"));
        }
        Self {
            endpoint,
            host_args,
            pin_file,
        }
    }
}

/// What the certificate is checked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expected {
    /// The WebPKI roots.
    Roots,
    /// A pin remembered from an earlier `--pin`.
    Stored(StoredPin),
    /// A certificate fingerprint given with `--pin` on this run.
    Given(Digest),
}

/// How the client treats certificates.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Skip,
    Verify(Expected),
}

/// Decide before connecting. A skip never reads the pin file, so a file tpi
/// cannot parse does not stand in the way of `--insecure`, and `--pin` replaces
/// it rather than tripping over it.
pub(crate) fn mode(
    target: &Target,
    host: &str,
    insecure: bool,
    localhost_build: bool,
    given: Option<Digest>,
) -> Result<Mode> {
    if skip_verification(host, insecure, localhost_build) {
        return Ok(Mode::Skip);
    }
    if let Some(given) = given {
        return Ok(Mode::Verify(Expected::Given(given)));
    }
    let stored = match &target.pin_file {
        Some(path) => read_pin(path)?,
        None => None,
    };
    Ok(Mode::Verify(
        stored.map_or(Expected::Roots, Expected::Stored),
    ))
}

/// Why a handshake was refused, recorded by the verifier so the command's
/// error can say it instead of "error sending request".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Untrusted {
        presented: Digest,
    },
    PinChanged {
        pinned: StoredPin,
        presented: Digest,
    },
    NotTheGivenPin {
        given: Digest,
        presented: Digest,
    },
}

/// Shared between the verifier and the command that made the request.
#[derive(Debug)]
pub(crate) struct Report {
    target: Target,
    failure: Mutex<Option<Failure>>,
    pinned: Mutex<bool>,
}

impl Report {
    fn new(target: Target) -> Self {
        Self {
            target,
            failure: Mutex::new(None),
            pinned: Mutex::new(false),
        }
    }

    fn record(&self, failure: Failure) {
        if let Ok(mut slot) = self.failure.lock() {
            slot.get_or_insert(failure);
        }
    }

    fn failure(&self) -> Option<Failure> {
        self.failure.lock().ok().and_then(|slot| *slot)
    }

    /// The recorded refusal in place of the transport error, if there is one.
    pub(crate) fn explain(&self, error: anyhow::Error) -> anyhow::Error {
        match self.failure() {
            Some(failure) => anyhow!(describe(&self.target, &failure)),
            None => error,
        }
    }

    /// Remember the key of the certificate the operator vouched for. Once per run.
    fn remember(&self, cert: &CertificateDer<'_>) -> Result<(), Error> {
        let spki = spki_hash(cert)?;
        let Ok(mut pinned) = self.pinned.lock() else {
            return Ok(());
        };
        if *pinned {
            return Ok(());
        }
        *pinned = true;
        let pin = StoredPin {
            spki,
            cert: Some(cert_fingerprint(cert)),
        };
        let result = match &self.target.pin_file {
            Some(path) => write_pin(path, pin),
            None => Err(anyhow!(
                "no cache directory, so the pin cannot be remembered. Pass --pin again next time"
            )),
        };
        if let Err(error) = result {
            eprintln!("Warning: {error:#}");
        }
        Ok(())
    }
}

fn check_on_board() -> String {
    format!(
        "BMC-UI › Security › Certificates, or on the serial console:\n  \
         openssl x509 -in {BOARD_CERT_PATH} -noout -fingerprint -sha256"
    )
}

fn describe(target: &Target, failure: &Failure) -> String {
    let endpoint = &target.endpoint;
    match failure {
        Failure::Untrusted { presented } => {
            let presented = format_fingerprint(presented);
            format!(
                "error: {endpoint} presented a certificate tpi does not trust.\n  \
                 SHA-256: {presented}\n  \
                 Check it on the board ({}).\n  \
                 If it matches:  tpi {} --pin {presented} info",
                check_on_board(),
                target.host_args
            )
        }
        Failure::PinChanged { pinned, presented } => {
            let pinned = match &pinned.cert {
                Some(cert) => format_fingerprint(cert),
                None => format!("public key SHA-256 {}", format_fingerprint(&pinned.spki)),
            };
            format!(
                "error: the certificate of {endpoint} no longer matches its pin.\n  \
                 pinned:    {pinned}\n  \
                 presented: {}\n  \
                 The pin is the certificate's key. `tpi tls install` replaces it, and older\n  \
                 firmware made a new key each time it renewed its own certificate, 30 days\n  \
                 before expiry. If one of those happened, check the new fingerprint on the\n  \
                 board ({}),\n  \
                 and re-run with --pin. Otherwise the connection may be intercepted.",
                format_fingerprint(presented),
                check_on_board()
            )
        }
        Failure::NotTheGivenPin { given, presented } => format!(
            "error: {endpoint} presented a certificate that is not the one given with --pin.\n  \
             --pin:     {}\n  \
             presented: {}\n  \
             Nothing was remembered. Check the fingerprint on the board ({}).",
            format_fingerprint(given),
            format_fingerprint(presented),
            check_on_board()
        ),
    }
}

fn pin_file(root: &Path, key: &str) -> PathBuf {
    root.join("tpi").join("pins").join(hex::encode(key))
}

fn read_pin(path: &Path) -> Result<Option<StoredPin>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!(
                "{} is a symbolic link, which tpi does not follow",
                path.display()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match StoredPin::from_text(&text) {
        Some(pin) => Ok(Some(pin)),
        None => anyhow::bail!(
            "{} is not a pin this tpi wrote. Check the fingerprint on the board and pass \
             --pin to replace it, or remove the file",
            path.display()
        ),
    }
}

fn write_pin(path: &Path, pin: StoredPin) -> Result<()> {
    if let Some(pins) = path.parent() {
        if let Some(tpi) = pins.parent() {
            // The cache directory itself is the system's, and may not exist yet.
            if let Some(root) = tpi.parent() {
                fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
            }
            private_dir(tpi)?;
        }
        private_dir(pins)?;
    }
    write_private(path, pin.to_text().as_bytes())
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
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
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

/// A client config whose verifier checks `expected` and records any refusal
/// in the returned report.
pub(crate) fn client_config(
    target: Target,
    expected: Expected,
) -> Result<(ClientConfig, Arc<Report>)> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let report = Arc::new(Report::new(target));
    let verifier = Arc::new(PinningVerifier {
        expected,
        inner: webpki_verifier(provider.clone())?,
        report: report.clone(),
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| anyhow!("TLS versions: {error}"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok((config, report))
}

fn webpki_verifier(
    provider: Arc<rustls::crypto::CryptoProvider>,
) -> Result<Arc<WebPkiServerVerifier>> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|error| anyhow!("certificate verifier: {error}"))
}

#[derive(Debug)]
struct PinningVerifier {
    expected: Expected,
    inner: Arc<WebPkiServerVerifier>,
    report: Arc<Report>,
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let presented = cert_fingerprint(end_entity);
        match self.expected {
            Expected::Given(given) => {
                if presented != given {
                    self.report
                        .record(Failure::NotTheGivenPin { given, presented });
                    return Err(Error::General(
                        "the certificate is not the one given with --pin".into(),
                    ));
                }
                self.report.remember(end_entity)?;
                Ok(ServerCertVerified::assertion())
            }
            Expected::Stored(pinned) => {
                if spki_hash(end_entity).ok() != Some(pinned.spki) {
                    self.report
                        .record(Failure::PinChanged { pinned, presented });
                    return Err(Error::General(
                        "the certificate's key does not match the pin for this host".into(),
                    ));
                }
                Ok(ServerCertVerified::assertion())
            }
            Expected::Roots => self
                .inner
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .inspect_err(|_| self.report.record(Failure::Untrusted { presented })),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed P-384 certificate for turingpi.local, like the board's.
    const BOARD_CERT: &[u8] = include_bytes!("testdata/board.der");
    /// `openssl x509 -noout -fingerprint -sha256` of it.
    const BOARD_CERT_SHA256: &str = "74:41:FC:E8:4B:D8:65:AA:DB:22:BD:89:63:A7:46:C8:\
                                     A0:73:32:46:29:A4:61:FA:16:42:C7:DA:C8:D8:E8:F6";
    /// `openssl x509 -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256`.
    const BOARD_SPKI_SHA256: &str =
        "e19817d2be6a5c247499746fbbf652aa29b33d6175c3ac96831cce41d9f89e50";

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("tpi-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        root
    }

    fn target(root: &Path) -> Target {
        let endpoint = endpoint_key("turingpi.local", "https");
        let file = pin_file(root, &endpoint);
        Target::with_pin_file(endpoint, "turingpi.local", None, Some(file))
    }

    fn verify(expected: Expected, target: Target) -> (Result<(), Error>, Arc<Report>) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let report = Arc::new(Report::new(target));
        let verifier = PinningVerifier {
            expected,
            inner: webpki_verifier(provider).unwrap(),
            report: report.clone(),
        };
        let cert = CertificateDer::from(BOARD_CERT);
        let name = ServerName::try_from("turingpi.local").unwrap();
        let result = verifier
            .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
            .map(|_| ());
        (result, report)
    }

    #[test]
    fn a_fingerprint_parses_with_or_without_colons() {
        let text = "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:\
                    AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99";
        let pin = parse_fingerprint(text).unwrap();
        assert_eq!(pin[0], 0xAA);
        assert_eq!(pin[31], 0x99);
        assert_eq!(parse_fingerprint(&format_fingerprint(&pin)).unwrap(), pin);
        let flat: String = text.chars().filter(|c| *c != ':').collect();
        assert_eq!(parse_fingerprint(&flat.to_ascii_lowercase()).unwrap(), pin);
    }

    #[test]
    fn a_short_fingerprint_is_refused() {
        let error = parse_fingerprint("AA:BB").unwrap_err();
        assert!(format!("{error:#}").contains("64"), "{error:#}");
    }

    #[test]
    fn the_fingerprint_and_the_key_hash_match_openssl() {
        let cert = CertificateDer::from(BOARD_CERT);
        assert_eq!(
            format_fingerprint(&cert_fingerprint(&cert)),
            BOARD_CERT_SHA256
        );
        assert_eq!(hex::encode(spki_hash(&cert).unwrap()), BOARD_SPKI_SHA256);
    }

    #[test]
    fn only_loopback_literals_count() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.1:443"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("[::1]:443"));
        assert!(!is_loopback_host("localhost"));
        assert!(!is_loopback_host("turingpi.local"));
        assert!(!is_loopback_host("10.0.0.1"));
    }

    #[test]
    fn insecure_skips_any_host_and_the_localhost_build_skips_only_loopback() {
        assert!(skip_verification("turingpi.local", true, false));
        assert!(!skip_verification("127.0.0.1", false, false));
        assert!(skip_verification("127.0.0.1", false, true));
        assert!(skip_verification("[::1]:443", false, true));
        assert!(!skip_verification("turingpi.local", false, true));
    }

    #[test]
    fn the_same_host_with_and_without_the_default_port_is_one_key() {
        assert_eq!(
            endpoint_key("turingpi.local", "https"),
            endpoint_key("turingpi.local:443", "https")
        );
        assert_eq!(
            endpoint_key("turingpi.local", "https"),
            "https://turingpi.local:443"
        );
    }

    #[test]
    fn a_pin_is_stored_mode_0600_in_a_directory_mode_0700() {
        let root = scratch("pin-mode");
        // The cache directory need not exist yet.
        let path = pin_file(&root.join("cache"), "https://board.local:443");
        let pin = StoredPin {
            spki: [7u8; 32],
            cert: Some([8u8; 32]),
        };
        write_pin(&path, pin).unwrap();
        assert_eq!(read_pin(&path).unwrap(), Some(pin));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&root.join("cache").join("tpi")), 0o700);
            assert_eq!(mode(&root.join("cache").join("tpi").join("pins")), 0o700);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_at_the_pin_path_is_replaced_and_not_written_through() {
        let root = scratch("pin-link");
        let path = pin_file(&root, "https://board.local:443");
        private_dir(&root.join("tpi")).unwrap();
        private_dir(path.parent().unwrap()).unwrap();
        let victim = root.join("victim");
        fs::write(&victim, "untouched").unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        assert!(read_pin(&path).is_err());
        let pin = StoredPin {
            spki: [7u8; 32],
            cert: None,
        };
        write_pin(&path, pin).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(!fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(read_pin(&path).unwrap(), Some(pin));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_pin_file_does_not_block_insecure_or_pin() {
        let root = scratch("pin-corrupt");
        let target = target(&root);
        let path = target.pin_file.clone().unwrap();
        private_dir(&root.join("tpi")).unwrap();
        private_dir(path.parent().unwrap()).unwrap();
        fs::write(&path, "AA:BB\n").unwrap();
        let error = mode(&target, "turingpi.local", false, false, None).unwrap_err();
        assert!(format!("{error:#}").contains("--pin"), "{error:#}");
        assert_eq!(
            mode(&target, "turingpi.local", true, false, None).unwrap(),
            Mode::Skip
        );
        assert_eq!(
            mode(&target, "turingpi.local", false, false, Some([1u8; 32])).unwrap(),
            Mode::Verify(Expected::Given([1u8; 32]))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_matching_pin_stores_the_key_hash() {
        let root = scratch("pin-given");
        let target = target(&root);
        let path = target.pin_file.clone().unwrap();
        let given = parse_fingerprint(BOARD_CERT_SHA256).unwrap();
        let (result, report) = verify(Expected::Given(given), target.clone());
        assert!(result.is_ok());
        assert_eq!(report.failure(), None);
        let stored = read_pin(&path).unwrap().unwrap();
        assert_eq!(hex::encode(stored.spki), BOARD_SPKI_SHA256);
        assert_eq!(stored.cert, Some(given));
        // And the next run, reading it back, accepts the same certificate.
        let expected = match mode(&target, "turingpi.local", false, false, None).unwrap() {
            Mode::Verify(expected) => expected,
            Mode::Skip => panic!("skipped"),
        };
        assert_eq!(expected, Expected::Stored(stored));
        assert!(verify(expected, target).0.is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_mismatched_pin_is_not_stored() {
        let root = scratch("pin-typo");
        let target = target(&root);
        let path = target.pin_file.clone().unwrap();
        let typo = [0x74u8; 32];
        let (result, report) = verify(Expected::Given(typo), target.clone());
        assert!(result.is_err());
        assert!(!path.exists());
        let error = report.explain(anyhow!("error sending request"));
        let text = format!("{error:#}");
        assert!(text.contains("not the one given with --pin"), "{text}");
        assert!(text.contains(BOARD_CERT_SHA256), "{text}");
        assert!(text.contains("Nothing was remembered"), "{text}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_certificate_says_how_to_check_and_pin_it() {
        let root = scratch("pin-unknown");
        let (result, report) = verify(Expected::Roots, target(&root));
        assert!(result.is_err());
        let text = format!("{:#}", report.explain(anyhow!("error sending request")));
        assert_eq!(
            text,
            format!(
                "error: https://turingpi.local:443 presented a certificate tpi does not trust.\n  \
                 SHA-256: {BOARD_CERT_SHA256}\n  \
                 Check it on the board (BMC-UI › Security › Certificates, or on the serial console:\n  \
                 openssl x509 -in /etc/ssl/certs/bmcd_cert.pem -noout -fingerprint -sha256).\n  \
                 If it matches:  tpi --host turingpi.local --pin {BOARD_CERT_SHA256} info"
            )
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_changed_key_names_both_fingerprints() {
        let root = scratch("pin-changed");
        let pinned = StoredPin {
            spki: [3u8; 32],
            cert: Some([4u8; 32]),
        };
        let (result, report) = verify(Expected::Stored(pinned), target(&root));
        assert!(result.is_err());
        let text = format!("{:#}", report.explain(anyhow!("error sending request")));
        assert!(
            text.starts_with(
                "error: the certificate of https://turingpi.local:443 no longer matches its pin."
            ),
            "{text}"
        );
        assert!(
            text.contains(&format!("pinned:    {}", format_fingerprint(&[4u8; 32]))),
            "{text}"
        );
        assert!(
            text.contains(&format!("presented: {BOARD_CERT_SHA256}")),
            "{text}"
        );
        assert!(text.contains("tpi tls install"), "{text}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn no_failure_leaves_the_error_alone() {
        let root = scratch("pin-none");
        let report = Report::new(target(&root));
        let text = format!("{:#}", report.explain(anyhow!("timed out")));
        assert_eq!(text, "timed out");
        let _ = fs::remove_dir_all(&root);
    }
}
