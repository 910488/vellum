#!/usr/bin/env python3
"""Sequential Verified-12 grader build, verify, and GitHub Release publish.

One instance at a time. Does not run `docker prune`. After each instance only
the named seed/run tags for that instance are removed.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

SEED_RUN_SUFFIXES = (
    "seed",
    "locked-seed",
    "run-1",
    "run-2",
)
SEED_RUN_PREFIXES = ("sweb.base.x86_64.", "sweb.env.x86_64.", "sweb.eval.x86_64.")


def run(args: list[str], cwd: Path | None = None, timeout: int | None = None) -> str:
    completed = subprocess.run(
        args,
        cwd=str(cwd) if cwd else None,
        check=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout,
    )
    if completed.stdout.strip():
        print(completed.stdout.rstrip())
    return completed.stdout


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def combined_sha256(paths: list[Path]) -> str:
    digest = hashlib.sha256()
    for path in paths:
        digest.update(path.name.encode("utf-8"))
        digest.update(b"\0")
        digest.update(bytes.fromhex(sha256_file(path)))
    return digest.hexdigest()


def load_progress(path: Path) -> dict:
    if path.is_file():
        return json.loads(path.read_text(encoding="utf-8"))
    return {"instances": {}}


def save_progress(path: Path, progress: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(progress, indent=2) + "\n", encoding="utf-8")


def instance_dir(artifacts: Path, instance: str) -> Path:
    return artifacts / instance


def export_tag_from_artifact(artifacts: Path, instance: str) -> str:
    manifest = json.loads(
        (instance_dir(artifacts, instance) / f"{instance}.artifact.json").read_text(
            encoding="utf-8"
        )
    )
    return str(manifest["tag"])


def remove_seed_run_tags(instance: str) -> None:
    slug = instance.lower()
    for prefix in SEED_RUN_PREFIXES:
        for suffix in SEED_RUN_SUFFIXES:
            tag = f"{prefix}{slug}:{suffix}"
            subprocess.run(
                ["docker", "rmi", tag],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )


def build_instance(
    tools: Path,
    dataset: Path,
    selection: Path,
    artifacts: Path,
    instance: str,
    apt_snapshot: str | None,
    replay_lock: Path | None,
) -> None:
    command = [
        sys.executable,
        str(tools / "build_swe_grader.py"),
        "--dataset",
        str(dataset),
        "--selection",
        str(selection),
        "--instance",
        instance,
        "--artifacts",
        str(artifacts),
        "--tag-instance",
        "--export-archive",
    ]
    if replay_lock is not None:
        command.extend(["--replay-lock", str(replay_lock)])
    else:
        command.append("--create-lock")
        if apt_snapshot:
            command.extend(["--apt-snapshot", apt_snapshot])
    print("RUN", " ".join(command), flush=True)
    run(command, timeout=None)


def prepare_bundle(
    tools: Path,
    dataset: Path,
    selection: Path,
    artifacts: Path,
    bundle_root: Path,
    instance: str,
    bundle_base_url: str,
) -> None:
    command = [
        sys.executable,
        str(tools / "prepare_swebench.py"),
        "--dataset",
        str(dataset),
        "--selection",
        str(selection),
        "--instance-id",
        instance,
        "--bundle-root",
        str(bundle_root),
        "--suite-output",
        str(instance_dir(artifacts, instance) / f"{instance}.partial-suite.json"),
        "--bundle-base-url",
        bundle_base_url,
        "--reuse-existing-images",
        "--image-archive-base-url",
        bundle_base_url,
        "--image-archive-root",
        instance_dir(artifacts, instance),
    ]
    print("RUN", " ".join(command), flush=True)
    run(command, timeout=None)


def verify_instance(tools: Path, dataset: Path, artifacts: Path, instance: str) -> None:
    image = export_tag_from_artifact(artifacts, instance)
    command = [
        sys.executable,
        str(tools / "verify_swe_grader.py"),
        "--dataset",
        str(dataset),
        "--instance",
        instance,
        "--image",
        image,
    ]
    print("RUN", " ".join(command), flush=True)
    run(command, timeout=2400)


def write_suite(
    tools: Path,
    dataset: Path,
    selection: Path,
    artifacts: Path,
    bundle_root: Path,
    suite_output: Path,
    bundle_base_url: str,
    instances: list[str],
) -> None:
    command = [
        sys.executable,
        str(tools / "prepare_swebench.py"),
        "--dataset",
        str(dataset),
        "--selection",
        str(selection),
        "--bundle-root",
        str(bundle_root),
        "--suite-output",
        str(suite_output),
        "--bundle-base-url",
        bundle_base_url,
        "--reuse-existing-images",
        "--image-archive-base-url",
        bundle_base_url,
        "--image-archive-root",
        artifacts,
    ]
    for instance in instances:
        command.extend(["--instance-id", instance])
    print("RUN", " ".join(command), flush=True)
    run(command, timeout=None)


def publish_release(
    artifacts: Path,
    bundle_root: Path,
    dataset_revision: str,
    instances: list[str],
    owner: str,
    repo: str,
) -> str:
    lock_paths = [
        instance_dir(artifacts, instance) / "locks" / "grader-lock.json"
        for instance in instances
    ]
    builder = Path(__file__).resolve().parent / "build_swe_grader.py"
    digest = combined_sha256([builder, *lock_paths])[:12]
    tag = f"swebench-verified12-{dataset_revision}-{digest}"
    notes = (
        "Deterministic SWE-bench Verified-12 grader archives and bundles.\n"
        f"Dataset revision {dataset_revision}. Tag is immutable; do not overwrite.\n"
        f"Builder+lock hash {digest}.\n"
    )
    existing = subprocess.run(
        ["gh", "release", "view", tag, "--repo", f"{owner}/{repo}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if existing.returncode == 0:
        raise SystemExit(f"release {tag} already exists; refusing to overwrite")
    run(
        [
            "gh",
            "release",
            "create",
            tag,
            "--repo",
            f"{owner}/{repo}",
            "--title",
            f"SWE-bench Verified-12 {dataset_revision}",
            "--notes",
            notes,
        ]
    )
    assets: list[str] = []
    for instance in instances:
        inst = instance_dir(artifacts, instance)
        assets.extend(
            [
                str(inst / f"{instance}.image.tar.gz"),
                str(inst / f"{instance}.artifact.json"),
                str(inst / "locks" / "grader-lock.json"),
            ]
        )
        bundle = bundle_root / dataset_revision / f"{instance}.tar"
        manifest = bundle_root / dataset_revision / f"{instance}.manifest.json"
        assets.extend([str(bundle), str(manifest)])
    for asset in assets:
        if not Path(asset).is_file():
            raise SystemExit(f"missing release asset {asset}")
        run(
            [
                "gh",
                "release",
                "upload",
                tag,
                asset,
                "--repo",
                f"{owner}/{repo}",
                "--clobber=false",
            ]
        )
    return tag


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset", required=True, type=Path)
    parser.add_argument("--selection", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--bundle-root", required=True, type=Path)
    parser.add_argument("--suite-output", required=True, type=Path)
    parser.add_argument(
        "--bundle-base-url",
        required=True,
        help="HTTPS prefix written into the suite; usually the immutable release download URL.",
    )
    parser.add_argument("--repo", default="910488/vellum")
    parser.add_argument("--start-from", default=None)
    parser.add_argument(
        "--publish",
        action="store_true",
        help="create the immutable GitHub Release after all 12 instances verify",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="do not invoke the image builder; require lock+archive+image to exist",
    )
    args = parser.parse_args()

    selection = json.loads(args.selection.read_text(encoding="utf-8"))
    if sha256_file(args.dataset) != selection["datasetSha256"]:
        raise SystemExit("parquet hash does not match the pinned selection")
    instances: list[str] = selection["instances"]
    if args.start_from:
        if args.start_from not in instances:
            raise SystemExit(f"--start-from {args.start_from} is not in the selection")
        instances = instances[instances.index(args.start_from) :]

    tools = Path(__file__).resolve().parent
    progress_path = args.artifacts / "verified-12-progress.json"
    progress = load_progress(progress_path)
    foundation_lock = (
        args.artifacts / "psf__requests-1142" / "locks" / "grader-lock.json"
    )
    apt_snapshot = None
    if foundation_lock.is_file():
        apt_snapshot = json.loads(foundation_lock.read_text(encoding="utf-8")).get(
            "aptSnapshot"
        )

    owner, repo = args.repo.split("/", 1)
    started = time.time()
    for instance in instances:
        state = progress["instances"].get(instance, {})
        if state.get("status") == "verified":
            print(f"SKIP already verified {instance}", flush=True)
            continue
        print(f"==== {instance} ====", flush=True)
        lock = instance_dir(args.artifacts, instance) / "locks" / "grader-lock.json"
        replay = lock if lock.is_file() and instance == "psf__requests-1142" else None
        if not args.skip_build:
            build_instance(
                tools,
                args.dataset,
                args.selection,
                args.artifacts,
                instance,
                apt_snapshot,
                replay,
            )
        prepare_bundle(
            tools,
            args.dataset,
            args.selection,
            args.artifacts,
            args.bundle_root,
            instance,
            args.bundle_base_url,
        )
        verify_instance(tools, args.dataset, args.artifacts, instance)
        remove_seed_run_tags(instance)
        progress["instances"][instance] = {
            "status": "verified",
            "artifact": sha256_file(
                instance_dir(args.artifacts, instance) / f"{instance}.image.tar.gz"
            ),
            "bundle": sha256_file(
                args.bundle_root
                / selection["datasetRevision"]
                / f"{instance}.tar"
            ),
        }
        save_progress(progress_path, progress)
        print(f"OK {instance} in {int(time.time() - started)}s", flush=True)

    if args.publish:
        placeholder = "https://github.com/910488/vellum/releases/download/pending"
        if args.bundle_base_url.rstrip("/") == placeholder.rstrip("/"):
            raise SystemExit("refusing to publish with a pending bundle URL")
        tag = publish_release(
            args.artifacts,
            args.bundle_root,
            selection["datasetRevision"],
            selection["instances"],
            owner,
            repo,
        )
        download = f"https://github.com/{owner}/{repo}/releases/download/{tag}"
        write_suite(
            tools,
            args.dataset,
            args.selection,
            args.artifacts,
            args.bundle_root,
            args.suite_output,
            download,
            selection["instances"],
        )
        print("published", tag)
    else:
        write_suite(
            tools,
            args.dataset,
            args.selection,
            args.artifacts,
            args.bundle_root,
            args.suite_output,
            args.bundle_base_url,
            selection["instances"],
        )


if __name__ == "__main__":
    main()
