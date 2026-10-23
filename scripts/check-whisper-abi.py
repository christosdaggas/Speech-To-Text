#!/usr/bin/env python3
from pathlib import Path
import hashlib
import json
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
vendor = root / 'vendor/whisper-rs-sys'
native = vendor / 'whisper.cpp'
expected = json.loads((vendor / 'upstream-files.json').read_text())
actual = {str(p.relative_to(native)): hashlib.sha256(p.read_bytes()).hexdigest()
          for p in native.rglob('*') if p.is_file()}
assert actual == expected, 'Pinned upstream source changed; review and update its provenance'
bindings = (vendor / 'src/bindings.rs').read_text()
c = ['#include <stddef.h>', '#include <stdio.h>', '#include "whisper.h"', '#include "ggml-backend.h"', 'int main(void) {']
rust = ['#![allow(dead_code, non_upper_case_globals, non_camel_case_types, non_snake_case)]',
        '#[path = '+json.dumps(str(vendor / 'src/bindings.rs'))+'] mod ffi;', 'fn main() {']
count = 0
labels = []
for name, body in re.findall(r'pub struct ((?:whisper_|ggml_backend_dev_)[A-Za-z0-9_]+) \{(.*?)\n\}', bindings, re.S):
    if '_bindgen' in name or '_unused' in body:
        continue
    c += [f'printf("%zu %zu\\n", sizeof(struct {name}), _Alignof(struct {name}));']
    rust += [f'println!("{{}} {{}}", std::mem::size_of::<ffi::{name}>(), std::mem::align_of::<ffi::{name}>());']
    labels.append(name+' size/alignment')
    for field in re.findall(r'pub ([A-Za-z0-9_]+):', body):
        c_field = field[:-1] if field in {'type_', 'ref_', 'async_'} else field
        c += [f'printf("%zu\\n", offsetof(struct {name}, {c_field}));']
        rust += [f'println!("{{}}", std::mem::offset_of!(ffi::{name}, {field}));']
        count += 1
        labels.append(name+'.'+field)
c += ['return 0; }']
rust += ['}']
with tempfile.TemporaryDirectory(prefix='stt-whisper-abi-') as directory:
    tmp = Path(directory)
    (tmp / 'probe.c').write_text('\n'.join(c))
    (tmp / 'probe.rs').write_text('\n'.join(rust))
    subprocess.run(['cc', '-std=c11', '-I'+str(native / 'include'), '-I'+str(native / 'ggml/include'), str(tmp/'probe.c'), '-o', str(tmp/'c')], check=True)
    subprocess.run(['rustc', '--edition=2021', str(tmp/'probe.rs'), '-o', str(tmp/'rust')], check=True)
    c_values = subprocess.check_output([str(tmp/'c')]).splitlines()
    rust_values = subprocess.check_output([str(tmp/'rust')]).splitlines()
    mismatches = [(label, a.decode(), b.decode()) for label, a, b in zip(labels, c_values, rust_values) if a != b]
    assert not mismatches, f'Native/Rust ABI layout mismatch (C, Rust): {mismatches}'
print(f'Pinned source hashes and {count} FFI field offsets/sizes/alignments verified.')
print('WHISPER_ABI_CHECKS_PASSED')
