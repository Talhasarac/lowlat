#!/usr/bin/env python3
"""Fetch the Windows Driver Kit the display driver builds against.

The kit is Microsoft's NuGet package, pinned by version and by the SHA-512
digest its catalogue publishes, unpacked into target/wdk/<version>. Nothing is
installed: the binding script, the driver's link and its packaging read the
headers, the stub libraries and the tools from there, on this machine and in
CI alike. A second run with the package already unpacked does nothing.

Usage:
    python scripts/fetch-wdk.py            # print the unpacked kit's root
"""

import base64
import hashlib
import os
import shutil
import sys
import tempfile
import urllib.parse
import urllib.request
import zipfile

PACKAGE = "Microsoft.Windows.WDK.x64"
VERSION = "10.0.26100.6584"
# The package catalogue's own figures for this version.
SHA512 = "jhddaBnhMDqt3GVr32RVTtaR0KHmZDjY0JCTMn10OQpk4+AShXCK9QA09vBazp8ni1Mju84qM5SGXfIfnyOJ+g=="
SIZE = 110872506

URL = "https://api.nuget.org/v3-flatcontainer/{0}/{1}/{0}.{1}.nupkg".format(
    PACKAGE.lower(), VERSION
)
MARKER = ".lowlat-unpacked"


def root():
    return os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))


def download(path):
    request = urllib.request.Request(URL, headers={"User-Agent": "lowlat-fetch-wdk"})
    digest = hashlib.sha512()
    size = 0
    with urllib.request.urlopen(request, timeout=120) as response, open(path, "wb") as out:
        while True:
            block = response.read(1 << 20)
            if not block:
                break
            digest.update(block)
            size += len(block)
            out.write(block)
    if size != SIZE:
        raise SystemExit("fetch-wdk: %s is %d bytes, expected %d" % (URL, size, SIZE))
    got = base64.b64encode(digest.digest()).decode("ascii")
    if got != SHA512:
        raise SystemExit("fetch-wdk: digest mismatch for %s: %s" % (URL, got))


def unpack(archive, dest):
    base = os.path.realpath(dest)
    with zipfile.ZipFile(archive) as z:
        for info in z.infolist():
            # Package entries are percent-encoded paths; nothing may land
            # outside the destination.
            name = urllib.parse.unquote(info.filename)
            target = os.path.realpath(os.path.join(base, name))
            if os.path.commonpath([base, target]) != base:
                raise SystemExit("fetch-wdk: entry escapes the destination: %r" % name)
            if name.endswith("/"):
                os.makedirs(target, exist_ok=True)
                continue
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with z.open(info) as src, open(target, "wb") as out:
                shutil.copyfileobj(src, out)


def main():
    dest = os.path.join(root(), "target", "wdk", VERSION)
    marker = os.path.join(dest, MARKER)
    if os.path.exists(marker) and open(marker, encoding="ascii").read().strip() == SHA512:
        print(dest)
        return
    if os.path.exists(dest):
        shutil.rmtree(dest)
    os.makedirs(dest)
    fd, archive = tempfile.mkstemp(suffix=".nupkg", dir=os.path.dirname(dest))
    os.close(fd)
    try:
        print("fetch-wdk: downloading %s %s" % (PACKAGE, VERSION), file=sys.stderr)
        download(archive)
        unpack(archive, dest)
    finally:
        os.remove(archive)
    with open(marker, "w", encoding="ascii") as out:
        out.write(SHA512 + "\n")
    print(dest)


if __name__ == "__main__":
    main()
