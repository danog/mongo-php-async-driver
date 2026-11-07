#!/usr/bin/env python3
"""Summarises JUnit files (run-tests.php / PHPUnit) into $GITHUB_STEP_SUMMARY; exits 1 when a
test failed or errored, unless it is listed in expected-failures.txt (or when a file is missing:
the suite did not run)."""
import os, sys
import xml.etree.ElementTree as ET

with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "expected-failures.txt")) as f:
    entries = [l.strip() for l in f if l.strip() and not l.lstrip().startswith("#")]
# "<RUNNER_OS>: <entry>" applies on that runner OS only (Linux, macOS, Windows)
runner_os = os.environ.get("RUNNER_OS", "")
entries = [e.split(": ", 1)[1] if e.split(": ", 1)[0] in ("Linux", "macOS", "Windows") else e
           for e in entries
           if e.split(": ", 1)[0] not in ("Linux", "macOS", "Windows") or e.split(": ", 1)[0] == runner_os]
expected = {e for e in entries if not e.startswith("message: ")}
messages = [e[len("message: "):] for e in entries if e.startswith("message: ")]


def is_expected(name, text=None):
    # PHPT names are "<path> (<title>)"; PHPUnit data sets are "<Class::method> with data set ..."
    if any(name == e or name.startswith(e + " ") for e in expected):
        return True
    return text is not None and any(m in text for m in messages)


failed_total = 0
out = []
for label, path in zip(sys.argv[1::2], sys.argv[2::2]):
    if not os.path.exists(path):
        out.append(f"### {label}\n\n**no results: the suite did not run** (`{path}` missing)\n")
        failed_total += 1
        continue
    counts = {"pass": 0, "fail": 0, "error": 0, "skip": 0, "expected": 0}
    failures = []
    fixed = []
    for tc in ET.parse(path).iter("testcase"):
        name = (tc.get("class") + "::" if tc.get("class") else "") + (tc.get("name") or "")
        status = "pass"
        for c in tc:
            if c.tag in ("failure", "error"):
                status = "fail" if c.tag == "failure" else "error"
                if is_expected(name, (c.get("message") or "") + "\n" + (c.text or "")):
                    status = "expected"
                    break
                # PHPUnit's text starts with the test's name: show the first line after it
                msg = [l.strip() for l in (c.get("message") or (c.text or "")).splitlines()
                       if l.strip() and not l.strip().startswith(name)]
                failures.append((status, name, msg[0][:200] if msg else ""))
                break
            if c.tag == "skipped":
                status = "skip"
        if status == "pass" and is_expected(name):
            fixed.append(name)
        counts[status] += 1
    failed_total += counts["fail"] + counts["error"]
    out.append(f"### {label}\n\n| pass | fail | error | skip | expected failures |\n|---|---|---|---|---|\n"
               f"| {counts['pass']} | {counts['fail']} | {counts['error']} | {counts['skip']} | {counts['expected']} |\n")
    if fixed:
        out.append("Listed in `tools/ci/expected-failures.txt` but passing now:\n\n" +
                   "\n".join(f"- `{n}`" for n in fixed) + "\n")
    if failures:
        out.append("<details><summary>failures</summary>\n\n" +
                   "\n".join(f"- **{s}** `{n}` {m}" for s, n, m in failures[:500]) + "\n\n</details>\n")
text = "\n".join(out)
print(text)
if os.environ.get("GITHUB_STEP_SUMMARY"):
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
        f.write(text + "\n")
sys.exit(1 if failed_total else 0)
