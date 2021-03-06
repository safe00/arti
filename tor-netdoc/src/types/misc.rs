//! Types used to parse arguments of entries in a directory document.
//!
//! There are some types that are pretty common, like "ISOTime",
//! "base64-encoded data", and so on.
//!
//! These types shouldn't be exposed outside of the netdoc crate.

pub(crate) use b16impl::*;
pub(crate) use b64impl::*;
pub(crate) use curve25519impl::*;
pub(crate) use ed25519impl::*;
pub(crate) use edcert::*;
pub(crate) use fingerprint::*;
pub(crate) use rsa::*;
pub(crate) use timeimpl::*;

/// Describes a value that van be decoded from a bunch of bytes.
///
/// Used for decoding the objects between BEGIN and END tags.
pub(crate) trait FromBytes: Sized {
    /// Try to parse a value of this type from a byte slice
    fn from_bytes(b: &[u8], p: crate::Pos) -> crate::Result<Self>;
    /// Try to parse a value of this type from a vector of bytes,
    /// and consume that value
    fn from_vec(v: Vec<u8>, p: crate::Pos) -> crate::Result<Self> {
        Self::from_bytes(&v[..], p)
    }
}

/// Types for decoding base64-encoded values.
mod b64impl {
    use crate::{Error, Pos, Result};
    use std::ops::RangeBounds;

    /// A byte array, encoded in base64 with optional padding.
    pub(crate) struct B64(Vec<u8>);

    impl std::str::FromStr for B64 {
        type Err = Error;
        fn from_str(s: &str) -> Result<Self> {
            let bytes = base64::decode_config(s, base64::STANDARD_NO_PAD)
                .map_err(|_| Error::BadArgument(Pos::at(s), "Invalid base64".into()))?;
            Ok(B64(bytes))
        }
    }

    impl B64 {
        /// Return the byte array from this object.
        pub(crate) fn as_bytes(&self) -> &[u8] {
            &self.0[..]
        }
        /// Return this object if its length is within the provided bounds
        /// object, or an error otherwise.
        pub(crate) fn check_len<B: RangeBounds<usize>>(self, bounds: B) -> Result<Self> {
            if bounds.contains(&self.0.len()) {
                Ok(self)
            } else {
                Err(Error::BadObjectVal(
                    Pos::Unknown,
                    "Invalid length on base64 data".to_string(),
                ))
            }
        }
    }

    impl From<B64> for Vec<u8> {
        fn from(w: B64) -> Vec<u8> {
            w.0
        }
    }
}

// ============================================================

/// Types for decoding hex-encoded values.
mod b16impl {
    use crate::{Error, Pos, Result};

    /// A byte array encoded in hexadecimal.
    pub(crate) struct B16(Vec<u8>);

    impl std::str::FromStr for B16 {
        type Err = Error;
        fn from_str(s: &str) -> Result<Self> {
            let bytes = hex::decode(s)
                .map_err(|_| Error::BadArgument(Pos::at(s), "invalid hexadecimal".to_string()))?;
            Ok(B16(bytes))
        }
    }

    impl B16 {
        /// Return the underlying byte array.
        #[allow(unused)]
        pub(crate) fn as_bytes(&self) -> &[u8] {
            &self.0[..]
        }
    }

    impl From<B16> for Vec<u8> {
        fn from(w: B16) -> Vec<u8> {
            w.0
        }
    }
}

// ============================================================

/// Types for decoding curve25519 keys
mod curve25519impl {
    use super::B64;
    use crate::{Error, Pos, Result};
    use std::convert::TryInto;
    use tor_llcrypto::pk::curve25519::PublicKey;

    /// A Curve25519 public key, encoded in base64 with optional padding
    pub(crate) struct Curve25519Public(PublicKey);

    impl std::str::FromStr for Curve25519Public {
        type Err = Error;
        fn from_str(s: &str) -> Result<Self> {
            let b64: B64 = s.parse()?;
            let arry: [u8; 32] = b64.as_bytes().try_into().map_err(|_| {
                Error::BadArgument(Pos::at(s), "bad length for curve25519 key.".into())
            })?;
            Ok(Curve25519Public(arry.into()))
        }
    }

    impl From<Curve25519Public> for PublicKey {
        fn from(w: Curve25519Public) -> PublicKey {
            w.0
        }
    }
}

// ============================================================

/// Types for decoding ed25519 keys
mod ed25519impl {
    use super::B64;
    use crate::{Error, Pos, Result};
    use tor_llcrypto::pk::ed25519::Ed25519Identity;

    /// An alleged ed25519 public key, encoded in base64 with optional
    /// padding.
    pub(crate) struct Ed25519Public(Ed25519Identity);

    impl std::str::FromStr for Ed25519Public {
        type Err = Error;
        fn from_str(s: &str) -> Result<Self> {
            let b64: B64 = s.parse()?;
            if b64.as_bytes().len() != 32 {
                return Err(Error::BadArgument(
                    Pos::at(s),
                    "bad length for ed25519 key.".into(),
                ));
            }
            let key = Ed25519Identity::from_bytes(b64.as_bytes()).ok_or_else(|| {
                Error::BadArgument(Pos::at(s), "bad value for ed25519 key.".into())
            })?;
            Ok(Ed25519Public(key))
        }
    }

    impl From<Ed25519Public> for Ed25519Identity {
        fn from(pk: Ed25519Public) -> Ed25519Identity {
            pk.0
        }
    }
}

// ============================================================

/// Types for decoding times and dates
mod timeimpl {
    use crate::{Error, Pos, Result};
    use std::time::SystemTime;

    /// A wall-clock time, encoded in Iso8601 format with an intervening
    /// space between the date and time.
    ///
    /// (Example: "2020-10-09 17:38:12")
    pub(crate) struct Iso8601TimeSp(SystemTime);

    impl std::str::FromStr for Iso8601TimeSp {
        type Err = Error;
        fn from_str(s: &str) -> Result<Iso8601TimeSp> {
            use chrono::{DateTime, NaiveDateTime, Utc};
            let d = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .map_err(|e| Error::BadArgument(Pos::at(s), format!("invalid time: {}", e)))?;
            let dt = DateTime::<Utc>::from_utc(d, Utc);
            Ok(Iso8601TimeSp(dt.into()))
        }
    }

    impl From<Iso8601TimeSp> for SystemTime {
        fn from(t: Iso8601TimeSp) -> SystemTime {
            t.0
        }
    }
}

/// Types for decoding RSA keys
mod rsa {
    use crate::{Error, Pos, Result};
    use std::ops::RangeBounds;
    use tor_llcrypto::pk::rsa::PublicKey;

    /// An RSA public key, as parsed from a base64-encoded object.
    #[allow(non_camel_case_types)]
    pub(crate) struct RsaPublic(PublicKey, Pos);

    impl From<RsaPublic> for PublicKey {
        fn from(k: RsaPublic) -> PublicKey {
            k.0
        }
    }
    impl super::FromBytes for RsaPublic {
        fn from_bytes(b: &[u8], pos: Pos) -> Result<Self> {
            let key = PublicKey::from_der(b).ok_or_else(|| {
                Error::BadObjectVal(Pos::None, "unable to decode RSA public key".into())
            })?;
            Ok(RsaPublic(key, pos))
        }
    }
    impl RsaPublic {
        /// Give an error if the exponent of this key is not 'e'
        pub(crate) fn check_exponent(self, e: u32) -> Result<Self> {
            if self.0.exponent_is(e) {
                Ok(self)
            } else {
                Err(Error::BadObjectVal(self.1, "invalid RSA exponent".into()))
            }
        }
        /// Give an error if the length of of this key's modulus, in
        /// bits, is not contained in 'bounds'
        pub(crate) fn check_len<B: RangeBounds<usize>>(self, bounds: B) -> Result<Self> {
            if bounds.contains(&self.0.bits()) {
                Ok(self)
            } else {
                Err(Error::BadObjectVal(self.1, "invalid RSA length".into()))
            }
        }
        /// Give an error if the length of of this key's modulus, in
        /// bits, is not exactly `n`.
        pub(crate) fn check_len_eq(self, n: usize) -> Result<Self> {
            self.check_len(n..=n)
        }
    }
}

/// Types for decoding Ed25519 certificates
mod edcert {
    use crate::{Error, Pos, Result};
    use tor_cert::{CertType, Ed25519Cert, KeyUnknownCert};
    use tor_llcrypto::pk::ed25519;

    /// An ed25519 certificate as parsed from a directory object, with
    /// signature not validated.
    pub(crate) struct UnvalidatedEdCert(KeyUnknownCert, Pos);

    impl super::FromBytes for UnvalidatedEdCert {
        fn from_bytes(b: &[u8], p: Pos) -> Result<Self> {
            let cert = Ed25519Cert::decode(b).map_err(|e| {
                Error::BadObjectVal(p, format!("Bad certificate: {}", e.to_string()))
            })?;
            Ok(Self(cert, p))
        }
        fn from_vec(v: Vec<u8>, p: Pos) -> Result<Self> {
            Self::from_bytes(&v[..], p)
        }
    }
    impl UnvalidatedEdCert {
        /// Give an error if this certificate's type is not `desired_type`.
        pub(crate) fn check_cert_type(self, desired_type: CertType) -> Result<Self> {
            if self.0.peek_cert_type() != desired_type {
                return Err(Error::BadObjectVal(
                    self.1,
                    format!(
                        "bad certificate type {} (wanted {})",
                        self.0.peek_cert_type(),
                        desired_type
                    ),
                ));
            }
            Ok(self)
        }
        /// Give an error if this certificate's subject_key is not `pk`
        pub(crate) fn check_subject_key_is(self, pk: &ed25519::PublicKey) -> Result<Self> {
            if self.0.peek_subject_key().as_ed25519() != Some(pk) {
                return Err(Error::BadObjectVal(self.1, "incorrect subject key".into()));
            }
            Ok(self)
        }
        /// Consume this object and return the inner Ed25519 certificate.
        pub(crate) fn into_unchecked(self) -> KeyUnknownCert {
            self.0
        }
    }
}

/// Types for decoding RSA fingerprints
mod fingerprint {
    use crate::{Error, Pos, Result};
    use tor_llcrypto::pk::rsa::RsaIdentity;

    /// A hex-encoded fingerprint with spaces in it.
    pub(crate) struct SpFingerprint(RsaIdentity);

    /// A hex-encoded fingerprint with no spaces.
    pub(crate) struct Fingerprint(RsaIdentity);

    /// A "long identity" in the format used for Family members.
    pub(crate) struct LongIdent(RsaIdentity);

    impl From<SpFingerprint> for RsaIdentity {
        fn from(f: SpFingerprint) -> RsaIdentity {
            f.0
        }
    }

    impl From<LongIdent> for RsaIdentity {
        fn from(f: LongIdent) -> RsaIdentity {
            f.0
        }
    }

    impl From<Fingerprint> for RsaIdentity {
        fn from(f: Fingerprint) -> RsaIdentity {
            f.0
        }
    }

    /// Helper: parse an identity from a hexadecimal string
    fn parse_hex_ident(s: &str) -> Result<RsaIdentity> {
        let bytes = hex::decode(s).map_err(|_| {
            Error::BadArgument(Pos::at(s), "invalid hexadecimal in fingerprint".into())
        })?;
        RsaIdentity::from_bytes(&bytes)
            .ok_or_else(|| Error::BadArgument(Pos::at(s), "wrong length on fingerprint".into()))
    }

    impl std::str::FromStr for SpFingerprint {
        type Err = Error;
        fn from_str(s: &str) -> Result<SpFingerprint> {
            let ident = parse_hex_ident(&s.replace(' ', "")).map_err(|e| e.at_pos(Pos::at(s)))?;
            Ok(SpFingerprint(ident))
        }
    }

    impl std::str::FromStr for Fingerprint {
        type Err = Error;
        fn from_str(s: &str) -> Result<Fingerprint> {
            let ident = parse_hex_ident(s).map_err(|e| e.at_pos(Pos::at(s)))?;
            Ok(Fingerprint(ident))
        }
    }

    impl std::str::FromStr for LongIdent {
        type Err = Error;
        fn from_str(mut s: &str) -> Result<LongIdent> {
            if s.starts_with('$') {
                s = &s[1..];
            }
            if let Some(idx) = s.find(|ch| ch == '=' || ch == '~') {
                s = &s[..idx];
            }
            let ident = parse_hex_ident(s)?;
            Ok(LongIdent(ident))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Result;

    #[test]
    fn base64() -> Result<()> {
        assert_eq!("Mi43MTgyOA".parse::<B64>()?.as_bytes(), &b"2.71828"[..]);
        assert_eq!("Mi43MTgyOA==".parse::<B64>()?.as_bytes(), &b"2.71828"[..]);
        assert!("Mi43!!!!!!".parse::<B64>().is_err());
        assert!("Mi".parse::<B64>().is_err());
        assert!("Mi43MTgyOA".parse::<B64>()?.check_len(7..=8).is_ok());
        assert!("Mi43MTgyOA".parse::<B64>()?.check_len(8..).is_err());
        Ok(())
    }

    #[test]
    fn base16() -> Result<()> {
        assert_eq!("332e313432".parse::<B16>()?.as_bytes(), &b"3.142"[..]);
        assert_eq!("332E313432".parse::<B16>()?.as_bytes(), &b"3.142"[..]);
        assert_eq!("332E3134".parse::<B16>()?.as_bytes(), &b"3.14"[..]);
        assert!("332E313".parse::<B16>().is_err());
        assert!("332G3134".parse::<B16>().is_err());
        Ok(())
    }

    #[test]
    fn curve25519() -> Result<()> {
        use std::convert::TryInto;
        use tor_llcrypto::pk::curve25519::PublicKey;
        let k1 = "ppwthHXW8kXD0f9fE7UPYsOAAu4uj5ORwSomCMxKkz8=";
        let k2 = hex::decode("a69c2d8475d6f245c3d1ff5f13b50f62c38002ee2e8f9391c12a2608cc4a933f")
            .unwrap();
        let k2: &[u8; 32] = &k2[..].try_into().unwrap();

        let k1: PublicKey = k1.parse::<Curve25519Public>()?.into();
        assert_eq!(k1, (*k2).into());

        assert!("ppwthHXW8kXD0f9fE7UPYsOAAu4uj5ORwSomCMxKkz"
            .parse::<Curve25519Public>()
            .is_err());
        assert!("ppwthHXW8kXD0f9fE7UPYsOAAu4uj5ORSomCMxKkz"
            .parse::<Curve25519Public>()
            .is_err());
        assert!("ppwthHXW8kXD0f9fE7UPYsOAAu4uj5wSomCMxKkz"
            .parse::<Curve25519Public>()
            .is_err());
        assert!("ppwthHXW8kXD0f9fE7UPYsOAAu4ORwSomCMxKkz"
            .parse::<Curve25519Public>()
            .is_err());

        Ok(())
    }

    #[test]
    fn ed25519() -> Result<()> {
        use tor_llcrypto::pk::ed25519::Ed25519Identity;
        let k1 = "WVIPQ8oArAqLY4XzkcpIOI6U8KsUJHBQhG8SC57qru0";
        let k2 = hex::decode("59520f43ca00ac0a8b6385f391ca48388e94f0ab14247050846f120b9eeaaeed")
            .unwrap();

        let k1: Ed25519Identity = k1.parse::<Ed25519Public>()?.into();
        assert_eq!(k1, Ed25519Identity::from_bytes(&k2).unwrap());

        assert!("WVIPQ8oArAqLY4Xzk0!!!!8KsUJHBQhG8SC57qru"
            .parse::<Curve25519Public>()
            .is_err());
        assert!("WVIPQ8oArAqLY4XzkcpIU8KsUJHBQhG8SC57qru"
            .parse::<Curve25519Public>()
            .is_err());
        assert!("WVIPQ8oArAqLY4XzkcpIU8KsUJHBQhG8SC57qr"
            .parse::<Curve25519Public>()
            .is_err());
        // right length, bad key:
        assert!("ppwthHXW8kXD0f9fE7UPYsOAAu4uj5ORwSomCMxaaaa"
            .parse::<Curve25519Public>()
            .is_err());
        Ok(())
    }

    #[test]
    fn time() -> Result<()> {
        use std::time::{Duration, SystemTime};

        let t = "2020-09-29 13:36:33".parse::<Iso8601TimeSp>()?;
        let t: SystemTime = t.into();
        assert_eq!(t, SystemTime::UNIX_EPOCH + Duration::new(1601386593, 0));

        assert!("2020-FF-29 13:36:33".parse::<Iso8601TimeSp>().is_err());
        assert!("2020-09-29Q13:99:33".parse::<Iso8601TimeSp>().is_err());
        assert!("2020-09-29".parse::<Iso8601TimeSp>().is_err());
        assert!("too bad, waluigi time".parse::<Iso8601TimeSp>().is_err());

        Ok(())
    }

    // TODO: tests for RSA public key parsing.
    // TODO: tests for edcert parsing.

    #[test]
    fn fingerprint() -> Result<()> {
        use tor_llcrypto::pk::rsa::RsaIdentity;
        let fp1 = "7467 A97D 19CD 2B4F 2BC0 388A A99C 5E67 710F 847E";
        let fp2 = "7467A97D19CD2B4F2BC0388AA99C5E67710F847E";
        let fp3 = "$7467A97D19CD2B4F2BC0388AA99C5E67710F847E";
        let fp4 = "$7467A97D19CD2B4F2BC0388AA99C5E67710F847E=fred";

        let k = hex::decode(fp2).unwrap();
        let k = RsaIdentity::from_bytes(&k[..]).unwrap();

        assert_eq!(RsaIdentity::from(fp1.parse::<SpFingerprint>()?), k);
        assert_eq!(RsaIdentity::from(fp2.parse::<SpFingerprint>()?), k);
        assert!(fp3.parse::<SpFingerprint>().is_err());
        assert!(fp4.parse::<SpFingerprint>().is_err());

        assert!(fp1.parse::<Fingerprint>().is_err());
        assert_eq!(RsaIdentity::from(fp2.parse::<Fingerprint>()?), k);
        assert!(fp3.parse::<Fingerprint>().is_err());
        assert!(fp4.parse::<Fingerprint>().is_err());

        assert!(fp1.parse::<LongIdent>().is_err());
        assert_eq!(RsaIdentity::from(fp2.parse::<LongIdent>()?), k);
        assert_eq!(RsaIdentity::from(fp3.parse::<LongIdent>()?), k);
        assert_eq!(RsaIdentity::from(fp4.parse::<LongIdent>()?), k);

        assert!("xxxx".parse::<Fingerprint>().is_err());
        assert!("ffffffffff".parse::<Fingerprint>().is_err());
        Ok(())
    }
}
