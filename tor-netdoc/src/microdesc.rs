//! Parsing implementation for Tor microdescriptors.
//!
//! A "microdescriptor" is an incomplete, infrequently-changing
//! summary of a relay's information that is generated by
//! the directory authorities.
//!
//! Microdescriptors are much smaller than router descriptors, and
//! change less frequently. For this reason, they're currently used
//! for building circuits by all relays and clients.

use crate::argtype::*;
use crate::family::RelayFamily;
use crate::keyword::Keyword;
use crate::parse::SectionRules;
use crate::policy::PortPolicy;
use crate::tokenize::NetDocReader;
use crate::util;
use crate::{Error, Result};
use tor_llcrypto::d;
use tor_llcrypto::pk::{curve25519, ed25519, rsa};

use digest::Digest;
use lazy_static::lazy_static;

use std::time;

/// Annotations prepended to a microdescriptor that has been stored to
/// disk.
#[allow(dead_code)]
pub struct MicrodescAnnotation {
    last_listed: Option<time::SystemTime>,
}

/// A single microdescriptor.
#[allow(dead_code)]
pub struct Microdesc {
    // TODO: maybe this belongs somewhere else. Once it's used to store
    // correlate the microdesc to a consensus, it's never used again.
    sha256: [u8; 32],
    tap_onion_key: rsa::PublicKey,
    ntor_onion_key: curve25519::PublicKey,
    family: RelayFamily,
    ipv4_policy: PortPolicy,
    ipv6_policy: PortPolicy,
    // TODO: this is redundant.
    ed25519_id: Option<ed25519::PublicKey>,
    // addr is obsolete and doesn't go here any more
    // pr is obsolete and doesn't go here any more.
}

decl_keyword! {
    /// Keyword type for recognized objects in microdescriptors.
    MicrodescKW {
        annotation "last-listed" => ANN_LAST_LISTED,
        "onion-key" => ONION_KEY,
        "ntor-onion-key" => NTOR_ONION_KEY,
        "family" => FAMILY,
        "p" => P,
        "p6" => P6,
        "id" => ID,
    }
}

lazy_static! {
    static ref MICRODESC_ANNOTATIONS: SectionRules<MicrodescKW> = {
        use MicrodescKW::*;
        let mut rules = SectionRules::new();
        rules.add(ANN_LAST_LISTED.rule().args(1..));
        rules.add(ANN_UNRECOGNIZED.rule().may_repeat().obj_optional());
        rules
    };
    static ref MICRODESC_RULES: SectionRules<MicrodescKW> = {
        use MicrodescKW::*;

        let mut rules = SectionRules::new();
        rules.add(ONION_KEY.rule().required().no_args().obj_required());
        rules.add(NTOR_ONION_KEY.rule().required().args(1..));
        rules.add(FAMILY.rule().args(1..));
        rules.add(P.rule().args(2..));
        rules.add(P6.rule().args(2..));
        rules.add(ID.rule().may_repeat().args(2..));
        rules.add(UNRECOGNIZED.rule().may_repeat().obj_optional());
        rules
    };
}

impl MicrodescAnnotation {
    #[allow(dead_code)]
    fn parse_from_reader(
        reader: &mut NetDocReader<'_, MicrodescKW>,
    ) -> Result<MicrodescAnnotation> {
        use MicrodescKW::*;

        let mut items = reader.pause_at(|item| match item {
            Err(_) => false,
            Ok(item) => !item.get_kwd().is_annotation(),
        });

        let body = MICRODESC_ANNOTATIONS.parse(&mut items)?;

        let last_listed = match body.get(ANN_LAST_LISTED) {
            None => None,
            Some(item) => Some(item.args_as_str().parse::<ISO8601TimeSp>()?.into()),
        };

        Ok(MicrodescAnnotation { last_listed })
    }
}

impl Microdesc {
    /// Parse a string into a new microdescriptor.
    pub fn parse(s: &str) -> Result<Microdesc> {
        let mut items = crate::tokenize::NetDocReader::new(s);
        Self::parse_from_reader(&mut items)
    }

    /// Extract a single microdescriptor from a NetDocReader.
    fn parse_from_reader(reader: &mut NetDocReader<'_, MicrodescKW>) -> Result<Microdesc> {
        use MicrodescKW::*;
        let s = reader.str();

        let mut first_onion_key = true;
        // We'll pause at the next annotation, or at the _second_ onion key.
        let mut items = reader.pause_at(|item| match item {
            Err(_) => false,
            Ok(item) => {
                item.get_kwd().is_annotation()
                    || if item.get_kwd() == ONION_KEY {
                        let was_first = first_onion_key;
                        first_onion_key = false;
                        !was_first
                    } else {
                        false
                    }
            }
        });

        // We have to start with onion-key
        let start_pos = {
            let first = items.peek();
            let kwd = match first {
                Some(Ok(tok)) => tok.get_kwd_str(),
                _ => return Err(Error::MissingToken("onion-key")),
            };
            if kwd != "onion-key" {
                return Err(Error::MissingToken("onion-key"));
            }
            util::str_offset(s, kwd).unwrap()
        };

        let body = MICRODESC_RULES.parse(&mut items)?;
        // Legacy (tap) onion key
        let tap_onion_key: rsa::PublicKey = body
            .get_required(ONION_KEY)?
            .parse_obj::<RSAPublic>("RSA PUBLIC KEY")?
            .check_len_eq(1024)?
            .check_exponent(65537)?
            .into();

        // Ntor onion key
        let ntor_onion_key = body
            .get_required(NTOR_ONION_KEY)?
            .parse_arg::<Curve25519Public>(0)?
            .into();

        // family
        let family = body
            .maybe(FAMILY)
            .parse_args_as_str::<RelayFamily>()?
            .unwrap_or_else(RelayFamily::new);

        // exit policies.
        let ipv4_policy = body
            .maybe(P)
            .parse_args_as_str::<PortPolicy>()?
            .unwrap_or_else(PortPolicy::new_reject_all);
        let ipv6_policy = body
            .maybe(P6)
            .parse_args_as_str::<PortPolicy>()?
            .unwrap_or_else(PortPolicy::new_reject_all);

        // ed25519 identity
        let ed25519_id = {
            let id_tok = body
                .get_slice(ID)
                .iter()
                .find(|item| item.get_arg(1) == Some("ed25519"));
            match id_tok {
                None => None,
                Some(tok) => Some(tok.parse_arg::<Ed25519Public>(0)?.into()),
            }
        };

        // TODO: This is the whole string! It just isn't the
        // microdescriptor.
        let sha256 = d::Sha256::digest(&s[start_pos..].as_bytes()).into();

        Ok(Microdesc {
            sha256,
            tap_onion_key,
            ntor_onion_key,
            family,
            ipv4_policy,
            ipv6_policy,
            ed25519_id,
        })
    }
}
#[cfg(test)]
mod test {
    use super::*;
    const TESTDATA: &str = include_str!("../testdata/microdesc1.txt");

    #[test]
    fn parse_arbitrary() -> Result<()> {
        let _md = Microdesc::parse(TESTDATA)?;
        Ok(())
    }
}
