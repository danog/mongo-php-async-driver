//
// Copyright (C) 2025-present MongoDB, Inc.
// 
// This program is free software: you can redistribute it and/or modify
// it under the terms of the Server Side Public License, version 1,
// as published by MongoDB, Inc.
// 
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// Server Side Public License for more details.
// 
// You should have received a copy of the Server Side Public License
// along with this program. If not, see
// <http://www.mongodb.com/licensing/server-side-public-license>.
// 
// As a special exception, the copyright holders give permission to link the
// code of portions of this program with the OpenSSL library under certain
// conditions as described in each individual source file and distribute
// linked combinations including the program with the OpenSSL library. You
// must comply with the Server Side Public License in all respects for
// all of the code used other than as permitted herein. If you modify file(s)
// with this exception, you may extend this exception to your version of the
// file(s), but you are not obligated to do so. If you do not wish to do so,
// delete this exception statement from your version. If you delete this
// exception statement from all source files in the program, then also delete
// it in the license file.
// 
// THIS IS A GENERATED FILE, DO NOT MODIFY.
//
//
// Ported from libmongocrypt's src/unicode/fold.c (a C translation of
// mongo::unicode::String::caseFoldAndStripDiacritics). Licensed under the SSPL v1 (see NOTICE).

//! Unicode case folding and diacritic removal for Queryable Encryption string search.

mod case_fold_map;
mod diacritic_fold_map;

/// kUnicodeFoldToLower / kUnicodeFoldRemoveDiacritics
pub const TO_LOWER: u32 = 1;
pub const REMOVE_DIACRITICS: u32 = 2;

fn lookup(table: &[(u32, u32)], cp: u32) -> Option<u32> {
    table.binary_search_by_key(&cp, |&(k, _)| k).ok().map(|i| table[i].1)
}

/// `unicode_codepoint_to_lower`
pub fn to_lower(cp: u32) -> u32 {
    if cp <= 0x7f {
        return if (b'A' as u32..=b'Z' as u32).contains(&cp) { cp | 0x20 } else { cp };
    }
    lookup(&case_fold_map::CASE_FOLD, cp).unwrap_or(cp)
}

/// `unicode_codepoint_remove_diacritics`: 0 for a pure diacritic
pub fn remove_diacritics(cp: u32) -> u32 {
    lookup(&diacritic_fold_map::DIACRITIC_FOLD, cp).unwrap_or(cp)
}

/// `append_utf8_codepoint`
fn append_utf8(cp: u32, out: &mut Vec<u8>) -> Result<(), String> {
    if cp <= 0x7f {
        out.push(cp as u8);
    } else if cp <= 0x7ff {
        out.extend_from_slice(&[(cp >> 6) as u8 | 0xc0, (cp & 0x3f) as u8 | 0x80]);
    } else if cp <= 0xffff {
        out.extend_from_slice(&[(cp >> 12) as u8 | 0xe0, ((cp >> 6) & 0x3f) as u8 | 0x80, (cp & 0x3f) as u8 | 0x80]);
    } else {
        if cp > 0x10FFFF {
            return Err("append_utf8_codepoint: codepoint was out of range for UTF-8".into());
        }
        out.extend_from_slice(&[
            (cp >> 18) as u8 | 0xf0,
            ((cp >> 12) & 0x3f) as u8 | 0x80,
            ((cp >> 6) & 0x3f) as u8 | 0x80,
            (cp & 0x3f) as u8 | 0x80,
        ]);
    }
    Ok(())
}

/// `unicode_fold`: `options` is TO_LOWER and/or REMOVE_DIACRITICS
pub fn fold(s: &[u8], options: u32) -> Result<Vec<u8>, String> {
    if options & (TO_LOWER | REMOVE_DIACRITICS) == 0 {
        return Err("unicode_fold: Either case or diacritic folding must be enabled".into());
    }
    let mut out = Vec::with_capacity(2 * s.len() + 1);
    let mut i = 0;
    while i < s.len() {
        let first = s[i];
        i += 1;
        let mut cp;
        if first <= 0x7f {
            if options & TO_LOWER != 0 && first.is_ascii_uppercase() {
                cp = (first | 0x20) as u32;
            } else {
                // ASCII's two pure diacritics
                if options & REMOVE_DIACRITICS != 0 && (first == b'^' || first == b'`') {
                    continue;
                }
                cp = first as u32;
            }
        } else {
            let leading_ones = first.leading_ones() as usize;
            if !(leading_ones > 1 && leading_ones <= 4 && i + (leading_ones - 1) <= s.len()) {
                return Err("unicode_fold: Text contains invalid UTF-8".into());
            }
            cp = (first & (0xff >> leading_ones)) as u32;
            for _ in 1..leading_ones {
                cp = (cp << 6) | (s[i] & 0x3f) as u32;
                i += 1;
            }
            if options & TO_LOWER != 0 {
                cp = to_lower(cp);
            }
            if options & REMOVE_DIACRITICS != 0 {
                cp = remove_diacritics(cp);
                if cp == 0 {
                    continue;
                }
            }
        }
        append_utf8(cp, &mut out)?;
    }
    Ok(out)
}
