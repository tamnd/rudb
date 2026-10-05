//! TLS with `rustls`: the configuration that the server loads at start, and the handshake of one
//! connection.
//!
//! The server loads the certificate and the key once, as `be_tls_init` of PostgreSQL does, and
//! does not start when they do not load. A connection starts TLS in one of two ways: with an
//! `SSLRequest` and the answer `S`, or with a TLS handshake as its first bytes, which is direct
//! TLS. Direct TLS needs the ALPN protocol `postgresql`, as in PostgreSQL 17 and later.

use std::io;
use std::net::TcpStream;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::server::{NoServerSessionStorage, WebPkiClientVerifier};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::{Config, TlsVersion};
use crate::crypto::{self, Digest};

/// The ALPN protocol of PostgreSQL, `PG_ALPN_PROTOCOL` in `pqcomm.h`.
pub(crate) const ALPN: &[u8] = b"postgresql";

/// The first byte of a TLS handshake record. No startup packet starts with it, because the length
/// would be hundreds of megabytes.
pub(crate) const HANDSHAKE_BYTE: u8 = 0x16;

/// A TLS connection over TCP.
pub(crate) type TlsStream = StreamOwned<ServerConnection, TcpStream>;

/// The crypto provider of the build: `aws-lc-rs` by default, `ring` with the feature `tls-ring`.
#[cfg(feature = "tls-aws-lc")]
fn provider() -> CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

#[cfg(all(feature = "tls-ring", not(feature = "tls-aws-lc")))]
fn provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

#[cfg(not(any(feature = "tls-aws-lc", feature = "tls-ring")))]
compile_error!("rudb-server needs the feature tls-aws-lc or the feature tls-ring");

/// A file of the TLS settings, relative to the data directory when it is not absolute.
fn in_data(config: &Config, file: &Path) -> PathBuf {
    if file.is_absolute() { file.to_owned() } else { config.data.join(file) }
}

/// The TLS configuration of the server and the hash of its certificate for SCRAM with channel
/// binding.
pub(crate) type Loaded = (Arc<ServerConfig>, Vec<u8>);

/// The TLS configuration and the certificate hash, or `None` when `ssl` is off.
///
/// # Errors
///
/// The text of PostgreSQL for a certificate or a key that does not load, a key that other users
/// can read, a key that is not the key of the certificate, or a version range that is empty.
pub(crate) fn load(config: &Config) -> Result<Option<Loaded>, String> {
    if !config.ssl {
        return Ok(None);
    }
    let cert_file = &config.ssl_cert_file;
    let certs = CertificateDer::pem_file_iter(in_data(config, cert_file))
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| {
            format!(
                "could not load server certificate file \"{}\": {}",
                cert_file.display(),
                pem_text(&e)
            )
        })?;
    if certs.is_empty() {
        return Err(format!(
            "could not load server certificate file \"{}\": no certificate in the file",
            cert_file.display()
        ));
    }
    let key_file = &config.ssl_key_file;
    check_key_access(&in_data(config, key_file), key_file)?;
    let key = PrivateKeyDer::from_pem_file(in_data(config, key_file)).map_err(|e| {
        format!("could not load private key file \"{}\": {}", key_file.display(), pem_text(&e))
    })?;
    let (min, max) = (config.ssl_min_protocol_version, config.ssl_max_protocol_version);
    if max != TlsVersion::Any && min > max {
        return Err("could not set SSL protocol version range\nDETAIL:  \
                    \"ssl_min_protocol_version\" cannot be higher than \
                    \"ssl_max_protocol_version\"."
            .to_owned());
    }
    let versions: Vec<&'static rustls::SupportedProtocolVersion> = [
        (TlsVersion::Tls12, &rustls::version::TLS12),
        (TlsVersion::Tls13, &rustls::version::TLS13),
    ]
    .into_iter()
    .filter(|(version, _)| *version >= min && (max == TlsVersion::Any || *version <= max))
    .map(|(_, version)| version)
    .collect();
    let provider = Arc::new(provider());
    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&versions)
        .map_err(|e| format!("could not set SSL protocol version range: {e}"))?;
    let builder = if config.ssl_ca_file.as_os_str().is_empty() {
        builder.with_no_client_auth()
    } else {
        let ca_file = &config.ssl_ca_file;
        let failed = |e: &dyn std::fmt::Display| {
            format!("could not load root certificate file \"{}\": {e}", ca_file.display())
        };
        let mut roots = RootCertStore::empty();
        let certs = CertificateDer::pem_file_iter(in_data(config, ca_file))
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(|e| failed(&pem_text(&e)))?;
        for cert in certs {
            roots.add(cert).map_err(|e| failed(&e))?;
        }
        // PostgreSQL asks for a client certificate when it has root certificates, and lets a
        // client without one go on to `pg_hba.conf`.
        let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
            .allow_unauthenticated()
            .build()
            .map_err(|e| failed(&e))?;
        builder.with_client_cert_verifier(verifier)
    };
    let hash = certificate_hash(&certs[0]);
    let mut tls = builder.with_single_cert(certs, key).map_err(|e| match e {
        rustls::Error::InconsistentKeys(_) => format!("check of private key failed: {e}"),
        _ => format!("could not load private key file \"{}\": {e}", key_file.display()),
    })?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    // `ssl_prefer_server_ciphers` is on by default.
    tls.ignore_client_order = true;
    // PostgreSQL turns off session resumption, both the cache and the tickets.
    tls.session_storage = Arc::new(NoServerSessionStorage {});
    tls.send_tls13_tickets = 0;
    Ok(Some((Arc::new(tls), hash)))
}

/// The hash of the certificate for `tls-server-end-point`, as `be_tls_get_certificate_hash`
/// makes it: the hash of the signature algorithm of the certificate, with SHA-256 in place of MD5
/// and SHA-1 as RFC 5929 asks. Other algorithms get SHA-256 too.
fn certificate_hash(cert: &[u8]) -> Vec<u8> {
    // The OIDs of sha384WithRSAEncryption, sha512WithRSAEncryption, ecdsa-with-SHA384 and
    // ecdsa-with-SHA512.
    const SHA384: [&[u8]; 2] = [
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c],
        &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03],
    ];
    const SHA512: [&[u8]; 2] = [
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d],
        &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04],
    ];
    let algorithm = signature_algorithm(cert).unwrap_or_default();
    let digest = if SHA384.contains(&algorithm) {
        Digest::Sha384
    } else if SHA512.contains(&algorithm) {
        Digest::Sha512
    } else {
        Digest::Sha256
    };
    crypto::digest(digest, cert)
}

/// The OID of `signatureAlgorithm` in a certificate: the second item of the outer sequence.
fn signature_algorithm(cert: &[u8]) -> Option<&[u8]> {
    let (_, certificate, _) = der_item(cert)?;
    let (_, _, rest) = der_item(certificate)?;
    let (_, algorithm, _) = der_item(rest)?;
    let (tag, oid, _) = der_item(algorithm)?;
    (tag == 0x06).then_some(oid)
}

/// The first DER item of `input`: its tag, its content and the bytes after it.
fn der_item(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > size_of::<usize>() || rest.len() < count {
            return None;
        }
        let len = rest[..count].iter().fold(0usize, |len, &b| len << 8 | usize::from(b));
        (len, &rest[count..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// The text of an error of a PEM file. An error of the system is the text of `strerror`, as in
/// the messages of PostgreSQL.
fn pem_text(error: &rustls_pki_types::pem::Error) -> String {
    match error {
        rustls_pki_types::pem::Error::Io(e) => os_text(e),
        other => other.to_string(),
    }
}

/// The text of an I/O error without the number that Rust adds, `No such file or directory`.
pub(crate) fn os_text(error: &io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

/// The check of `be_tls_init` that other users cannot read the key: the owner of the file must be
/// the user of the server or root, and the mode at most 0600, or 0640 for root.
fn check_key_access(path: &Path, shown: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| {
        format!("could not access private key file \"{}\": {}", shown.display(), os_text(&e))
    })?;
    if !meta.is_file() {
        return Err(format!("private key file \"{}\" is not a regular file", shown.display()));
    }
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    let (owner, mode) = (meta.uid(), meta.mode());
    if owner != me && owner != 0 {
        return Err(format!(
            "private key file \"{}\" must be owned by the database user or root",
            shown.display()
        ));
    }
    if (owner == me && mode & 0o077 != 0) || (owner == 0 && mode & 0o037 != 0) {
        return Err(format!(
            "private key file \"{}\" has group or world access\nDETAIL:  File must have \
             permissions u=rw (0600) or less if owned by the database user, or permissions \
             u=rw,g=r (0640) or less if owned by root.",
            shown.display()
        ));
    }
    Ok(())
}

/// Runs the TLS handshake on `socket`. `early` holds the bytes of the handshake that the server
/// already read, for direct TLS. An error is the text of the log line of PostgreSQL.
pub(crate) fn accept(
    config: &Arc<ServerConfig>,
    socket: TcpStream,
    mut early: &[u8],
) -> Result<TlsStream, String> {
    let failed = |e: &dyn std::fmt::Display| format!("could not accept SSL connection: {e}");
    let mut conn = ServerConnection::new(config.clone()).map_err(|e| failed(&e))?;
    while !early.is_empty() {
        conn.read_tls(&mut early).map_err(|e| failed(&e))?;
        conn.process_new_packets().map_err(|e| failed(&e))?;
    }
    let mut stream = StreamOwned::new(conn, socket);
    while stream.conn.is_handshaking() {
        match stream.conn.complete_io(&mut stream.sock) {
            Ok((0, 0)) => return Err(failed(&"EOF detected")),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(failed(&"EOF detected"));
            }
            Err(e) => return Err(failed(&e)),
        }
    }
    Ok(stream)
}
