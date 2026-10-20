"""Describes an image built from a case: its settings, its history and every entry of
every layer, in a form two builders can be compared by.

    describe.py bases SAVED.tar                      the diff_ids of the base images
    describe.py image SAVED.tar --bases BASES.json   an image saved by the reference
    describe.py store ROOT NAME --bases BASES.json   an image in nspawn's store
    describe.py compare GOLDEN.json ACTUAL.json      what differs, exit 1 if anything

Times are left out (the history's and the files'), and so is what names the builder.
A layer of a base image is described by its diff_id alone.
"""

import hashlib
import io
import json
import os
import sys
import tarfile

CONFIG_KEYS = [
    "User", "ExposedPorts", "Env", "Entrypoint", "Cmd", "Volumes", "WorkingDir",
    "Labels", "StopSignal", "Shell", "Healthcheck", "OnBuild", "ArgsEscaped",
]
TYPES = {
    tarfile.REGTYPE: "file", tarfile.AREGTYPE: "file", tarfile.DIRTYPE: "dir",
    tarfile.SYMTYPE: "symlink", tarfile.LNKTYPE: "hardlink", tarfile.CHRTYPE: "char",
    tarfile.BLKTYPE: "block", tarfile.FIFOTYPE: "fifo",
}


def sha256(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


# What a file holds of the day it was made: the shadow files' third field, the day of
# the last password change, which adduser takes from the clock.
DATED = {"etc/shadow", "etc/shadow-"}


def undated(path, content):
    if path not in DATED:
        return content
    lines = []
    for line in content.split(b"\n"):
        fields = line.split(b":")
        if len(fields) > 2:
            fields[2] = b""
        lines.append(b":".join(fields))
    return b"\n".join(lines)


def entries(data):
    """Every member of a layer tar, in order."""
    out = []
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:*") as tar:
        for m in tar:
            name = m.name[2:] if m.name.startswith("./") else m.name
            e = {
                "path": name.rstrip("/"),
                "type": TYPES.get(m.type, repr(m.type)),
                "mode": "%04o" % (m.mode & 0o7777),
                "uid": m.uid,
                "gid": m.gid,
            }
            if m.isreg():
                content = tar.extractfile(m).read()
                e["size"] = len(content)
                e["sha256"] = sha256(undated(e["path"], content))
            if m.issym() or m.islnk():
                e["link"] = m.linkname[2:] if m.linkname.startswith("./") else m.linkname
            if m.ischr() or m.isblk():
                e["dev"] = [m.devmajor, m.devminor]
            xattrs = {k[len("SCHILY.xattr."):]: v for k, v in m.pax_headers.items() if k.startswith("SCHILY.xattr.")}
            if xattrs:
                e["xattrs"] = xattrs
            out.append(e)
    return out


def uncompressed(data):
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:*") as tar:
        tar.getmembers()
    if data[:2] == b"\x1f\x8b":
        import gzip
        return gzip.decompress(data)
    return data


def describe(config, layers, bases):
    """config: the image config JSON; layers: the uncompressed layer tars, bottom first."""
    c = config.get("config") or {}
    history = []
    for h in config.get("history") or []:
        created_by = h.get("created_by", "")
        if created_by.endswith(" # buildkit"):
            created_by = created_by[: -len(" # buildkit")]
        item = {"created_by": created_by}
        if h.get("empty_layer"):
            item["empty_layer"] = True
        comment = h.get("comment") or ""
        # Each builder names itself in the comment of the steps it made.
        if comment.endswith(".dockerfile.v0"):
            comment = "dockerfile.v0"
        if comment:
            item["comment"] = comment
        history.append(item)
    described = []
    for data in layers:
        diff_id = sha256(data)
        if diff_id in bases:
            described.append({"diff_id": diff_id, "base": True})
        else:
            described.append({"diff_id": diff_id, "entries": entries(data)})
    return {
        "architecture": config.get("architecture"),
        "os": config.get("os"),
        "config": {k: c[k] for k in CONFIG_KEYS if c.get(k) not in (None, "", [], {})},
        "history": history,
        "layers": described,
    }


def saved_images(path):
    """(config, [layer tar bytes]) of every image of a saved tar."""
    with tarfile.open(path) as tar:
        def read(name):
            return tar.extractfile(name).read()
        manifest = json.loads(read("manifest.json"))
        for item in manifest:
            config = json.loads(read(item["Config"]))
            yield config, [uncompressed(read(layer)) for layer in item["Layers"]]


def store_image(root, name):
    manifest = json.load(open(os.path.join(root, "manifests", name + ".json")))

    def blob(digest):
        return open(os.path.join(root, "blobs", digest.replace(":", "-", 1)), "rb").read()

    config = json.loads(blob(manifest["config"]["digest"]))
    return config, [uncompressed(blob(layer["digest"])) for layer in manifest["layers"]]


def compare(golden, actual):
    problems = []
    for key in ("architecture", "os", "config", "history"):
        if golden.get(key) != actual.get(key):
            problems.append("%s: expected %s, got %s" % (key, json.dumps(golden.get(key)), json.dumps(actual.get(key))))
    gl, al = golden["layers"], actual["layers"]
    if len(gl) != len(al):
        problems.append("%d layers expected, %d made" % (len(gl), len(al)))
    for i, (g, a) in enumerate(zip(gl, al)):
        if g.get("base") or a.get("base"):
            if g["diff_id"] != a["diff_id"]:
                problems.append("layer %d: base %s expected, got %s" % (i, g["diff_id"], a["diff_id"]))
            continue
        ge, ae = g["entries"], a["entries"]
        # The same entries with other times (a context checked out at another moment,
        # files made by RUN at another time) make another tar: not a difference here.
        if ge == ae:
            continue
        gp = {e["path"]: e for e in ge}
        ap = {e["path"]: e for e in ae}
        for p in sorted(set(gp) | set(ap)):
            if gp.get(p) != ap.get(p):
                problems.append("layer %d: %s: expected %s, got %s" % (i, p, json.dumps(gp.get(p)), json.dumps(ap.get(p))))
        if [e["path"] for e in ge] != [e["path"] for e in ae] and set(gp) == set(ap):
            problems.append("layer %d: the entries come in another order" % i)
    return problems


def main(argv):
    if argv[0] == "bases":
        ids = sorted({sha256(layer) for _, layers in saved_images(argv[1]) for layer in layers})
        json.dump(ids, sys.stdout, indent=1)
        return 0
    if argv[0] == "compare":
        problems = compare(json.load(open(argv[1])), json.load(open(argv[2])))
        for p in problems:
            print(p)
        return 1 if problems else 0
    bases = set()
    if "--bases" in argv:
        bases = set(json.load(open(argv[argv.index("--bases") + 1])))
    if argv[0] == "image":
        config, layers = next(saved_images(argv[1]))
    elif argv[0] == "store":
        config, layers = store_image(argv[1], argv[2])
    else:
        print(__doc__, file=sys.stderr)
        return 2
    json.dump(describe(config, layers, bases), sys.stdout, indent=1, sort_keys=True)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
