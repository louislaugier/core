#!/usr/bin/env python3
"""Fork CI (12/11/2026): clippy findings of the fork vs those of pristine 5.0.3.

Both trees are linted the way upstream's static_analysis step lints them (--workspace
--all-targets, -D warnings), but with --cap-lints warn, so that a finding in one crate no longer
stops every crate that depends on it (on 4.15 upstream's own findings hid the fork's crates).
Usage: clippy_gate.py <head.jsonl> <head exit> <base.jsonl> <base exit> <base_sha> <label>
(<x>.jsonl: stdout of `cargo clippy --message-format=json ...`, <x exit>: its exit status)

Each finding of HEAD (lint warning or hard error) is matched with the base's findings after
mapping the base's line numbers through `git diff base HEAD`: one the base has at the same place
(file, line, column) with the same lint or error code is upstream's own (it fails upstream's CI
the same way); any other is new with the fork's diff and fails the step. Results are published
as annotations, which GitHub's public check-runs API serves without a login (job logs need
admin rights).
"""
import json
import re
import subprocess
import sys


def primary(span_list):
    for s in span_list:
        if s.get('is_primary'):
            # a span inside a macro definition: report its call site in our source
            while s.get('expansion') and s['file_name'].startswith('/'):
                s = s['expansion']['span']
            return s
    return None


def findings(path):
    """{(file, line, col, code, text): level} of the cargo JSON output at `path`."""
    found = {}
    for raw in open(path, errors='replace'):
        try:
            msg = json.loads(raw)
        except ValueError:
            continue
        if msg.get('reason') != 'compiler-message':
            continue
        m = msg['message']
        level, text = m.get('level'), m.get('message', '')
        if level not in ('warning', 'error') or text.startswith('aborting due to'):
            continue
        if re.match(r'^\d+ warnings? emitted$', text):
            continue
        code = (m.get('code') or {}).get('code') or ''
        if level == 'error':
            code = 'error' + (f'[{code}]' if code else '')
        s = primary(m.get('spans') or [])
        if s is None:
            key = ('-', 0, 0, code, text)
        else:
            key = (s['file_name'], s['line_start'], s['column_start'], code, text)
        found[key] = level
    return found


def hunks(base):
    """{file: [(old_start, old_len, new_start, new_len)]} of `git diff -U0 base HEAD`."""
    out = subprocess.run(['git', 'diff', '-U0', '--no-color', base, 'HEAD', '--', '*.rs'],
                         capture_output=True, text=True, check=True).stdout
    res, cur = {}, None
    for line in out.splitlines():
        if line.startswith('+++ '):
            cur = None if line.endswith('/dev/null') else line[6:]
            res.setdefault(cur, [])
        elif line.startswith('@@') and cur is not None:
            m = re.match(r'@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@', line)
            o = int(m[1])
            ol = 1 if m[2] is None else int(m[2])
            n = int(m[3])
            nl = 1 if m[4] is None else int(m[4])
            res[cur].append((o, ol, n, nl))
    return res


def changed(file, line, hs):
    return any(n <= line < n + nl for _o, _ol, n, nl in hs.get(file, []))


def map_old(file, line, hs):
    """Line number in HEAD of an unchanged base line, None if the fork changed that line."""
    shift = 0
    for o, ol, _n, nl in hs.get(file, []):
        if ol == 0:  # pure insertion after old line o
            if line > o:
                shift += nl
            continue
        if o <= line < o + ol:
            return None
        if line >= o + ol:
            shift += nl - ol
    return line + shift


def publish(level, title, lines):
    """Workflow-command annotations, 40 lines each (GitHub keeps 10 per level and step)."""
    for i in range(0, min(len(lines), 400), 40):
        body = '%0A'.join(x.replace('%', '%25').replace('\r', '') for x in lines[i:i + 40])
        prop = f'{title} ({i // 40 + 1})'.replace('%', '%25').replace(':', '%3A').replace(',', '%2C')
        print(f'::{level} title={prop}::{body}')


def main():
    head_log, head_rc, base_log, base_rc, base, label = sys.argv[1:7]
    head, basef = findings(head_log), findings(base_log)
    hs = hunks(base)
    # Matched on place and code, not on the text: a count in the text may move with the fork
    # (e.g. too_many_arguments "(10/7)" where 5.0.3 says "(9/7)" for the same function).
    mapped = {}
    for (f, ln, col, code, text) in basef:
        if f == '-':
            mapped[('-', 0, 0, code, text)] = text
            continue
        new = map_old(f, ln, hs)
        if new is not None:
            mapped[(f, new, col, code)] = text
    ours, upstream, by_code = [], [], {}
    for key in sorted(head):
        f, ln, col, code, text = key
        line = f'{f}:{ln}:{col}: {code}: {text}'
        base_text = mapped.get(key) if f == '-' else mapped.get((f, ln, col, code))
        if base_text is not None:
            upstream.append(line + ('' if base_text == text else f'   [5.0.3: {base_text}]'))
            by_code[code] = by_code.get(code, 0) + 1
        elif f == '-':
            ours.append(line + '   [no location]')
        else:
            ours.append(line + ('   [changed line]' if changed(f, ln, hs) else '   [upstream line]'))
    rc_note = []
    if head_rc != '0' and base_rc == '0':
        rc_note = [f'cargo clippy exited {head_rc} on the fork but 0 on 5.0.3']
    summary = (f'{label}: fork {len(head)} findings (cargo exit {head_rc}), 5.0.3 {len(basef)} '
               f'(cargo exit {base_rc}); shared with 5.0.3 (upstream\'s own) {len(upstream)}; '
               f'new with the fork {len(ours)}')
    print(summary)
    for x in rc_note + ours:
        print('FORK ' + x)
    if rc_note or ours:
        publish('error', f'{label} - new with the fork', rc_note + ours)
    codes = ', '.join(f'{k} {v}' for k, v in sorted(by_code.items(), key=lambda kv: -kv[1]))
    publish('notice', f'{label} - upstream findings (same place and lint on {base[:9]})',
            [summary, 'by lint: ' + codes] + upstream)
    sys.exit(1 if ours or rc_note else 0)


if __name__ == '__main__':
    main()
