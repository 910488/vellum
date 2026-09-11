#!/usr/bin/env python3
"""Build a deterministic SWE-bench grader image for one pinned instance.

Phase 2 canonical replacement for `swebench.harness.prepare_images` on this
machine. Never trusts floating `ubuntu:*` tags or unpinned conda/pip
resolution. The image chain is:

  baseseed       docker build (network)  : apt packages from a pinned
                                           snapshot.ubuntu.com timestamp
  envseed        FROM basecanonical (net): miniconda + one-time conda
                                           resolution (python + pytest)
                                           -> host captures env spec
  envcanonical   FROM scratch (offline)  : normalized env rootfs
  instseed       FROM envcanonical (net) : whole repo history + pip resolve
                                           -> host captures pip lock + wheelhouse
  lockedseed     FROM envcanonical (off) : locked wheelhouse + eval script
  instcanonical  FROM scratch (offline)  : normalized grader rootfs

Each canonical image is built twice from the same normalized rootfs with
`--no-cache --network none` and the tool fails unless both runs
produce the same `.Id`. Only then is the instance image tagged with the
official `sweb.eval.x86_64.<instance>:latest` key so `prepare_swebench.py`
and the harness keep working unchanged.

Every input is fixed in `grader-lock.json` (base digest, apt snapshot,
miniconda checksum, env spec, pip lock, wheelhouse, bundle, eval script,
SOURCE_DATE_EPOCH).

Usage:
  python evals/tools/build_swe_grader.py \
    --dataset ../public_datasets/swe-bench-verified.parquet \
    --selection evals/swebench/verified-12-selection.json \
    --instance psf__requests-1142 \
    --artifacts ../public_datasets/swebench/_grader \
    --create-lock \
    --tag-instance

Requires: swebench, pandas, pyarrow; a running Docker daemon.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import types
import urllib.request

import pandas as pd

BASE_IMAGE = "ubuntu:22.04@sha256:3b06811b2afd352be909dd088a004166d665dc76d38b13eada33522a9d915c6f"
MINICONDA_URL = (
    "https://repo.anaconda.com/miniconda/Miniconda3-py311_23.11.0-2-Linux-x86_64.sh"
)
MINICONDA_SHA256 = "c9ae82568e9665b1105117b4b1e499607d2a920f0aea6f94410e417a0eff1b9c"
SCHEMA_VERSION = 1
APT_SNAPSHOT_WINDOW_HOURS = 7 * 24

BASE_DOCKERFILE = """\
# syntax=docker/dockerfile:1.7
# vellum swebench BASE seed: packages from a pinned snapshot
FROM --platform=linux/amd64 {base_image}

ENV DEBIAN_FRONTEND=noninteractive TZ=Etc/UTC LANG=C.UTF-8 LC_ALL=C.UTF-8
ARG SOURCE_DATE_EPOCH
# Base image ships no CA bundle, so bootstrap ca-certificates from the
# pinned snapshot with TLS verification disabled (InRelease is GPG-signed,
# so authenticity still holds); the next RUN enables full verification.
ARG APT_SNAPSHOT
RUN rm -f /etc/apt/sources.list.d/* \\
 && printf '%s\\n' \\
      'deb {apt_snapshot}/ jammy main restricted universe multiverse' \\
      'deb {apt_snapshot}/ jammy-updates main restricted universe multiverse' \\
      'deb {apt_snapshot}/ jammy-security main restricted universe multiverse' \\
    > /etc/apt/sources.list \\
 && apt-get -o Acquire::https::Verify-Peer=false update \\
 && apt-get -o Acquire::https::Verify-Peer=false install -y --no-install-recommends ca-certificates \\
 && rm -rf /var/lib/apt/lists/* \\
 && find /var/log -type f -delete
RUN apt-get update \\
 && apt-get --allow-downgrades -y dist-upgrade \\
 && apt-get install -y --no-install-recommends \\
      wget git build-essential libffi-dev libtiff-dev python3 python3-pip \\
      python-is-python3 jq curl locales locales-all tzdata \\
 && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/partial /var/cache/apt/archives/*.deb \\
 && find /var/log -type f -delete

RUN adduser --disabled-password --gecos 'dog' nonroot
"""

CANONICAL_ROOTFS_DOCKERFILE = """\
# syntax=docker/dockerfile:1.7
FROM scratch
ARG SOURCE_DATE_EPOCH
ADD {rootfs_name} /
ENV DEBIAN_FRONTEND=noninteractive TZ=Etc/UTC LANG=C.UTF-8 LC_ALL=C.UTF-8 {path_env}
WORKDIR {workdir}
LABEL org.opencontainers.image.vendor="vellum" \
      org.opencontainers.image.title="swebench grader {stage}" \
      vellum.swebench.stage="{stage}" \
      vellum.swebench.instance="{instance}" \
      vellum.swebench.base-commit="{base_commit}"
"""

ENV_SEED_DOCKERFILE = """\
# syntax=docker/dockerfile:1.7
# vellum swebench ENV seed image (network resolution, one-time)
ARG BASE_IMAGE_REF
FROM ${BASE_IMAGE_REF}

ARG SOURCE_DATE_EPOCH
COPY miniconda.sh /root/miniconda.sh
ARG MINICONDA_SHA256
RUN echo "${MINICONDA_SHA256}  /root/miniconda.sh" | sha256sum -c - \\
 && bash /root/miniconda.sh -b -p /opt/miniconda3 \\
 && rm /root/miniconda.sh
ENV PATH=/opt/miniconda3/bin:$PATH
# Prefer IPv4: Docker Desktop on Windows often resolves conda-forge over
# broken IPv6 and then dies with HTTP 000 after a long hang. Do not append
# conda-forge; its linux-64 repodata is >400MB and is not required for the
# pinned python/pytest envs (they resolve from repo.anaconda.com).
RUN printf '%s\\n' 'precedence :ffff:0:0/96  100' >> /etc/gai.conf \\
 && conda init --all \\
 && conda config --system --set remote_max_retries 8 \\
 && conda config --system --set remote_connect_timeout_secs 30 \\
 && conda config --system --set remote_read_timeout_secs 180

{env_spec_copy}COPY seed_env.sh /root/seed_env.sh
RUN sed -i -e 's/\\r$//' /root/seed_env.sh \\
 && chmod +x /root/seed_env.sh \\
 && /bin/bash /root/seed_env.sh
"""

INSTANCE_SEED_DOCKERFILE = """\
# syntax=docker/dockerfile:1.7
# vellum swebench INSTANCE seed image (network resolution, one-time)
ARG ENV_IMAGE_REF
FROM ${ENV_IMAGE_REF}

ARG SOURCE_DATE_EPOCH
COPY testbed.bundle /root/testbed.bundle
COPY eval.sh /root/eval.sh
COPY seed_repo.sh /root/seed_repo.sh
RUN sed -i -e 's/\\r$//' /root/eval.sh /root/seed_repo.sh \\
 && chmod +x /root/eval.sh /root/seed_repo.sh \\
 && /bin/bash /root/seed_repo.sh

WORKDIR /testbed/
"""

INSTANCE_CANONICAL_DOCKERFILE = """\
# syntax=docker/dockerfile:1.7
# vellum swebench INSTANCE canonical grader image (offline, locked)
ARG ENV_IMAGE_REF
FROM ${ENV_IMAGE_REF}

ARG SOURCE_DATE_EPOCH
ARG INSTANCE_ID
ARG BASE_COMMIT
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH} PYTHONHASHSEED=0
COPY testbed.bundle /root/testbed.bundle
COPY eval.sh /root/eval.sh
COPY requirements.lock.txt /root/requirements.lock.txt
COPY wheelhouse.tar /root/wheelhouse.tar
COPY seed_repo.sh /root/seed_repo.sh
RUN sed -i -e 's/\\r$//' /root/eval.sh /root/seed_repo.sh \\
 && chmod +x /root/eval.sh /root/seed_repo.sh \\
 && mkdir -p /root/wheelhouse \\
 && tar -C /root/wheelhouse --strip-components=1 -xf /root/wheelhouse.tar \\
 && rm /root/wheelhouse.tar \\
 && /bin/bash /root/seed_repo.sh

# Build-time and hidden-grader materials must not remain in the image.
RUN rm -rf /root/testbed.bundle /root/seed_repo.sh /root/requirements.lock.txt \
           /root/wheelhouse /root/eval.sh

WORKDIR /testbed/
LABEL org.opencontainers.image.vendor="vellum" \\
      org.opencontainers.image.title="swebench grader" \\
      org.opencontainers.image.description="deterministic SWE-bench Verified grader image" \\
      vellum.swebench.instance="${INSTANCE_ID}" \\
      vellum.swebench.base-commit="${BASE_COMMIT}" \\
      vellum.swebench.grader-format="canonical"
"""


def run(*args: str, timeout: int | None = None) -> str:
    completed = subprocess.run(
        list(args),
        check=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )
    return completed.stdout.strip()


def run_out(args: list[str], destination: Path, timeout: int | None = None) -> None:
    with destination.open("wb") as stream:
        subprocess.run(
            list(args),
            check=True,
            encoding="utf-8",
            errors="replace",
            stdout=stream,
            stderr=subprocess.PIPE,
            timeout=timeout,
        )


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def combined_sha256(paths: list[Path]) -> str:
    digest = hashlib.sha256()
    for path in sorted(paths, key=lambda item: item.name):
        digest.update(path.name.encode("utf-8"))
        digest.update(b"\0")
        digest.update(bytes.fromhex(sha256(path)))
    return digest.hexdigest()


MAX_ARCHIVE_BYTES = 2 * 1024 * 1024 * 1024


def config_tag_prefix(digest: str) -> str:
    hex_digest = digest[7:] if digest.startswith("sha256:") else digest
    if len(hex_digest) != 64:
        raise SystemExit(f"invalid config digest: {digest}")
    return hex_digest[:12]


def archive_local_tag(instance_id: str, digest: str) -> str:
    return f"sweb.eval.x86_64.{instance_id.lower()}:cfg-{config_tag_prefix(digest)}"


def normalize_tar(source: Path, destination: Path) -> None:
    """Rewrite a docker-save tar with sorted entries and frozen metadata."""
    with tarfile.open(source, "r") as incoming, tarfile.open(destination, "w") as outgoing:
        members = sorted(incoming.getmembers(), key=lambda member: member.name)
        for member in members:
            member.mtime = 0
            member.uid = 0
            member.gid = 0
            member.uname = ""
            member.gname = ""
            if member.isfile():
                extracted = incoming.extractfile(member)
                outgoing.addfile(member, extracted)
            else:
                outgoing.addfile(member)


def gzip_mtime0(source: Path, destination: Path) -> None:
    import gzip

    with source.open("rb") as raw, destination.open("wb") as encoded:
        with gzip.GzipFile(filename="", mode="wb", fileobj=encoded, mtime=0) as stream:
            shutil.copyfileobj(raw, stream)


def export_image_archive(image: str, destination: Path) -> dict[str, object]:
    """Export `image` as a deterministic `.tar.gz` and return size/hash facts."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="vellum-image-export-") as temporary:
        raw = Path(temporary) / "image.tar"
        normalized = Path(temporary) / "image.normalized.tar"
        subprocess.run(
            ["docker", "save", "--output", str(raw), image],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        normalize_tar(raw, normalized)
        gzip_mtime0(normalized, destination)
        uncompressed = normalized.stat().st_size
    size = destination.stat().st_size
    if size > MAX_ARCHIVE_BYTES:
        destination.unlink(missing_ok=True)
        raise SystemExit(
            f"image archive for {image} is {size} bytes; assets over 2 GiB are forbidden"
        )
    return {
        "sha256": sha256(destination),
        "sizeBytes": size,
        "uncompressedSizeBytes": uncompressed,
        "compression": "gzip",
    }


def export_image_archive_twice(image: str, destination: Path) -> dict[str, object]:
    first = destination.with_suffix(".run-1.tar.gz")
    second = destination.with_suffix(".run-2.tar.gz")
    first_facts = export_image_archive(image, first)
    second_facts = export_image_archive(image, second)
    if first_facts != second_facts:
        raise SystemExit(f"image archive is not deterministic for {image}")
    first.replace(destination)
    second.unlink(missing_ok=True)
    return first_facts


def image_config_digest(image: str) -> str:
    """Return the image config blob digest, independent of the buildkit driver.

    Docker-format images expose ``config.digest`` as a descriptor annotation,
    but OCI-format images produced by a docker-container builder do not. The
    ``docker save`` OCI layout is the one representation that is identical for
    both, so fall back to reading the config digest out of its manifest.json.
    The config digest is the deterministic content identity: the manifest
    digest (``docker image inspect {{.Id}}``) can differ between the embedded
    ``docker`` driver and a ``docker-container`` builder even though the config
    and layer blobs are byte-for-byte identical.
    """
    descriptor = run(
        "docker",
        "image",
        "inspect",
        "--format",
        '{{ index .Descriptor.Annotations "config.digest" }}',
        image,
    )
    if descriptor.startswith("sha256:"):
        return descriptor
    with tempfile.TemporaryDirectory(prefix="vellum-config-digest-") as temporary:
        archive = Path(temporary) / "image.tar"
        subprocess.run(
            ["docker", "save", image, "-o", str(archive)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        with tarfile.open(archive, "r") as stream:
            manifest = json.loads(
                stream.extractfile("manifest.json").read().decode("utf-8")
            )
        config = manifest[0]["Config"]
        prefix = "blobs/sha256/"
        digest = config[len(prefix) :] if config.startswith(prefix) else config
        return digest if digest.startswith("sha256:") else f"sha256:{digest}"


def write_rootfs_manifest(archive: Path, destination: Path) -> None:
    """Write a bounded-content manifest for reproducibility diagnostics."""
    entries: list[dict[str, object]] = []
    with tarfile.open(archive, "r") as source:
        for member in source:
            row: dict[str, object] = {
                "path": member.name,
                "type": member.type.decode("ascii", errors="replace"),
                "mode": member.mode,
                "uid": member.uid,
                "gid": member.gid,
                "mtime": member.mtime,
                "size": member.size,
            }
            if member.linkname:
                row["link"] = member.linkname
            if member.isfile():
                stream = source.extractfile(member)
                digest = hashlib.sha256()
                if stream is not None:
                    for block in iter(lambda: stream.read(1024 * 1024), b""):
                        digest.update(block)
                row["sha256"] = digest.hexdigest()
            entries.append(row)
    destination.write_text(
        json.dumps(entries, ensure_ascii=False, separators=(",", ":")) + "\n",
        encoding="utf-8",
    )


def export_normalized_rootfs(
    image: str, destination: Path, source_date_epoch: str
) -> None:
    """Export one normalized rootfs tar from an otherwise disposable seed."""
    script = f"""
set -euo pipefail
rm -rf /tmp/* /var/tmp/* /var/cache/apt/archives/* /var/lib/apt/lists/*
rm -rf /root/.cache /opt/miniconda3/pkgs/*
find /var/log -type f -delete
rm -f /etc/machine-id /var/lib/dbus/machine-id
mkdir -p /testbed
if [ -d /testbed/.git ]; then git -C /testbed clean -ffdqx; fi
tar --sort=name --mtime='@{source_date_epoch}' --clamp-mtime \\
  --format=posix --pax-option=delete=atime,delete=ctime \\
  --numeric-owner --owner=0 --group=0 --one-file-system \\
  --exclude=./proc --exclude=./sys --exclude=./dev --exclude=./run \\
  --exclude=./etc/hosts --exclude=./etc/hostname --exclude=./etc/resolv.conf \\
  -C / -cf - .
""".strip()
    run_out(
        ["docker", "run", "--rm", "--network", "none", image, "bash", "-lc", script],
        destination,
        timeout=1800,
    )


def link_or_copy(source: Path, destination: Path) -> None:
    destination.unlink(missing_ok=True)
    try:
        os.link(source, destination)
    except OSError:
        shutil.copyfile(source, destination)


def load_test_spec(record: dict):
    if os.name == "nt" and "resource" not in sys.modules:
        resource = types.ModuleType("resource")
        resource.RLIMIT_NOFILE = 7
        resource.setrlimit = lambda *_args, **_kwargs: None
        sys.modules["resource"] = resource
    from swebench.harness.test_spec.test_spec import make_test_spec

    return make_test_spec(record)


def resolve_apt_snapshot(
    now: _dt.datetime, window_hours: int = APT_SNAPSHOT_WINDOW_HOURS
) -> str:
    """Return the newest available snapshot stamp <= now.

    snapshot.ubuntu.com keeps a take whenever the Ubuntu archive changes
    (typically a few times a day; retention >= 2 years). Directories do not
    list; probe a real file inside the take scope (`dists/jammy/Release`).
    """
    candidate = now.replace(microsecond=0, second=0, minute=0)
    for _ in range(window_hours // 2 + 1):
        stamp = candidate.strftime("%Y%m%dT%H%M%SZ")
        request = urllib.request.Request(
            f"https://snapshot.ubuntu.com/ubuntu/{stamp}/dists/jammy/Release",
            method="HEAD",
        )
        try:
            with urllib.request.urlopen(request, timeout=20) as resp:
                if resp.status == 200:
                    return stamp
        except Exception:
            pass
        candidate -= _dt.timedelta(hours=2)
    raise RuntimeError("no apt snapshot available within the probe window")


def ensure_miniconda(ctx: Path) -> None:
    target = ctx / "miniconda.sh"
    if target.exists() and sha256(target) == MINICONDA_SHA256:
        return
    _dt0 = _dt.datetime.now()
    url_stream = urllib.request.urlopen(MINICONDA_URL, timeout=600)
    with target.open("wb") as stream:
        while True:
            block = url_stream.read(1024 * 1024)
            if not block:
                break
            stream.write(block)
    if sha256(target) != MINICONDA_SHA256:
        raise SystemExit("miniconda download failed checksum verification")


def docker_build(
    ctx: Path,
    dockerfile: str,
    tag: str,
    build_args: list[tuple[str, str]],
    network: str,
    platform: str = "linux/amd64",
) -> str:
    env = os.environ.copy()
    env["DOCKER_BUILDKIT"] = "1"
    command = [
        "docker",
        "build",
        "--load",
        "--no-cache",
        "--progress=plain",
        "--provenance=false",
        "--sbom=false",
        f"--network={network}",
        f"--platform={platform}",
    ]
    for key, value in build_args:
        command += ["--build-arg", f"{key}={value}"]
    command += [
        "-t",
        tag,
        "-f",
        str((ctx / dockerfile).resolve()).replace("\\", "/"),
        str(ctx.resolve()).replace("\\", "/"),
    ]
    subprocess.run(
        command, check=True, env=env, cwd=str(ctx), encoding="utf-8", errors="replace"
    )
    return run("docker", "image", "inspect", "--format", "{{.Id}}", tag)


def canonicalize_seed(
    root: Path,
    seed_image: str,
    stage: str,
    instance: str,
    base_commit: str,
    source_date_epoch: str,
    path_env: str,
    workdir: str,
    verify_double_build: bool = True,
) -> tuple[str, str, Path, Path]:
    """Snapshot a seed once and prove two offline canonical builds match."""
    archive = root / f"{stage}.rootfs.tar"
    manifest = root / f"{stage}.rootfs-manifest.json"
    export_normalized_rootfs(seed_image, archive, source_date_epoch)
    write_rootfs_manifest(archive, manifest)

    image_id, tag = build_canonical_rootfs(
        root,
        archive,
        stage,
        instance,
        base_commit,
        source_date_epoch,
        path_env,
        workdir,
        verify_double_build,
    )
    return image_id, tag, archive, manifest


def build_canonical_rootfs(
    root: Path,
    archive: Path,
    stage: str,
    instance: str,
    base_commit: str,
    source_date_epoch: str,
    path_env: str,
    workdir: str,
    verify_double_build: bool = True,
) -> tuple[str, str]:
    """Build one or two offline images from an already normalized rootfs."""

    build_ctx = root / f"{stage}.canonical-context"
    build_ctx.mkdir(parents=True, exist_ok=True)
    linked_archive = build_ctx / "rootfs.tar"
    link_or_copy(archive, linked_archive)
    dockerfile = build_ctx / "Dockerfile"
    dockerfile.write_text(
        CANONICAL_ROOTFS_DOCKERFILE.format(
            rootfs_name="rootfs.tar",
            path_env=path_env,
            workdir=workdir,
            stage=stage,
            instance=instance,
            base_commit=base_commit,
        ),
        encoding="utf-8",
    )
    tag_one = f"sweb.{stage}.x86_64.{instance.lower()}:run-1"
    tag_two = f"sweb.{stage}.x86_64.{instance.lower()}:run-2"
    args = [("SOURCE_DATE_EPOCH", source_date_epoch)]
    first = docker_build(build_ctx, "Dockerfile", tag_one, args, "none")
    if not verify_double_build:
        return first, tag_one
    second = docker_build(build_ctx, "Dockerfile", tag_two, args, "none")
    if first != second:
        raise SystemExit(
            f"{stage} canonical build is not deterministic: {first} != {second}"
        )
    return first, tag_two


CONDA_RETRY_HELPER = """
conda_retry() {
  local attempt
  for attempt in 1 2 3 4 5; do
    if "$@"; then
      return 0
    fi
    echo "conda command failed (attempt ${attempt}); retrying" >&2
    sleep $((attempt * 15))
  done
  return 1
}
""".strip()


def render_seed_env_script(env_script_list: list[str], env_name: str) -> str:
    body = "\n".join(env_script_list).replace("conda create ", "conda_retry conda create ")
    return (
        "\n".join(
            [
                "#!/bin/bash",
                "set -euxo pipefail",
                "source /opt/miniconda3/bin/activate",
                CONDA_RETRY_HELPER,
                body,
                f"conda activate {env_name}",
            ]
        )
        + "\n"
    )


def render_pinned_env_script(env_name: str) -> str:
    return (
        "\n".join(
            [
                "#!/bin/bash",
                "set -euxo pipefail",
                "source /opt/miniconda3/bin/activate",
                CONDA_RETRY_HELPER,
                "sed -i -e 's/\\r$//' /root/env.spec.txt",
                f"conda_retry conda create -n {env_name} --file /root/env.spec.txt -y",
                f"conda activate {env_name}",
            ]
        )
        + "\n"
    )


def split_repo_script(repo_script_list: list[str]) -> tuple[list[str], list[str]]:
    for index, line in enumerate(repo_script_list):
        if "git config --global user.email" in line:
            return repo_script_list[:index], repo_script_list[index:]
    return list(repo_script_list), []


def canonical_repo_pre(base_commit: str) -> list[str]:
    return [
        "git init -q /testbed",
        "git -C /testbed fetch -q /root/testbed.bundle bundle-tip:refs/heads/base",
        "git -C /testbed checkout -q base",
        "cd /testbed",
        "TARGET_TIMESTAMP=$(git show -s --format=%ci " + base_commit + ")",
        'export GIT_AUTHOR_DATE="$TARGET_TIMESTAMP"',
        'export GIT_COMMITTER_DATE="$TARGET_TIMESTAMP"',
        'git tag -l | while read tag; do TAG_COMMIT=$(git rev-list -n 1 "$tag"); TAG_TIME=$(git show -s --format=%ci "$TAG_COMMIT"); if [[ "$TAG_TIME" > "$TARGET_TIMESTAMP" ]]; then git tag -d "$tag"; fi; done',
        "git reflog expire --expire=now --all",
        "git gc --prune=now --aggressive",
        "AFTER_TIMESTAMP=$(date -d \"$TARGET_TIMESTAMP + 1 second\" '+%Y-%m-%d %H:%M:%S')",
        'COMMIT_COUNT=$(git log --oneline --all --since="$AFTER_TIMESTAMP" | wc -l)',
        '[ "$COMMIT_COUNT" -eq 0 ] || exit 1',
    ]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", required=True, type=Path)
    parser.add_argument("--selection", required=True, type=Path)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--apt-snapshot", default=None)
    parser.add_argument("--tag-instance", action="store_true")
    parser.add_argument(
        "--export-archive",
        action="store_true",
        help="write a deterministic .image.tar.gz next to the lock after tagging",
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--create-lock", action="store_true")
    mode.add_argument("--replay-lock", type=Path)
    parser.add_argument(
        "--developer-skip-double-build",
        action="store_true",
        help="diagnostics only; forbidden with --tag-instance",
    )
    parser.add_argument(
        "--resume-after-base",
        action="store_true",
        help="resume a create-lock run from an already verified base.rootfs.tar",
    )
    parser.add_argument(
        "--resume-after-env",
        action="store_true",
        help="developer diagnostics: resume from verified base/env rootfs artifacts",
    )
    args = parser.parse_args()

    if args.developer_skip_double_build and args.tag_instance:
        raise SystemExit(
            "cannot tag a canonical instance without double-build verification"
        )
    if args.tag_instance and (args.resume_after_base or args.resume_after_env):
        raise SystemExit("cannot tag a canonical instance from a resumed build")
    if args.replay_lock and args.apt_snapshot:
        raise SystemExit("--apt-snapshot cannot override a replayed lock")
    if args.replay_lock and (args.resume_after_base or args.resume_after_env):
        raise SystemExit("resume options are only valid with --create-lock")

    selection = json.loads(args.selection.read_text(encoding="utf-8"))
    if sha256(args.dataset) != selection["datasetSha256"]:
        raise SystemExit("parquet hash does not match the pinned selection")
    if args.instance not in selection["instances"]:
        raise SystemExit(f"{args.instance} is not pinned in the selection")

    frame = pd.read_parquet(args.dataset).set_index("instance_id", drop=False)
    record = frame.loc[args.instance].to_dict()
    spec = load_test_spec(record)
    base_commit = str(record["base_commit"])
    env_name = "testbed"
    repo = str(record["repo"])

    instance_dir = args.artifacts / args.instance
    ctx = instance_dir / "ctx"
    locks = instance_dir / "locks"
    ctx.mkdir(parents=True, exist_ok=True)
    locks.mkdir(parents=True, exist_ok=True)

    replay: dict[str, object] | None = None
    if args.replay_lock:
        replay = json.loads(args.replay_lock.read_text(encoding="utf-8"))
        if replay.get("instance") != args.instance:
            raise SystemExit("replay lock instance does not match --instance")
        if replay.get("baseCommit") != base_commit:
            raise SystemExit("replay lock base commit does not match the dataset")
        apt_snapshot = str(replay["aptSnapshot"])
    else:
        apt_snapshot = args.apt_snapshot or resolve_apt_snapshot(
            _dt.datetime.now(_dt.timezone.utc)
        )

    bundle_path = ctx / "testbed.bundle"
    if replay is not None:
        source_date_epoch = str(replay["sourceDateEpoch"])
        if not bundle_path.is_file():
            raise SystemExit("replay requires the locked testbed.bundle")
        if sha256(bundle_path) != replay.get("repoBundleSha256"):
            raise SystemExit("replay testbed.bundle hash mismatch")
    else:
        mirror = instance_dir / "mirror"
        is_bare = (
            mirror.exists()
            and run("git", "-C", str(mirror), "rev-parse", "--is-bare-repository")
            == "true"
        )
        if not is_bare:
            run("git", "clone", "--mirror", f"https://github.com/{repo}", str(mirror))
        source_date_epoch = run(
            "git", "-C", str(mirror), "show", "-s", "--format=%ct", base_commit
        )
        run("git", "-C", str(mirror), "branch", "-f", "bundle-tip", base_commit)
        run(
            "git",
            "-C",
            str(mirror),
            "bundle",
            "create",
            str(bundle_path.resolve()),
            "refs/heads/bundle-tip",
        )

    env_pinned = (ctx / "env.spec.txt").is_file()
    if replay is not None:
        env_pinned = bool(replay.get("envPinned"))

    eval_script = spec.eval_script.replace("\r\n", "\n").replace("\r", "\n")
    eval_script = eval_script.replace(
        "python -m pip install .", "python -m pip install --no-build-isolation ."
    )
    (ctx / "eval.sh").write_bytes(eval_script.encode("utf-8"))
    if env_pinned:
        (ctx / "seed_env.sh").write_text(
            render_pinned_env_script(env_name), encoding="utf-8"
        )
    else:
        (ctx / "seed_env.sh").write_text(
            render_seed_env_script(spec.env_script_list, env_name), encoding="utf-8"
        )

    repo_pre, repo_post = split_repo_script(spec.repo_script_list)
    seed_repo_body = "\n".join(
        repo_pre
        + [
            "python -m pip install . --report /root/pip-report.json > /root/pip-install.log 2>&1 || { cat /root/pip-install.log; exit 1; }",
            "python - <<'LOCKEOF'",
            "import json",
            "rows = []",
            "for item in json.load(open('/root/pip-report.json'))['install']:",
            "    m = item.get('metadata', {})",
            "    hash_value = (item.get('download_info', {}).get('archive_info', {}) or {}).get('hash', '')",
            "    if hash_value:",
            "        rows.append('%s==%s --hash=%s # %s' % (m.get('name'), m.get('version'), hash_value, item.get('download_info', {}).get('url', '')))",
            "open('/root/requirements.lock.txt', 'w').write('\\n'.join(sorted(rows)) + '\\n')",
            "LOCKEOF",
            "python -m pip download --only-binary=:all: --no-deps --require-hashes -r /root/requirements.lock.txt -d /root/wheelhouse > /root/pip-download.log 2>&1 || { cat /root/pip-download.log; exit 1; }",
        ]
        + repo_post
    )
    (ctx / "seed_repo.sh").write_text(
        "#!/bin/bash\nset -uxo pipefail\n"
        + "source /opt/miniconda3/bin/activate\n"
        + f"conda activate {env_name}\n"
        + seed_repo_body
        + "\n",
        encoding="utf-8",
    )

    canonical_pre = canonical_repo_pre(base_commit)
    canonical_repo_body = "\n".join(
        canonical_pre
        + [
            "source /opt/miniconda3/bin/activate",
            f"conda activate {env_name}",
            "python -m pip install --require-hashes --only-binary=:all: --no-index "
            "--find-links /root/wheelhouse -r /root/requirements.lock.txt",
            "cd /testbed",
            "python -m pip install --no-deps --no-build-isolation --no-index "
            "--find-links /root/wheelhouse .",
        ]
        + repo_post
    )
    (ctx / "seed_repo_canonical.sh").write_text(
        "#!/bin/bash\nset -uxo pipefail\n"
        + "source /opt/miniconda3/bin/activate\n"
        + f"conda activate {env_name}\n"
        + canonical_repo_body
        + "\n",
        encoding="utf-8",
    )

    (ctx / "base.Dockerfile").write_text(
        BASE_DOCKERFILE.format(
            base_image=BASE_IMAGE,
            apt_snapshot=f"https://snapshot.ubuntu.com/ubuntu/{apt_snapshot}",
        ),
        encoding="utf-8",
    )
    (ctx / "env.seed.Dockerfile").write_text(
        ENV_SEED_DOCKERFILE.replace(
            "{env_spec_copy}",
            "COPY env.spec.txt /root/env.spec.txt\n" if env_pinned else "",
        ),
        encoding="utf-8",
    )
    (ctx / "instance.seed.Dockerfile").write_text(
        INSTANCE_SEED_DOCKERFILE, encoding="utf-8"
    )
    (ctx / "instance.canonical.Dockerfile").write_text(
        INSTANCE_CANONICAL_DOCKERFILE, encoding="utf-8"
    )

    context_files = [
        ctx / "base.Dockerfile",
        ctx / "env.seed.Dockerfile",
        ctx / "instance.seed.Dockerfile",
        ctx / "instance.canonical.Dockerfile",
        ctx / "seed_env.sh",
        ctx / "seed_repo_canonical.sh",
        ctx / "eval.sh",
    ]
    if replay is not None:
        if sha256(Path(__file__).resolve()) != replay.get("builderScriptSha256"):
            raise SystemExit("replay builder script hash mismatch")
        if combined_sha256(context_files) != replay.get("contextSha256"):
            raise SystemExit("replay generated context hash mismatch")
        locked_ubuntu = replay.get("ubuntu", {})
        if (
            not isinstance(locked_ubuntu, dict)
            or locked_ubuntu.get("sourceRef") != BASE_IMAGE
        ):
            raise SystemExit("replay Ubuntu source reference mismatch")

    verify_double = not args.developer_skip_double_build
    canonical_path = "/opt/miniconda3/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

    if replay is not None:
        for stage in ("base", "env", "eval"):
            archive = ctx / f"{stage}.rootfs.tar"
            expected = replay.get("rootfs", {}).get(stage, {}).get("sha256")  # type: ignore[union-attr]
            if not archive.is_file() or sha256(archive) != expected:
                raise SystemExit(f"replay {stage} rootfs archive hash mismatch")
        base_id, base_tag = build_canonical_rootfs(
            ctx,
            ctx / "base.rootfs.tar",
            "base",
            args.instance,
            base_commit,
            source_date_epoch,
            "",
            "/",
            verify_double,
        )
        env_canon_id, env_canonical_tag = build_canonical_rootfs(
            ctx,
            ctx / "env.rootfs.tar",
            "env",
            args.instance,
            base_commit,
            source_date_epoch,
            f"PATH={canonical_path}",
            "/testbed",
            verify_double,
        )
        instance_id, instance_tag = build_canonical_rootfs(
            ctx,
            ctx / "eval.rootfs.tar",
            "eval",
            args.instance,
            base_commit,
            source_date_epoch,
            f"PATH={canonical_path}",
            "/testbed",
            verify_double,
        )
    else:
        ensure_miniconda(ctx)
        if args.resume_after_base or args.resume_after_env:
            base_archive = ctx / "base.rootfs.tar"
            if (
                not base_archive.is_file()
                or not (ctx / "base.rootfs-manifest.json").is_file()
            ):
                raise SystemExit(
                    "--resume-after-base requires the verified base artifacts"
                )
            base_id, base_tag = build_canonical_rootfs(
                ctx,
                base_archive,
                "base",
                args.instance,
                base_commit,
                source_date_epoch,
                "",
                "/",
                verify_double,
            )
        else:
            base_seed_tag = f"sweb.base.x86_64.{args.instance.lower()}:seed"
            docker_build(
                ctx,
                "base.Dockerfile",
                base_seed_tag,
                [
                    ("SOURCE_DATE_EPOCH", source_date_epoch),
                    ("APT_SNAPSHOT", apt_snapshot),
                ],
                "default",
            )
            base_id, base_tag, _, _ = canonicalize_seed(
                ctx,
                base_seed_tag,
                "base",
                args.instance,
                base_commit,
                source_date_epoch,
                "",
                "/",
                verify_double,
            )
        print("base canonical verified:", base_id)

        if args.resume_after_env:
            env_archive = ctx / "env.rootfs.tar"
            required = (
                env_archive,
                ctx / "env.rootfs-manifest.json",
                ctx / "env.spec.txt",
            )
            if not all(path.is_file() for path in required):
                raise SystemExit("--resume-after-env requires verified env artifacts")
            env_canon_id, env_canonical_tag = build_canonical_rootfs(
                ctx,
                env_archive,
                "env",
                args.instance,
                base_commit,
                source_date_epoch,
                f"PATH={canonical_path}",
                "/testbed",
                verify_double,
            )
        else:
            env_seed_tag = f"sweb.env.x86_64.{args.instance.lower()}:seed"
            docker_build(
                ctx,
                "env.seed.Dockerfile",
                env_seed_tag,
                [
                    ("SOURCE_DATE_EPOCH", source_date_epoch),
                    ("MINICONDA_SHA256", MINICONDA_SHA256),
                    ("BASE_IMAGE_REF", base_tag),
                ],
                "default",
            )
            explicit = run(
                "docker",
                "run",
                "--rm",
                env_seed_tag,
                "bash",
                "-lc",
                "source /opt/miniconda3/bin/activate && conda list --explicit --md5 -n testbed",
            )
            (ctx / "env.spec.txt").write_text(explicit + "\n", encoding="utf-8")
            env_canon_id, env_canonical_tag, _, _ = canonicalize_seed(
                ctx,
                env_seed_tag,
                "env",
                args.instance,
                base_commit,
                source_date_epoch,
                f"PATH={canonical_path}",
                "/testbed",
                verify_double,
            )
        print("env canonical verified:", env_canon_id)

        instance_seed_tag = f"sweb.eval.x86_64.{args.instance.lower()}:seed"
        docker_build(
            ctx,
            "instance.seed.Dockerfile",
            instance_seed_tag,
            [
                ("ENV_IMAGE_REF", env_canonical_tag),
                ("SOURCE_DATE_EPOCH", source_date_epoch),
            ],
            "default",
        )
        keep = run(
            "docker",
            "run",
            "--rm",
            instance_seed_tag,
            "bash",
            "-lc",
            "cat /root/requirements.lock.txt",
        )
        (ctx / "requirements.lock.txt").write_text(keep + "\n", encoding="utf-8")
        run_out(
            [
                "docker",
                "run",
                "--rm",
                instance_seed_tag,
                "bash",
                "-lc",
                "tar --sort=name --mtime=@0 --numeric-owner --owner=0 --group=0 "
                "-C /root -cf - wheelhouse",
            ],
            ctx / "wheelhouse.tar",
        )
        (ctx / "seed_repo.sh").write_bytes(
            (ctx / "seed_repo_canonical.sh").read_bytes()
        )
        locked_seed_tag = f"sweb.eval.x86_64.{args.instance.lower()}:locked-seed"
        docker_build(
            ctx,
            "instance.canonical.Dockerfile",
            locked_seed_tag,
            [
                ("ENV_IMAGE_REF", env_canonical_tag),
                ("SOURCE_DATE_EPOCH", source_date_epoch),
                ("INSTANCE_ID", args.instance),
                ("BASE_COMMIT", base_commit),
            ],
            "none",
        )
        instance_id, instance_tag, _, _ = canonicalize_seed(
            ctx,
            locked_seed_tag,
            "eval",
            args.instance,
            base_commit,
            source_date_epoch,
            f"PATH={canonical_path}",
            "/testbed",
            verify_double,
        )
        print("instance canonical verified:", instance_id)

    instance_config = image_config_digest(instance_tag)
    export_tag = archive_local_tag(args.instance, instance_config)
    run("docker", "tag", instance_tag, export_tag)
    print("tagged", export_tag)

    if args.tag_instance:
        official = f"sweb.eval.x86_64.{args.instance.lower()}:latest"
        run("docker", "tag", instance_tag, official)
        print("tagged", official)

    if args.export_archive:
        if not args.tag_instance:
            raise SystemExit("--export-archive requires --tag-instance")
        archive_path = instance_dir / f"{args.instance}.image.tar.gz"
        facts = export_image_archive_twice(export_tag, archive_path)
        manifest = {
            "instance": args.instance,
            "tag": export_tag,
            "imageDigest": instance_config,
            "archive": {
                "path": archive_path.name,
                **facts,
            },
        }
        (instance_dir / f"{args.instance}.artifact.json").write_text(
            json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
        )
        print("exported", archive_path, facts["sha256"])

    if replay is not None:
        expected_configs = (
            replay.get("baseImageConfigDigest"),
            replay.get("envImageConfigDigest"),
            replay.get("instanceImageConfigDigest"),
        )
        actual_configs = tuple(
            image_config_digest(image)
            for image in (base_tag, env_canonical_tag, instance_tag)
        )
        if actual_configs != expected_configs:
            raise SystemExit("replayed image config digests do not match the lock")
        print("replay lock verified:", args.replay_lock)
        return

    lock = {
        "schemaVersion": SCHEMA_VERSION,
        "instance": args.instance,
        "repo": repo,
        "baseCommit": base_commit,
        "sourceDateEpoch": int(source_date_epoch),
        "ubuntu": {
            "sourceRef": BASE_IMAGE,
            "digest": BASE_IMAGE.split("@")[1],
            "configDigest": run(
                "docker", "image", "inspect", "--format", "{{.Id}}", BASE_IMAGE
            ),
        },
        "miniconda": {"url": MINICONDA_URL, "sha256": MINICONDA_SHA256},
        "aptSnapshot": apt_snapshot,
        "envPinned": env_pinned,
        "env": {
            "specSha256": sha256(ctx / "env.spec.txt"),
        },
        "pip": {
            "lockSha256": sha256(ctx / "requirements.lock.txt"),
            "wheelhouseTarSha256": sha256(ctx / "wheelhouse.tar"),
        },
        "repoBundleSha256": sha256(bundle_path),
        "evalScriptSha256": sha256(ctx / "eval.sh"),
        "builderScriptSha256": sha256(Path(__file__).resolve()),
        "contextSha256": combined_sha256(context_files),
        "docker": {
            "engine": run("docker", "version", "--format", "{{.Server.Version}}"),
            "buildx": run("docker", "buildx", "version"),
            "platform": "linux/amd64",
        },
        "rootfs": {
            stage: {
                "sha256": sha256(ctx / f"{stage}.rootfs.tar"),
                "manifestSha256": sha256(ctx / f"{stage}.rootfs-manifest.json"),
            }
            for stage in ("base", "env", "eval")
        },
        "baseImageId": base_id,
        "baseImageConfigDigest": image_config_digest(base_tag),
        "envImageId": env_canon_id,
        "envImageConfigDigest": image_config_digest(env_canonical_tag),
        "instanceImageId": instance_id,
        "instanceImageConfigDigest": image_config_digest(instance_tag),
        "doubleBuildVerified": verify_double,
    }
    lock_path = locks / "grader-lock.json"
    lock_path.write_text(json.dumps(lock, indent=2) + "\n", encoding="utf-8")
    print("wrote", lock_path)
    print("base image      ", base_id)
    print("env image       ", env_canon_id)
    print("instance image  ", instance_id)


if __name__ == "__main__":
    main()
