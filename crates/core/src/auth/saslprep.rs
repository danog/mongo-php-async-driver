//! SASLprep (RFC 4013), as libmongoc implements it (`_mongoc_sasl_prep`, mongoc-scram.c):
//! map (non-ASCII spaces to U+0020, "commonly mapped to nothing" removed), normalize (NFKC),
//! then the prohibited-output, unassigned and bidirectional checks on the mapped code points
//! (libmongoc checks before normalization: NFKC never creates a character that fails them).

use super::tables::{LCAT, MAPPED_TO_NOTHING, NON_ASCII_SPACE, PROHIBITED_OUTPUT, RANDALCAT, UNASSIGNED};
use crate::error::{code, domain, Error};
use unicode_normalization::UnicodeNormalization;

fn in_table(c: u32, t: &[(u32, u32)]) -> bool {
    t.iter().any(|&(lo, hi)| c >= lo && c <= hi)
}

/// `_mongoc_sasl_prep_required`: only strings with control characters, DEL or non-ASCII
/// characters go through SASLprep.
pub fn required(s: &str) -> bool {
    s.bytes().any(|c| !(32..127).contains(&c))
}

/// `_mongoc_sasl_prep_impl(name, s)`. `name` names the input in error messages ("password").
pub fn prep_impl(name: &str, s: &str) -> Result<String, Error> {
    let err = |m: String| Error::new(domain::SCRAM, code::SCRAM_PROTOCOL_ERROR, m);
    // a. map
    let mapped: Vec<char> = s
        .chars()
        .filter_map(|c| {
            if in_table(c as u32, NON_ASCII_SPACE) {
                Some(' ')
            } else if in_table(c as u32, MAPPED_TO_NOTHING) {
                None
            } else {
                Some(c)
            }
        })
        .collect();
    // b. normalize
    let out: String = mapped.iter().copied().nfkc().collect();
    // c. prohibit
    if mapped.iter().any(|&c| in_table(c as u32, PROHIBITED_OUTPUT) || in_table(c as u32, UNASSIGNED)) {
        return Err(err(format!("prohibited character included in {name}")));
    }
    // d. bidi: a string with a RandALCat character has no LCat character, and starts and ends
    // with a RandALCat character
    let rand_al = |c: &char| in_table(*c as u32, RANDALCAT);
    if mapped.iter().any(rand_al)
        && (mapped.iter().any(|&c| in_table(c as u32, LCAT)) || !mapped.first().is_some_and(rand_al) || !mapped.last().is_some_and(rand_al))
    {
        return Err(err(format!("{name} does not meet bidirectional requirements")));
    }
    Ok(out)
}

/// `_mongoc_sasl_prep`: the password as SCRAM-SHA-256 uses it.
pub fn prep(password: &str) -> Result<String, Error> {
    if required(password) {
        prep_impl("password", password)
    } else {
        Ok(password.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // libmongoc's test-mongoc-scram.c (test_mongoc_scram_sasl_prep: RFC 4013 section 3 examples).
    // Its "\xFB\x1D apple \x09\xA8" case is not valid UTF-8, which a Rust string cannot hold
    // (URI passwords are validated as UTF-8 when they are decoded).
    #[test]
    fn sasl_prep_cases() {
        let cases: &[(&str, Option<&str>, bool)] = &[
            ("e\u{0301}", Some("\u{00E9}"), true),
            ("\u{00AA}", Some("a"), true),
            ("Henry \u{2163}", Some("Henry IV"), true),
            ("A\u{FB03}n", Some("Affin"), true),
            ("I\u{00AD}X", Some("IX"), true),
            ("I\u{2000}\u{00A0}X", Some("I  X"), true),
            ("banana \u{0007} apple", None, true),
            ("banana \u{0ABA} apple", None, true),
            ("\u{0627}1", None, true),
            ("\u{0621} \u{0712}", Some("\u{0621} \u{0712}"), true),
            ("I\u{2000}\u{00A0}X \u{2163}", Some("I  X IV"), true),
            ("user", Some("user"), false),
            ("USER", Some("USER"), false),
        ];
        for &(i, o, req) in cases {
            assert_eq!(required(i), req, "{i:?}");
            match (prep(i), o) {
                (Ok(v), Some(o)) => assert_eq!(v, o, "{i:?}"),
                (Err(e), None) => assert_eq!((e.domain, e.code), (domain::SCRAM, code::SCRAM_PROTOCOL_ERROR), "{i:?}"),
                (r, _) => panic!("{i:?}: {r:?}"),
            }
        }
        assert_eq!(prep("\u{0627}1").unwrap_err().message, "password does not meet bidirectional requirements");
        assert_eq!(prep("a\u{0007}").unwrap_err().message, "prohibited character included in password");
        assert!(required("del\u{7f}"));
    }
}
