//! The vTPM's endorsement key certificate, issued by a CA of this limina install.
//!
//! A real TPM leaves its factory with a certificate for its endorsement key, and clients now
//! expect one: systemd-tpm2-setup (systemd 262 on) persists an EK only when a certificate
//! certifies it. janus makes the key and holds the certificate; what the certificate says and
//! who signs it is limina's (docs/design/vtpm.md): one P-256 CA per install, made the first
//! time a TPM needs it and kept beside the VM library, so every VM on this Mac chains to one
//! root an attestation verifier could pin.
//!
//! The CA directory holds `ca.key` (the raw P-256 scalar, owner-only) and `ca.der` (its
//! self-signed certificate). It appears whole or not at all: it is assembled in a sibling and
//! renamed into place, and a worker that loses that race uses the winner's.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::time::SystemTime;

use p256::ecdsa::{DerSignature, SigningKey};
use x509_cert::Certificate;
use x509_cert::builder::profile::BuilderProfile;
use x509_cert::builder::{Builder, CertificateBuilder};
use x509_cert::der::{Decode as _, Encode as _};
use x509_cert::ext::Extension;
use x509_cert::ext::ToExtension as _;
use x509_cert::ext::pkix::{
    AuthorityKeyIdentifier, BasicConstraints, KeyUsage, KeyUsages, SubjectKeyIdentifier,
};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::{SubjectPublicKeyInfo, SubjectPublicKeyInfoRef};
use x509_cert::time::{Time, Validity};

const CA_SUBJECT: &str = "CN=limina vTPM EK CA";
const EK_SUBJECT: &str = "CN=limina vTPM endorsement key";

/// The CA, and where it lives: `$LIMINA_TPM_EK_CA_DIR`, or `tpm-ek-ca` in the application
/// support directory.
pub fn ca_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LIMINA_TPM_EK_CA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Application Support/Limina/tpm-ek-ca")
}

pub struct EkCa {
    key: SigningKey,
    cert: Certificate,
}

impl EkCa {
    /// The CA in `dir`, made there first when there is none.
    pub fn open_or_create(dir: &Path) -> io::Result<EkCa> {
        match EkCa::open(dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            other => return other,
        }
        let ca = EkCa::generate()?;
        let parent = dir.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        let staging = parent.join(format!(".{name}.{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir(&staging)?;
        write_private(&staging.join("ca.key"), &ca.key.to_bytes())?;
        write_private(&staging.join("ca.der"), &der(&ca.cert)?)?;
        match fs::rename(&staging, dir) {
            Ok(()) => Ok(ca),
            // Another worker made it first: its CA is the install's.
            Err(_) if dir.join("ca.der").exists() => {
                let _ = fs::remove_dir_all(&staging);
                EkCa::open(dir)
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                Err(e)
            }
        }
    }

    fn open(dir: &Path) -> io::Result<EkCa> {
        let key = fs::read(dir.join("ca.key"))?;
        let key = SigningKey::from_slice(&key).map_err(|_| invalid(dir, "ca.key is not a key"))?;
        let cert = fs::read(dir.join("ca.der"))?;
        let cert = Certificate::from_der(&cert).map_err(|e| invalid(dir, &e.to_string()))?;
        let mine = SubjectPublicKeyInfo::from_key(key.verifying_key()).map_err(other)?;
        if cert.tbs_certificate().subject_public_key_info() != &mine {
            return Err(invalid(dir, "ca.der does not certify ca.key"));
        }
        Ok(EkCa { key, cert })
    }

    fn generate() -> io::Result<EkCa> {
        let key = random_key();
        let profile = Authority {
            subject: Name::from_str(CA_SUBJECT).map_err(other)?,
        };
        let spki = SubjectPublicKeyInfo::from_key(key.verifying_key()).map_err(other)?;
        let validity = Validity::new(
            Time::try_from(SystemTime::now()).map_err(other)?,
            Time::INFINITY,
        );
        let cert = CertificateBuilder::new(profile, serial()?, validity, spki)
            .map_err(other)?
            .build::<_, DerSignature>(&key)
            .map_err(other)?;
        Ok(EkCa { key, cert })
    }

    /// A DER certificate for the EK whose public key is `point` (uncompressed SEC1, as
    /// `janus::Janus::endorsement_key` gives it), issued by this CA and valid from now on.
    pub fn certify(&self, point: &[u8]) -> io::Result<Vec<u8>> {
        let ek = p256::PublicKey::from_sec1_bytes(point)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not a P-256 point"))?;
        let spki = SubjectPublicKeyInfo::from_key(&ek).map_err(other)?;
        let profile = Endorsement {
            issuer: self.cert.tbs_certificate().subject().clone(),
            subject: Name::from_str(EK_SUBJECT).map_err(other)?,
        };
        let validity = Validity::new(
            Time::try_from(SystemTime::now()).map_err(other)?,
            Time::INFINITY,
        );
        let cert = CertificateBuilder::new(profile, serial()?, validity, spki)
            .map_err(other)?
            .build::<_, DerSignature>(&self.key)
            .map_err(other)?;
        der(&cert)
    }

    #[cfg(test)]
    pub fn certificate(&self) -> &Certificate {
        &self.cert
    }
}

/// The CA's own certificate: self-signed, for signing certificates only.
struct Authority {
    subject: Name,
}

impl BuilderProfile for Authority {
    fn get_issuer(&self, subject: &Name) -> Name {
        subject.clone()
    }

    fn get_subject(&self) -> Name {
        self.subject.clone()
    }

    fn build_extensions(
        &self,
        spk: SubjectPublicKeyInfoRef<'_>,
        _issuer_spk: SubjectPublicKeyInfoRef<'_>,
        tbs: &x509_cert::TbsCertificate,
    ) -> x509_cert::builder::Result<Vec<Extension>> {
        let mut extensions = Vec::new();
        let ski = SubjectKeyIdentifier::try_from(spk)?;
        let basic = BasicConstraints {
            ca: true,
            path_len_constraint: Some(0),
        };
        extensions.push(basic.to_extension(tbs.subject(), &extensions)?);
        let usage = KeyUsage(KeyUsages::KeyCertSign | KeyUsages::CRLSign);
        extensions.push(usage.to_extension(tbs.subject(), &extensions)?);
        extensions.push(ski.to_extension(tbs.subject(), &extensions)?);
        Ok(extensions)
    }
}

/// An EK certificate: an end entity whose key is for key agreement only (an ECC EK decrypts
/// by ECDH and never signs), naming the CA's key as its authority.
struct Endorsement {
    issuer: Name,
    subject: Name,
}

impl BuilderProfile for Endorsement {
    fn get_issuer(&self, _subject: &Name) -> Name {
        self.issuer.clone()
    }

    fn get_subject(&self) -> Name {
        self.subject.clone()
    }

    fn build_extensions(
        &self,
        _spk: SubjectPublicKeyInfoRef<'_>,
        issuer_spk: SubjectPublicKeyInfoRef<'_>,
        tbs: &x509_cert::TbsCertificate,
    ) -> x509_cert::builder::Result<Vec<Extension>> {
        let mut extensions = Vec::new();
        let aki = AuthorityKeyIdentifier {
            key_identifier: Some(SubjectKeyIdentifier::try_from(issuer_spk)?.0),
            ..Default::default()
        };
        extensions.push(aki.to_extension(tbs.subject(), &extensions)?);
        let basic = BasicConstraints {
            ca: false,
            path_len_constraint: None,
        };
        extensions.push(basic.to_extension(tbs.subject(), &extensions)?);
        let usage = KeyUsage(KeyUsages::KeyAgreement.into());
        extensions.push(usage.to_extension(tbs.subject(), &extensions)?);
        Ok(extensions)
    }
}

fn random_key() -> SigningKey {
    loop {
        let mut scalar = [0u8; 32];
        getrandom::fill(&mut scalar).expect("the host CSPRNG failed");
        // Zero or past the group order, a chance of about 2^-32: draw again.
        if let Ok(key) = SigningKey::from_slice(&scalar) {
            return key;
        }
    }
}

/// A random positive 127-bit serial: unique without a counter to keep.
fn serial() -> io::Result<SerialNumber> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the host CSPRNG failed");
    bytes[0] &= 0x7F;
    bytes[0] |= 0x40;
    SerialNumber::new(&bytes).map_err(other)
}

fn der(cert: &Certificate) -> io::Result<Vec<u8>> {
    cert.to_der().map_err(other)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn invalid(dir: &Path, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {what}", dir.display()),
    )
}

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier as _;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("limina-ek-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("tpm-ek-ca")
    }

    fn ek_point() -> Vec<u8> {
        random_key()
            .verifying_key()
            .to_sec1_point(false)
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn the_ca_is_made_once_and_reopened() {
        let dir = scratch("once");
        let first = EkCa::open_or_create(&dir).unwrap();
        let mode = fs::metadata(dir.join("ca.key")).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        let again = EkCa::open_or_create(&dir).unwrap();
        assert_eq!(
            der(first.certificate()).unwrap(),
            der(again.certificate()).unwrap()
        );
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_ca_whose_key_and_certificate_disagree_is_refused() {
        let dir = scratch("mismatch");
        EkCa::open_or_create(&dir).unwrap();
        fs::write(dir.join("ca.key"), random_key().to_bytes()).unwrap();
        let e = EkCa::open_or_create(&dir).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /// The certificate carries the EK's key and the CA's signature over it, which is all
    /// systemd compares and what a verifier checks.
    #[test]
    fn an_ek_certificate_certifies_the_ek_under_the_ca() {
        let dir = scratch("certify");
        let ca = EkCa::open_or_create(&dir).unwrap();
        let point = ek_point();
        let cert = Certificate::from_der(&ca.certify(&point).unwrap()).unwrap();
        let tbs = cert.tbs_certificate();
        let key = tbs.subject_public_key_info().subject_public_key.raw_bytes();
        assert_eq!(key, point.as_slice());
        assert_eq!(tbs.issuer(), ca.certificate().tbs_certificate().subject());

        let ca_spki = ca.certificate().tbs_certificate().subject_public_key_info();
        let ca_key = VerifyingKey::from_sec1_bytes(ca_spki.subject_public_key.raw_bytes()).unwrap();
        let signature = p256::ecdsa::DerSignature::try_from(cert.signature().raw_bytes()).unwrap();
        ca_key.verify(&tbs.to_der().unwrap(), &signature).unwrap();
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_point_that_is_not_on_p256_is_refused() {
        let dir = scratch("refuse");
        let ca = EkCa::open_or_create(&dir).unwrap();
        let e = ca.certify(&[0x04; 65]).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
