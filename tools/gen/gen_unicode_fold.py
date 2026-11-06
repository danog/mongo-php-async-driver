#!/usr/bin/env python3
"""Converts libmongocrypt's generated Unicode fold maps (src/unicode/{case,diacritic}-fold-map.c,
MongoDB's SSPL-licensed files) into Rust tables in crates/crypt/src/unicode/. The output keeps
MongoDB's SSPL header: those files are licensed under the SSPL v1 (see NOTICE).

usage: gen_unicode_fold.py <libmongocrypt/src/unicode> <crates/crypt/src/unicode>"""
import re, sys, os

src, dst = sys.argv[1], sys.argv[2]
for name, fn in [('case-fold-map', 'CASE_FOLD'), ('diacritic-fold-map', 'DIACRITIC_FOLD')]:
    c = open(os.path.join(src, name + '.c'), encoding='utf-8').read()
    header = c[:c.index('*/') + 2]
    header = '\n'.join('// ' + l.strip().lstrip('/*').lstrip('*').strip() if l.strip() not in ('/**', '*/') else '//' for l in header.splitlines())
    pairs = [(int(a, 16), int(b, 16)) for a, b in re.findall(r'case (0x[0-9a-f]+): return (0x[0-9a-f]+);', c)]
    assert pairs == sorted(pairs) and len({a for a, _ in pairs}) == len(pairs), name
    out = [header, '//', '// Converted from libmongocrypt\'s src/unicode/%s.c by tools/gen/gen_unicode_fold.py.' % name, '',
           '/// (codepoint, mapped codepoint), sorted by codepoint; 0 means "a pure diacritic, removed"',
           'pub static %s: [(u32, u32); %d] = [' % (fn, len(pairs))]
    out += ['    (0x%x, 0x%x),' % p for p in pairs]
    out += ['];', '']
    open(os.path.join(dst, name.replace('-', '_') + '.rs'), 'w').write('\n'.join(out))
    print(name, len(pairs))
