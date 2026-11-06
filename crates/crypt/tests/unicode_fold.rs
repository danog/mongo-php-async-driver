//! libmongocrypt's test/test-unicode-fold.c cases

use mongo_crypt::unicode::{fold, REMOVE_DIACRITICS, TO_LOWER};

fn all(input: &str, case: &str, dia: &str, both: &str) {
    assert_eq!(fold(input.as_bytes(), TO_LOWER).unwrap(), case.as_bytes(), "case {input}");
    assert_eq!(fold(input.as_bytes(), REMOVE_DIACRITICS).unwrap(), dia.as_bytes(), "dia {input}");
    assert_eq!(fold(input.as_bytes(), TO_LOWER | REMOVE_DIACRITICS).unwrap(), both.as_bytes(), "both {input}");
}

#[test]
fn unicode_fold() {
    for ch in 0u8..=0x7f {
        let s = (ch as char).to_string();
        if ch.is_ascii_uppercase() {
            let l = ((ch + 0x20) as char).to_string();
            all(&s, &l, &s, &l);
        } else if ch == b'^' || ch == b'`' {
            all(&s, &s, "", "");
        } else {
            all(&s, &s, &s, &s);
        }
    }
    all("abc", "abc", "abc", "abc");
    all("¿CUÁNTOS AÑOS tienes Tú?", "¿cuántos años tienes tú?", "¿CUANTOS ANOS tienes Tu?", "¿cuantos anos tienes tu?");
    all("СКОЛЬКО ТЕБЕ ЛЕТ?", "сколько тебе лет?", "СКОЛЬКО ТЕБЕ ЛЕТ?", "сколько тебе лет?");
    all("Πόσο χρονών είσαι?", "πόσο χρονών είσαι?", "Ποσο χρονων εισαι?", "ποσο χρονων εισαι?");
    all("Cafe\u{301}", "cafe\u{301}", "Cafe", "cafe");
    all("CafE\u{301}", "cafe\u{301}", "CafE", "cafe");
    assert_eq!(fold(b"fo\0bar", TO_LOWER | REMOVE_DIACRITICS).unwrap(), b"fo\0bar");
    assert_eq!(fold(b"\xc8\xbe", TO_LOWER).unwrap(), b"\xe2\xb1\xa6");
    assert_eq!(fold(b"\xef\xa9\xac", REMOVE_DIACRITICS).unwrap(), b"\xf0\xa4\x8b\xae");
    // malformed input never panics
    assert!(fold(b"\xf0\x9f", TO_LOWER).is_err());
    assert!(fold(b"\xff", TO_LOWER).is_err());
    assert!(fold(b"x", 0).is_err());
}
