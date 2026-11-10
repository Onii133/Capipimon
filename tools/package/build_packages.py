"""Builds the registry files: capipimon .hmod, capipimon_models .hnmod and a .hpack that needs both.

    python tools/package/build_packages.py --out <folder> [--pack-only]

Writes native/data/capipimon-content.pack (models and cosmetics, read by api/src/pack.rs; not
committed), then <out>/content-stage (the .hmod's files, unpacked, for tests) and the three files.
Zips are byte-for-byte repeatable: the server compares the .hmod's SHA-256 with each player's.
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import struct
import zipfile

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
CONTENT = os.path.join(ROOT, "content")
NATIVE = os.path.join(ROOT, "native")
PACK = os.path.join(NATIVE, "data", "capipimon-content.pack")

MAGIC = b"CAPIPAK1"
PACKED_FOLDERS = ("cobblemon-models", "cosmetics")
# The horizon-mixins commit Capipimon is built and tested with (native/built.json after a local build).
MIXINS_REV = "5a64af938ec86b00d53d16a408aabc0038fb3e57"
GAME = "26.3"
EPOCH = (1980, 1, 1, 0, 0, 0)


def files_under(base):
    """Every file under base, as sorted /-separated paths relative to it."""
    found = []
    for folder, dirs, names in os.walk(base):
        dirs[:] = [d for d in dirs if d != "target"]
        for name in names:
            found.append(os.path.relpath(os.path.join(folder, name), base).replace(os.sep, "/"))
    return sorted(found)


def read(path):
    with open(path, "rb") as f:
        return f.read()


def write_pack():
    names = [f"{top}/{rel}" for top in PACKED_FOLDERS for rel in files_under(os.path.join(CONTENT, top))]
    index, body, offset = {}, [], 0
    for name in names:
        data = read(os.path.join(CONTENT, *name.split("/")))
        index[name] = [offset, len(data)]
        body.append(data)
        offset += len(data)
    head = json.dumps(index, sort_keys=True, separators=(",", ":")).encode("utf-8")
    os.makedirs(os.path.dirname(PACK), exist_ok=True)
    with open(PACK, "wb") as f:
        f.write(MAGIC + struct.pack("<Q", len(head)) + head)
        f.writelines(body)
    print(f"pack: {len(names)} files, {os.path.getsize(PACK) / (1 << 20):.1f} MiB")


def write_zip(path, entries):
    """entries: {path in the zip: bytes}. Sorted, with a fixed date, so the bytes repeat."""
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for name in sorted(entries):
            info = zipfile.ZipInfo(name, EPOCH)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            z.writestr(info, entries[name], compresslevel=9)
    size = os.path.getsize(path) / (1 << 20)
    print(f"{os.path.basename(path)}: {len(entries)} entries, {size:.1f} MiB")


def sha256(path):
    return hashlib.sha256(read(path)).hexdigest()


def licence_files():
    """{file name: bytes} of the licences and credits that travel with the third-party art."""
    out = {f"licences/{rel}": read(os.path.join(CONTENT, "licences", rel))
           for rel in files_under(os.path.join(CONTENT, "licences"))}
    out["credits/credits.md"] = read(os.path.join(ROOT, "CREDITS.md"))
    out["credits/credits-addons.md"] = read(os.path.join(CONTENT, "CREDITS-ADDONS.md"))
    return out


def content_entries():
    entries = {"horizon-mod.toml": read(os.path.join(CONTENT, "horizon-mod.toml"))}
    for top in ("assets", "data"):
        for rel in files_under(os.path.join(CONTENT, top)):
            entries[f"{top}/{rel}"] = read(os.path.join(CONTENT, top, *rel.split("/")))
    for name, data in licence_files().items():
        entries[f"assets/capipimon/{name}"] = data
    return entries


def wanted_in_bundle(rel):
    """Only what a registry bundle may hold, and none of the repo's helper scripts."""
    if rel.startswith("tools/") or rel.endswith("Cargo.lock"):
        return False
    # Private server parts (docs/CLIENT_SERVER_SPLIT.md), in case this runs on the private repo.
    if rel.startswith(("server-marker/", "api/src/server/", "api/data/server/")):
        return False
    parts = rel.split("/")
    return rel.endswith((".rs", ".toml")) or "data" in parts[:2]


def native_entries(mod_id, version, horizon):
    base = f"mixins/{mod_id}"
    entries = {f"{base}/{rel}": read(os.path.join(NATIVE, *rel.split("/")))
               for rel in files_under(NATIVE) if wanted_in_bundle(rel)}
    entries[f"{base}/LICENSE"] = read(os.path.join(ROOT, "LICENSE"))
    for name, data in licence_files().items():
        entries[f"{base}/data/{name}"] = data
    manifest = {"schema": 1, "id": mod_id, "version": version, "game": GAME, "horizon": horizon, "mixins": MIXINS_REV}
    entries["horizon-native.json"] = (json.dumps(manifest, indent=2) + "\n").encode("utf-8")
    return entries


def toml_value(path, key):
    """The first `key = "value"` in a manifest (the tools here run on Python 3.10, without tomllib)."""
    return re.search(rf'^{key}\s*=\s*"([^"]+)"', read(path).decode("utf-8"), re.M).group(1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out")
    ap.add_argument("--pack-only", action="store_true")
    args = ap.parse_args()
    write_pack()
    if args.pack_only:
        return
    if not args.out:
        ap.error("--out is needed unless --pack-only")
    os.makedirs(args.out, exist_ok=True)

    content, native = os.path.join(CONTENT, "horizon-mod.toml"), os.path.join(NATIVE, "horizon-mixin.toml")
    cid, cver = toml_value(content, "id"), toml_value(content, "version")
    nid, nver = toml_value(native, "id"), toml_value(native, "version")

    entries = content_entries()
    stage = os.path.join(args.out, "content-stage")
    shutil.rmtree(stage, ignore_errors=True)
    for name, data in entries.items():
        path = os.path.join(stage, *name.split("/"))
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(data)
    hmod = os.path.join(args.out, f"{cid}-{cver}.hmod")
    write_zip(hmod, entries)

    hnmod = os.path.join(args.out, f"{nid}-{nver}.hnmod")
    write_zip(hnmod, native_entries(nid, nver, toml_value(native, "horizon_revision")))

    toml = (f'format = 1\nid = "capipimon_pack"\nname = "Capipimon"\nversion = "{cver}"\n'
            f'description = "Cobblemon-style PvP."\nauthors = ["Capippalist Trade Guild"]\ngame = "{GAME}"\n')
    for mid, ver, path in ((cid, cver, hmod), (nid, nver, hnmod)):
        toml += f'\n[[mods]]\nid = "{mid}"\nversion = "{ver}"\nsha256 = "{sha256(path)}"\n'
    write_zip(os.path.join(args.out, f"capipimon_pack-{cver}.hpack"), {"horizon-modpack.toml": toml.encode("utf-8")})


if __name__ == "__main__":
    main()
