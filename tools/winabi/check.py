#!/usr/bin/env python3
"""Check sard-windows' hand-written Windows ABI against the SDK headers.

sard binds Win32, COM (D3D11, DXGI, Media Foundation, WASAPI) and the NVIDIA
SDK by hand. This script extracts from `crates/windows/src/**.rs`:

  * every `com_interface!` method's vtable slot   -> offsetof(IFooVtbl, Method)
  * every interface IID                            -> DEFINE_GUID(IID_IFoo, ...)
  * `assert!(size_of::<T>() == N)` / `offset_of!`  -> sizeof / offsetof in C
  * `pub const NAME: <int> = value;`               -> the C constant of that name
  * `pub const NAME: GUID = GUID::from_u128(..)`   -> DEFINE_GUID(NAME, ...)

and checks them with clang against the mingw-w64 headers for x86_64 Windows
(no Windows machine or MSVC needed; nothing is linked or run):

    git clone --depth 1 --filter=blob:none --sparse https://github.com/mingw-w64/mingw-w64 /tmp/mingw
    (cd /tmp/mingw && git sparse-checkout set mingw-w64-headers)
    python3 tools/winabi/check.py /tmp/mingw/mingw-w64-headers

Exit status 0 when everything matches. Names that don't exist in the headers
(sard's own helpers) are listed as skipped.
"""

import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SRC = os.path.join(ROOT, "crates", "windows", "src")

INCLUDES = """
#define COBJMACROS
#define CINTERFACE
#include <winsock2.h>
#include <windows.h>
#include <stddef.h>
#include <shellapi.h>
#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <d3d10.h>
#include <d3dcompiler.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mftransform.h>
#include <mferror.h>
#include <codecapi.h>
#include <icodecapi.h>
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <ksmedia.h>
#include <iphlpapi.h>
#include <commctrl.h>
#include <shellscalingapi.h>
"""

# Rust name -> C name, where they differ.
ALIASES = {"ID3DBlob": "ID3D10Blob"}
GUID_ALIASES = {"D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0": "DXVA_ModeAV1_VLD_Profile0"}


def rust_files():
    for d, _, files in os.walk(SRC):
        for f in files:
            if f.endswith(".rs"):
                yield os.path.join(d, f)


def parse_rust():
    ifaces = []  # (name, iid, [(idx, method)])
    sizes = []  # (C expr, value, where)
    consts = []  # (name, value C expr, where)
    guids = []  # (name, u128, where)
    for path in rust_files():
        text = open(path, encoding="utf-8").read()
        rel = os.path.relpath(path, ROOT)
        for m in re.finditer(r"com_interface!\s*\{(.*?)\n\}", text, re.S):
            body = m.group(1)
            h = re.search(r"struct\s+(\w+)\((0x[0-9a-fA-F_]+)\)", body)
            if not h:
                continue
            methods = [(int(i), n) for i, n in re.findall(r"^\s*(\d+)\s+fn\s+(\w+)", body, re.M)]
            ifaces.append((h.group(1), int(h.group(2).replace("_", ""), 16), methods, rel))
        for m in re.finditer(r"size_of::<(\w+)>\(\)\s*==\s*(\d+)", text):
            sizes.append((f"sizeof({ALIASES.get(m.group(1), m.group(1))})", int(m.group(2)), rel))
        for m in re.finditer(r"offset_of!\((\w+),\s*(\w+)\)\s*==\s*(\d+)", text):
            sizes.append((f"offsetof({m.group(1)}, {m.group(2)})", int(m.group(3)), rel))
        for m in re.finditer(r"^\s*pub const (\w+): (u32|i32|u16|i64|u64|HRESULT) = ([^;]+);", text, re.M):
            name, _, val = m.groups()
            c = rust_int_to_c(val)
            if c is not None:
                consts.append((name, c, rel))
        for m in re.finditer(r"^\s*pub const (\w+): GUID = GUID::from_u128\((0x[0-9a-fA-F_]+)\);", text, re.M):
            guids.append((m.group(1), int(m.group(2).replace("_", ""), 16), rel))
    return ifaces, sizes, consts, guids


def rust_int_to_c(v):
    v = v.strip()
    neg = re.fullmatch(r"-(\d+)i32 as u32", v)
    if neg:
        return f"(unsigned)(-{neg.group(1)})"
    m = re.fullmatch(r"(0x[0-9A-Fa-f_]+|\d[\d_]*)(?:_?(u32|i32|u64|usize))?(?: as (i32|u32|HRESULT))?", v)
    if not m:
        return None
    return "(unsigned)" + m.group(1).replace("_", "") + "ull"


def header_guids(hdr_dir):
    guids = {}
    nums = r"\s*(0x[0-9A-Fa-f]+)L?\s*,\s*(0x[0-9A-Fa-f]+)\s*,\s*(0x[0-9A-Fa-f]+)\s*," + r"\s*(0x[0-9A-Fa-f]+)\s*," * 7
    pats = [
        re.compile(r"(?:DEFINE_GUID|EXTERN_GUID)\s*\(\s*(\w+)\s*," + nums + r"\s*(0x[0-9A-Fa-f]+)\s*\)"),
        # codecapi.h: #define STATIC_CODECAPI_AVLowLatencyMode 0x9c27891a, 0xed7a, ...
        re.compile(r"#define STATIC_(CODECAPI_\w+)" + nums + r"\s*(0x[0-9A-Fa-f]+)"),
    ]
    for d in ("include",):
        base = os.path.join(hdr_dir, d)
        for f in os.listdir(base):
            if not f.endswith(".h"):
                continue
            text = open(os.path.join(base, f), encoding="latin-1").read()
            for m in (m for pat in pats for m in pat.finditer(text)):
                g = [int(x, 16) for x in m.groups()[1:]]
                val = (g[0] << 96) | (g[1] << 80) | (g[2] << 64)
                for i, b in enumerate(g[3:]):
                    val |= b << (8 * (7 - i))
                guids.setdefault(m.group(1), val)
    return guids


def run_clang(hdr_dir, body):
    gen = os.path.join(tempfile.gettempdir(), "sard-winabi")
    os.makedirs(gen, exist_ok=True)
    mingw_h = os.path.join(gen, "_mingw.h")
    if not os.path.exists(mingw_h):
        src = open(os.path.join(hdr_dir, "crt", "_mingw.h.in")).read()
        src = (
            src.replace("@MINGW_HAS_SECURE_API@", "#define MINGW_HAS_SECURE_API 1")
            .replace("@DEFAULT_WIN32_WINNT@", "0x0A00")
            .replace("@DEFAULT_MSVCRT_VERSION@", "0xE00")
            .replace("@MINGW_HAS_DDK@", "")
        )
        open(mingw_h, "w").write(src)
    c = os.path.join(gen, "check.c")
    open(c, "w").write(INCLUDES + body)
    cmd = [
        "clang", "--target=x86_64-w64-mingw32", "-fsyntax-only", "-w", "-ferror-limit=0",
        "-I" + gen, "-I" + os.path.join(hdr_dir, "crt"), "-I" + os.path.join(hdr_dir, "include"),
        "-D_WIN32_WINNT=0x0A00", "-DWINVER=0x0A00", "-DNTDDI_VERSION=0x0A00000C", c,
    ]
    r = subprocess.run(cmd, capture_output=True, text=True)
    return r.returncode, r.stderr, c


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    hdr = sys.argv[1]
    ifaces, sizes, consts, guids = parse_rust()
    hguids = header_guids(hdr)
    failures = []
    skipped = []
    lines = []
    tags = {}

    def add(expr, tag):
        tags[len(lines)] = tag
        lines.append(f'_Static_assert({expr}, "{len(lines)}");')

    for name, iid, methods, rel in ifaces:
        cname = ALIASES.get(name, name)
        hg = hguids.get("IID_" + cname)
        if hg is None:
            skipped.append(f"IID_{cname} (not in headers)")
        elif hg != iid:
            failures.append(f"{rel}: {name} IID {iid:032x} != headers {hg:032x}")
        for idx, meth in methods:
            add(f"offsetof({cname}Vtbl, {meth}) == {idx} * sizeof(void*)", f"{rel}: {name}::{meth} slot {idx}")
    for expr, val, rel in sizes:
        add(f"{expr} == {val}", f"{rel}: {expr} == {val}")
    for name, val, rel in consts:
        add(f"(unsigned)({name}) == {val}", f"{rel}: const {name}")
    for name, val, rel in guids:
        hg = hguids.get(GUID_ALIASES.get(name, name))
        if hg is None:
            skipped.append(f"{name} (GUID not in headers)")
        elif hg != val:
            failures.append(f"{rel}: GUID {name} {val:032x} != headers {hg:032x}")

    # Drop asserts naming things the headers don't have, then check the rest.
    for _ in range(4):
        code, err, cfile = run_clang(hdr, "\n".join(lines) + "\n")
        undeclared = set()
        bad = []
        prefix = len(INCLUDES.splitlines())
        for m in re.finditer(r"check\.c:(\d+):\d+: error: (.*)", err):
            ln = int(m.group(1)) - prefix - 1
            msg = m.group(2)
            if ln not in tags:
                continue
            if "undeclared" in msg or "no member named" in msg or "unknown type" in msg or "incomplete" in msg:
                undeclared.add(ln)
            elif "static assertion failed" in msg or "static_assert" in msg:
                bad.append((ln, msg))
            else:
                undeclared.add(ln)
        if not undeclared:
            for ln, msg in bad:
                failures.append(f"{tags[ln]}: {msg}")
            break
        for ln in sorted(undeclared, reverse=True):
            skipped.append(tags[ln])
            lines[ln] = "/* skipped */"
    checked = sum(1 for l in lines if l.startswith("_Static")) + len(ifaces) + len(guids)
    for s in sorted(set(skipped)):
        print("skipped:", s)
    for f in failures:
        print("MISMATCH:", f)
    print(f"{checked} items checked, {len(set(skipped))} skipped, {len(failures)} mismatches")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
