#!/usr/bin/env python3
"""Admin-gated, retryable Linux releases. Uses only Python, Git, Cargo and gh."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile

REPOSITORY = "Ermesoml/storage-analitics"
TARGET = "x86_64-unknown-linux-musl"
SHA = re.compile(r"[0-9a-f]{40}")
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.0")


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def git(root, *args):
    return run("git", "-C", str(root), *args, capture_output=True,
               text=True).stdout.strip()


def api(path, method="GET", data=None, missing_ok=False):
    command = ["gh", "api", "--hostname", "github.com", "--method", method,
               f"repos/{REPOSITORY}/{path}"]
    if data is not None:
        command += ["--input", "-"]
    result = subprocess.run(command, input=json.dumps(data) if data is not None else None,
                            capture_output=True, text=True)
    if result.returncode:
        if missing_ok and "(HTTP 404)" in result.stderr:
            return None
        raise RuntimeError(f"GitHub API {method} {path} failed: {result.stderr.strip()}")
    return json.loads(result.stdout) if result.stdout.strip() else None


def authorize(repository, ref, actors, request=api):
    if repository != REPOSITORY or ref != "refs/heads/main":
        raise ValueError("Releases are allowed only from the upstream main branch.")
    for actor in set(actors):
        if not re.fullmatch(r"[A-Za-z0-9-]+(?:\[bot\])?", actor or ""):
            raise ValueError("Missing or invalid GitHub actor.")
        permission = request(f"collaborators/{actor}/permission")
        if permission.get("permission") != "admin":
            raise PermissionError(f"Release actor {actor} must be a repository admin.")


def history(root, head):
    if not SHA.fullmatch(head or ""):
        raise ValueError("Expected a full commit SHA.")
    base = json.loads((root / ".github/release-base.json").read_text())
    if not SHA.fullmatch(base["commit"]) or not VERSION.fullmatch(base["version"]):
        raise ValueError("Invalid release baseline.")
    commits = git(root, "rev-list", "--first-parent", head).splitlines()
    if base["commit"] not in commits:
        raise ValueError("The baseline must remain on main's first-parent history.")
    commits = list(reversed(commits[:commits.index(base["commit"])]))
    major, minor, _ = map(int, base["version"].split("."))
    return [{"sha": commit, "version": f"{major}.{minor + index}.0"}
            for index, commit in enumerate(commits, 1)]


def archive_name(version):
    if not VERSION.fullmatch(version):
        raise ValueError("Invalid release version.")
    return f"storage_analytics-v{version}-linux-x86_64.tar.gz"


def tag_commit(tag, request=api):
    ref = request(f"git/ref/tags/{tag}", missing_ok=True)
    if ref is None:
        return None
    obj = ref["object"]
    # Support an existing annotated tag, but never change its target.
    for _ in range(4):
        if obj["type"] == "commit":
            return obj["sha"]
        if obj["type"] != "tag":
            break
        obj = request(f"git/tags/{obj['sha']}")["object"]
    raise ValueError(f"{tag} does not resolve to a commit.")


def check_tag(release, request=api):
    tag = f"v{release['version']}"
    existing = tag_commit(tag, request)
    if existing is not None and existing != release["sha"]:
        raise ValueError(f"{tag} already points to a different commit.")
    return existing


def completed(release, remote):
    if remote.get("draft") or remote.get("prerelease"):
        return False
    assets = {asset["name"] for asset in remote.get("assets", [])
              if asset.get("state") == "uploaded" and asset.get("size", 0) > 0}
    return {archive_name(release["version"]), "SHA256SUMS"} <= assets


def pending(root, head, request=api):
    releases = []
    for release in history(root, head):
        existing = check_tag(release, request)
        remote = request(f"releases/tags/v{release['version']}", missing_ok=True)
        if remote is not None and not remote.get("draft"):
            if not completed(release, remote):
                raise ValueError(f"Published v{release['version']} has incomplete assets.")
            if existing is None:
                raise ValueError(f"Published v{release['version']} has lost its tag.")
            continue
        releases.append(release)
    return releases


def package(binary, source, output, version, timestamp):
    output.mkdir(parents=True, exist_ok=True)
    archive = output / archive_name(version)
    # Stable archive metadata also makes interrupted draft uploads retryable.
    with archive.open("wb") as file, gzip.GzipFile(fileobj=file, mode="wb", mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w") as tar:
            for path, name, mode in [(binary, "storage_analytics", 0o755),
                                      (source / "README.md", "README.md", 0o644)]:
                content = path.read_bytes()
                info = tarfile.TarInfo(name)
                info.size, info.mode, info.mtime = len(content), mode, timestamp
                tar.addfile(info, io.BytesIO(content))
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (output / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n")


def build(root, releases, output):
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="storage-analytics-release-") as directory:
        work = Path(directory)
        for release in releases:
            source = work / "source"
            run("git", "-C", str(root), "worktree", "add", "--detach", str(source), release["sha"])
            try:
                env = {key: value for key, value in os.environ.items()
                       if key not in {"GH_TOKEN", "GITHUB_TOKEN"}}
                env.update(STORAGE_ANALYTICS_VERSION=release["version"],
                           CARGO_TARGET_DIR=str(work / "target"))
                run("rustup", "target", "add", TARGET, cwd=source, env=env)
                run("cargo", "test", "--locked", "--target", TARGET, cwd=source, env=env)
                run("cargo", "build", "--locked", "--release", "--target", TARGET, cwd=source, env=env)
                binary = work / "target" / TARGET / "release/storage_analytics"
                version = run(str(binary), "--version", capture_output=True, text=True, env=env).stdout.strip()
                if version != f"storage_analytics {release['version']}":
                    raise ValueError("Built executable reports the wrong version.")
                run("python3", str(root / "tests/smoke_tui.py"), str(binary), cwd=source, env=env)
                timestamp = int(git(root, "show", "-s", "--format=%ct", release["sha"]))
                package(binary, source, output / release["version"], release["version"], timestamp)
            finally:
                run("git", "-C", str(root), "worktree", "remove", "--force", str(source))
    (output / "releases.json").write_text(json.dumps(releases, indent=2) + "\n")


def publish(release, directory, request=api, upload=None):
    version, commit = release["version"], release["sha"]
    tag = f"v{version}"
    existing = check_tag(release, request)
    remote = request(f"releases/tags/{tag}", missing_ok=True)
    if remote is not None and not remote.get("draft"):
        if completed(release, remote):
            if existing is None:
                raise ValueError(f"Published {tag} has lost its tag; refusing to recreate it.")
            return
        raise ValueError(f"Published {tag} has incomplete assets; refusing to overwrite it.")
    paths = [directory / archive_name(version), directory / "SHA256SUMS"]
    digest = hashlib.sha256(paths[0].read_bytes()).hexdigest()
    if paths[1].read_text() != f"{digest}  {paths[0].name}\n":
        raise ValueError("Release checksum does not match its archive.")
    if existing is None:
        request("git/refs", "POST", {"ref": f"refs/tags/{tag}", "sha": commit})
    if remote is None:
        remote = request("releases", "POST", {
            "tag_name": tag,
            # The tag already identifies the exact source. This field is unused
            # for existing tags; main avoids requesting workflow-modifying access.
            "target_commitish": "main",
            "name": tag, "draft": True, "prerelease": False,
            "body": f"Linux x86_64 static executable from commit `{commit}`.\n\n"
                    "Extract the archive and run `./storage_analytics`. "
                    "Verify the download with `sha256sum -c SHA256SUMS`.\n",
        })
    if upload is None:
        def upload(paths):
            run("gh", "release", "upload", tag, *map(str, paths), "--clobber",
                "--repo", f"github.com/{REPOSITORY}")
    upload(paths)  # Replacement is allowed only while the release is still a draft.
    remote = request(f"releases/{remote['id']}")
    expected = {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in paths}
    uploaded = {asset["name"]: asset for asset in remote.get("assets", [])}
    for name, digest in expected.items():
        asset = uploaded.get(name, {})
        if asset.get("state") != "uploaded" or asset.get("digest") != f"sha256:{digest}":
            raise ValueError(f"GitHub did not confirm the upload checksum for {name}.")
    if check_tag(release, request) != commit:
        raise ValueError(f"{tag} disappeared before publication.")
    request(f"releases/{remote['id']}", "PATCH", {"draft": False, "make_latest": "legacy"})
    print(f"Published {tag} from {commit}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["authorize", "plan", "build", "publish"])
    parser.add_argument("--head")
    parser.add_argument("--directory", type=Path, default=Path("dist"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    if args.command == "authorize":
        authorize(os.environ.get("GITHUB_REPOSITORY"), os.environ.get("GITHUB_REF"),
                  [os.environ.get("RELEASE_ACTOR"), os.environ.get("RELEASE_TRIGGERING_ACTOR")])
    elif args.command == "plan":
        plan = pending(root, args.head)
        args.directory.mkdir(parents=True, exist_ok=True)
        (args.directory / "releases.json").write_text(json.dumps(plan, indent=2) + "\n")
        print(f"{len(plan)} commit(s) need a Linux release.")
    else:
        releases = json.loads((args.directory / "releases.json").read_text())
        valid = history(root, args.head)
        if any(release not in valid for release in releases) or len({r["sha"] for r in releases}) != len(releases):
            raise ValueError("Release manifest does not match main history.")
        if args.command == "build":
            build(root, releases, args.directory)
        else:
            authorize(os.environ.get("GITHUB_REPOSITORY"), os.environ.get("GITHUB_REF"),
                      [os.environ.get("RELEASE_ACTOR"), os.environ.get("RELEASE_TRIGGERING_ACTOR")])
            for release in releases:
                publish(release, args.directory / release["version"])


if __name__ == "__main__":
    main()
