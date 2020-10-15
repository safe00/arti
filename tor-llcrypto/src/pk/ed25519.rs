//! Re-exporting Ed25519 implementations.
//!
//! Eventually this should probably be replaced with a wrapper that
//! uses the ed25519 trait and the Signature trait.

use arrayref::array_ref;
use std::convert::TryInto;
use std::fmt::{self, Debug, Display, Formatter};
use subtle::*;

pub use ed25519_dalek::{ExpandedSecretKey, Keypair, PublicKey, SecretKey, Signature};

/// A relay's identity, as an unchecked, unvalidated Ed25519 key.
#[derive(Clone)]
pub struct Ed25519Identity {
    /// A raw unchecked Ed25519 public key.
    id: [u8; 32],
}

impl Ed25519Identity {
    /// Construct a new Ed25519 identity from a 32-byte sequence.
    ///
    /// This might or might not actually be a valid Ed25519 public key.
    ///
    /// ```
    /// use tor_llcrypto::pk::ed25519::{Ed25519Identity, PublicKey};
    /// use std::convert::TryInto;
    ///
    /// let bytes = b"klsadjfkladsfjklsdafkljasdfsdsd!";
    /// let id = Ed25519Identity::new(*bytes);
    /// let pk: Result<PublicKey,_> = (&id).try_into();
    /// assert!(pk.is_ok());
    ///
    /// let bytes = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    /// let id = Ed25519Identity::new(*bytes);
    /// let pk: Result<PublicKey,_> = (&id).try_into();
    /// assert!(pk.is_err());
    /// ```
    pub fn new(id: [u8; 32]) -> Self {
        Ed25519Identity { id }
    }
    /// If `id` is of the correct length, wrap it in an Ed25519Identity.
    pub fn from_slice(id: &[u8]) -> Option<Self> {
        if id.len() == 32 {
            Some(Ed25519Identity::new(*array_ref!(id, 0, 32)))
        } else {
            None
        }
    }
    /// Return a reference to the bytes in this key.
    pub fn as_bytes(&self) -> &[u8] {
        &self.id[..]
    }
}

impl From<[u8; 32]> for Ed25519Identity {
    fn from(id: [u8; 32]) -> Self {
        Ed25519Identity::new(id)
    }
}

impl TryInto<PublicKey> for &Ed25519Identity {
    type Error = ed25519_dalek::SignatureError;
    fn try_into(self) -> Result<PublicKey, Self::Error> {
        PublicKey::from_bytes(&self.id[..])
    }
}

impl PartialEq<Ed25519Identity> for Ed25519Identity {
    fn eq(&self, rhs: &Ed25519Identity) -> bool {
        self.id.ct_eq(&rhs.id).unwrap_u8() == 1
    }
}

impl Eq for Ed25519Identity {}

impl Display for Ed25519Identity {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            base64::encode_config(self.id, base64::STANDARD_NO_PAD)
        )
    }
}

impl Debug for Ed25519Identity {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "Ed25519Identity {{ {} }}", self)
    }
}

/// An ed25519 signature, plus the document that it signs and its
/// public key.
pub struct ValidatableEd25519Signature {
    /// The key that allegedly produced the signature
    key: PublicKey,
    /// The alleged signature
    sig: Signature,
    /// The entire body of text that is allegedly signed here.
    ///
    /// TODO: It's not so good to have this included here; it
    /// would be better to have a patch to ed25519_dalek to allow
    /// us to pre-hash the signed thing, and just store a digest.
    /// We can't use that with the 'prehash' variant of ed25519,
    /// since that has different constants.
    entire_text_of_signed_thing: Vec<u8>,
}

impl ValidatableEd25519Signature {
    /// Create a new ValidatableEd25519Signature
    pub fn new(key: PublicKey, sig: Signature, text: &[u8]) -> Self {
        ValidatableEd25519Signature {
            key,
            sig,
            entire_text_of_signed_thing: text.into(),
        }
    }

    /// View the interior of this signature object.
    pub(crate) fn as_parts(&self) -> (&PublicKey, &Signature, &[u8]) {
        (&self.key, &self.sig, &self.entire_text_of_signed_thing[..])
    }
}

impl super::ValidatableSignature for ValidatableEd25519Signature {
    fn is_valid(&self) -> bool {
        use signature::Verifier;
        self.key
            .verify(&self.entire_text_of_signed_thing[..], &self.sig)
            .is_ok()
    }

    fn as_ed25519(&self) -> Option<&ValidatableEd25519Signature> {
        Some(self)
    }
}

/// Perform a batch verification operation on the provided signatures
pub fn validate_batch(sigs: &[&ValidatableEd25519Signature]) -> bool {
    use crate::pk::ValidatableSignature;
    if sigs.is_empty() {
        true
    } else if sigs.len() == 1 {
        sigs[0].is_valid()
    } else {
        let mut ed_msgs = Vec::new();
        let mut ed_sigs = Vec::new();
        let mut ed_pks = Vec::new();
        for ed_sig in sigs {
            let (pk, sig, msg) = ed_sig.as_parts();
            ed_sigs.push(*sig);
            ed_pks.push(*pk);
            ed_msgs.push(msg);
        }
        ed25519_dalek::verify_batch(&ed_msgs[..], &ed_sigs[..], &ed_pks[..]).is_ok()
    }
}