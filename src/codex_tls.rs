//! The local TLS identity that lets Codex read its usage through the proxy.
//!
//! Codex repaints its status line only from account-usage reads, which go to
//! `chatgpt_base_url` with the session's own login - so behind the proxy it
//! showed the quota of an account nobody was paying with. Pointing that URL at
//! the proxy is the only lever, and Codex accepts it only over HTTPS. So the
//! proxy also listens on TLS, with a certificate Codex is told to trust through
//! `CODEX_CA_CERTIFICATE` (added to its system roots, not replacing them).
//!
//! Codex will trust this CA for every HTTPS connection it makes, so the CA is
//! built to be useless for anything else: it is name-constrained to 127.0.0.1
//! and localhost, and its private key exists only in memory for the one
//! signature it makes. Nothing that can mint a certificate is ever on disk.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How long the server certificate is valid for, and how early it is replaced.
const VALID_DAYS: i64 = 397;
const RENEW_AFTER_DAYS: i64 = 300;

/// `<store>/codex-tls`, holding `ca.pem`, `server.pem`, `server.key`, `issued`.
pub fn dir(paths: &crate::paths::Paths) -> PathBuf {
    paths.store_dir().join("codex-tls")
}

/// The CA certificate Codex is pointed at with `CODEX_CA_CERTIFICATE`.
pub fn ca_path(paths: &crate::paths::Paths) -> PathBuf {
    dir(paths).join("ca.pem")
}

/// The TLS configuration for the usage listener, creating or renewing the
/// certificate first when it is missing or old.
pub fn server_config(
    paths: &crate::paths::Paths,
) -> Result<Arc<tokio_rustls::rustls::ServerConfig>> {
    let dir = dir(paths);
    if needs_issue(&dir, now_secs()) {
        issue(&dir, now_secs())?;
    }
    load(&dir)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn needs_issue(dir: &Path, now: i64) -> bool {
    let complete = ["ca.pem", "server.pem", "server.key"]
        .iter()
        .all(|f| dir.join(f).is_file());
    let issued = std::fs::read_to_string(dir.join("issued"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    !complete || issued.is_none_or(|at| now - at >= RENEW_AFTER_DAYS * 86_400 || at > now)
}

/// Mint a fresh CA and server certificate. The CA key is dropped at the end of
/// this function and never serialised.
fn issue(dir: &Path, now: i64) -> Result<()> {
    use rcgen::{
        BasicConstraints, CertificateParams, CidrSubnet, DnType, ExtendedKeyUsagePurpose,
        GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints,
    };
    std::fs::create_dir_all(dir).context("create codex-tls dir")?;
    set_mode(dir, 0o700);

    let (before, after) = (civil(now - 86_400), civil(now + VALID_DAYS * 86_400));

    let mut ca = CertificateParams::new(Vec::<String>::new())?;
    ca.distinguished_name
        .push(DnType::CommonName, "swapdex local usage CA");
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::V4([127, 0, 0, 1], [255, 255, 255, 255])),
            GeneralSubtree::DnsName("localhost".into()),
        ],
        excluded_subtrees: Vec::new(),
    });
    ca.not_before = rcgen::date_time_ymd(before.0, before.1, before.2);
    ca.not_after = rcgen::date_time_ymd(after.0, after.1, after.2);
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca.self_signed(&ca_key)?;
    let issuer = Issuer::new(ca, ca_key);

    let mut leaf = CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()])?;
    leaf.distinguished_name
        .push(DnType::CommonName, "swapdex local usage");
    leaf.is_ca = IsCa::ExplicitNoCa;
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    leaf.not_before = rcgen::date_time_ymd(before.0, before.1, before.2);
    leaf.not_after = rcgen::date_time_ymd(after.0, after.1, after.2);
    let leaf_key = KeyPair::generate()?;
    let leaf_cert = leaf.signed_by(&leaf_key, &issuer)?;
    drop(issuer);

    crate::atomic::write_secret(&dir.join("server.key"), leaf_key.serialize_pem().as_bytes())
        .context("write server key")?;
    crate::atomic::write_secret(&dir.join("server.pem"), leaf_cert.pem().as_bytes())
        .context("write server certificate")?;
    crate::atomic::write_secret(&dir.join("ca.pem"), ca_cert.pem().as_bytes())
        .context("write CA certificate")?;
    crate::atomic::write_secret(&dir.join("issued"), now.to_string().as_bytes())
        .context("write issue time")?;
    Ok(())
}

fn load(dir: &Path) -> Result<Arc<tokio_rustls::rustls::ServerConfig>> {
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let chain = CertificateDer::pem_file_iter(dir.join("server.pem"))
        .context("read server certificate")?
        .collect::<Result<Vec<_>, _>>()
        .context("parse server certificate")?;
    let key = PrivateKeyDer::from_pem_file(dir.join("server.key")).context("read server key")?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("TLS server certificate")?;
    Ok(Arc::new(config))
}

/// (year, month, day) in UTC for a unix time, without a date crate.
fn civil(secs: i64) -> (i32, u8, u8) {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = era * 400 + yoe + i64::from(m <= 2);
    (y as i32, m as u8, d as u8)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issued(root: &Path) -> PathBuf {
        let dir = root.join("codex-tls");
        issue(&dir, now_secs()).unwrap();
        dir
    }

    /// Codex will trust this CA for every HTTPS call it makes, so nothing that
    /// could sign with it may be left behind. The only private key on disk is the
    /// server's own, and every file is readable by the owner alone.
    #[test]
    fn no_key_that_can_mint_a_certificate_is_written() {
        let root = tempfile::tempdir().unwrap();
        let dir = issued(root.path());
        let mut keys = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            if text.contains("PRIVATE KEY") {
                keys.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} is {mode:o}", path.display());
        }
        assert_eq!(keys, ["server.key"], "a signing key was written: {keys:?}");
    }

    /// The CA is name-constrained, so even a stolen copy of the server key's
    /// issuer could not vouch for anything but this machine's loopback. The
    /// nameConstraints extension (OID 2.5.29.30) must be present and critical.
    #[test]
    fn the_ca_is_constrained_to_loopback() {
        use tokio_rustls::rustls::pki_types::pem::PemObject;
        use tokio_rustls::rustls::pki_types::CertificateDer;
        let root = tempfile::tempdir().unwrap();
        let dir = issued(root.path());
        let ca = CertificateDer::from_pem_file(dir.join("ca.pem")).unwrap();
        let der = ca.as_ref();
        let oid = [0x06, 0x03, 0x55, 0x1d, 0x1e];
        let at = der
            .windows(oid.len())
            .position(|w| w == oid)
            .expect("the CA carries no name constraints");
        // OID, then BOOLEAN TRUE for "critical".
        assert_eq!(&der[at + 5..at + 8], [0x01, 0x01, 0xff], "not critical");
        for name in [&b"localhost"[..], &[127, 0, 0, 1, 255, 255, 255, 255][..]] {
            assert!(
                der.windows(name.len()).any(|w| w == name),
                "permitted subtree missing: {name:?}"
            );
        }
    }

    /// Reuse while fresh, reissue when missing or old. Reissuing on every start
    /// would break every Codex window already trusting the previous CA.
    #[test]
    fn the_certificate_is_reused_until_it_is_due() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("codex-tls");
        let now = now_secs();
        assert!(needs_issue(&dir, now), "nothing issued yet");
        issue(&dir, now).unwrap();
        assert!(!needs_issue(&dir, now), "a fresh certificate was due again");
        assert!(!needs_issue(&dir, now + (RENEW_AFTER_DAYS - 1) * 86_400));
        assert!(
            needs_issue(&dir, now + RENEW_AFTER_DAYS * 86_400),
            "never renewed"
        );
        std::fs::remove_file(dir.join("server.key")).unwrap();
        assert!(needs_issue(&dir, now), "a missing key was not reissued");
    }

    #[test]
    fn civil_dates_match_known_days() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(951_782_400), (2000, 2, 29));
        assert_eq!(civil(1_790_380_800), (2026, 9, 26));
    }
}
