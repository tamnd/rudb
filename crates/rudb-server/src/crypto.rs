//! The hash functions of the passwords, from the library of the TLS provider, so that a build has
//! one SHA-256. Neither library offers MD5, which comes from `rudb-pgwire`.

use std::num::NonZeroU32;

#[cfg(feature = "tls-aws-lc")]
use aws_lc_rs as lib;
#[cfg(all(feature = "tls-ring", not(feature = "tls-aws-lc")))]
use ring as lib;

use rudb_pgwire::Crypto;

/// A hash for the end point of a TLS channel binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Digest {
    Sha256,
    Sha384,
    Sha512,
}

/// The hash `digest` of `bytes`.
pub(crate) fn digest(digest: Digest, bytes: &[u8]) -> Vec<u8> {
    let algorithm = match digest {
        Digest::Sha256 => &lib::digest::SHA256,
        Digest::Sha384 => &lib::digest::SHA384,
        Digest::Sha512 => &lib::digest::SHA512,
    };
    lib::digest::digest(algorithm, bytes).as_ref().to_vec()
}

/// The [`Crypto`] of the server.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Provider;

impl Crypto for Provider {
    fn sha256(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut context = lib::digest::Context::new(&lib::digest::SHA256);
        for part in parts {
            context.update(part);
        }
        let mut out = [0; 32];
        out.copy_from_slice(context.finish().as_ref());
        out
    }

    fn hmac_sha256(&self, key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
        let key = lib::hmac::Key::new(lib::hmac::HMAC_SHA256, key);
        let mut context = lib::hmac::Context::with_key(&key);
        for part in parts {
            context.update(part);
        }
        let mut out = [0; 32];
        out.copy_from_slice(context.sign().as_ref());
        out
    }

    fn pbkdf2_sha256(&self, password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
        let mut out = [0; 32];
        let iterations = NonZeroU32::new(iterations).unwrap_or(NonZeroU32::MIN);
        lib::pbkdf2::derive(lib::pbkdf2::PBKDF2_HMAC_SHA256, iterations, salt, password, &mut out);
        out
    }

    fn md5(&self, parts: &[&[u8]]) -> [u8; 16] {
        rudb_pgwire::md5(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::Provider;
    use rudb_pgwire::{Crypto, verify_password};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn vectors() {
        assert_eq!(
            hex(&Provider.sha256(&[b"a", b"bc"])),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // RFC 4231, test case 2.
        assert_eq!(
            hex(&Provider.hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"])),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 7914, section 11, the first 32 bytes.
        assert_eq!(
            hex(&Provider.pbkdf2_sha256(b"passwd", b"salt", 1)),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
        assert_eq!(hex(&Provider.md5(&[b"pw", b"rpgx10"])), "116346ced10d82e112c8fb432bac3346");
    }

    #[test]
    fn the_secrets_of_the_oracle() {
        // The roles `rpg` and `rpg_md5` of `oracle/setup.sql` on PostgreSQL 19, password `rpg`.
        let scram =
            b"SCRAM-SHA-256$4096:ApVV/SMHQZ8qBo7/RKar3A==$Odczh9SIgXwcbcV5V3Tj7dX9y9qE9rndINjO\
                      Zu5OlhU=:jdNRKaZ8FGwDLCKHOrxWVGaNB1jb06y55d3JXJd2DAE=";
        assert!(verify_password(&Provider, b"rpg", scram, b"rpg"));
        assert!(!verify_password(&Provider, b"rpg", scram, b"rpx"));
        let md5 = b"md5d70e5c01fc7c169cbc3242449255c58d";
        assert!(verify_password(&Provider, b"rpg_md5", md5, b"rpg"));
    }
}
