#!/usr/bin/env python3
"""Build the pinned SWE-bench Verified-12 bundles and suite manifest.

Requires:
  pip install swebench pandas pyarrow docker datasets

The official SWE-bench package builds each instance image. This script copies
only /testbed into the agent bundle, keeps the official eval script under
hidden/, records the local image digest, and emits a Vellum suite manifest.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import types

import pandas as pd


def run(*args: str) -> str:
    completed = subprocess.run(
        list(args),
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return completed.stdout.strip()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def image_config_digest(image: str) -> str:
    """Return the deterministic config blob digest for a local image.

    ``docker image inspect {{.Id}}`` returns the manifest digest, whose media
    type differs between the embedded ``docker`` driver and a
    ``docker-container`` builder. The config blob is identical in both, so it
    is the only driver-independent pin.
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


def image_key(instance_id: str) -> str:
    return f"sweb.eval.x86_64.{instance_id.lower()}:latest"


def image_manifest_digest(image: str) -> str:
    """Return the OCI manifest digest pinned in `graderImage`.

    This is distinct from the Docker config digest stored as `image_digest`.
    A clean host pulls the digest-pinned registry reference and then checks
    both identities.
    """
    descriptor = run(
        "docker",
        "image",
        "inspect",
        "--format",
        "{{.Descriptor.Digest}}",
        image,
    )
    if descriptor.startswith("sha256:") and len(descriptor) == 71:
        return descriptor
    image_id = run("docker", "image", "inspect", "--format", "{{.Id}}", image)
    if image_id.startswith("sha256:") and len(image_id) == 71:
        return image_id
    raise SystemExit(f"cannot resolve OCI manifest digest for {image}")


def grader_oci_reference(registry: str, instance_id: str, manifest_digest: str) -> str:
    slug = instance_id.lower().replace("__", "-")
    return f"{registry.rstrip('/')}/swe-grader-{slug}@{manifest_digest}"


def safe_tag(value: object) -> str:
    normalized = re.sub(r"[^a-z0-9._-]+", "-", str(value).strip().lower())
    return normalized.strip("-") or "unknown"


def copy_testbed(image: str, destination: Path) -> None:
    # Canonical grader images intentionally have no default command. `create`
    # still needs an explicit command even though the container is never run.
    container = run("docker", "create", image, "/bin/true")
    try:
        destination.mkdir(parents=True, exist_ok=True)
        run("docker", "cp", f"{container}:/testbed/.", str(destination))
    finally:
        subprocess.run(["docker", "rm", "-f", container], check=False)


def excluded_bundle_path(relative: Path) -> bool:
    """Reject host/runtime state that is not part of the pinned testbed."""
    parts = relative.parts
    if "__pycache__" in parts:
        return True
    if any(
        part in {".cache", ".mypy_cache", ".pytest_cache", ".ruff_cache"}
        for part in parts
    ):
        return True
    if len(parts) >= 3 and parts[0] == "workspace" and parts[1:3] == (".git", "logs"):
        return True
    return relative.name in {"index.lock", "gc.log"} or relative.name.endswith(
        (".tmp", ".swp", "~")
    )


def write_deterministic_bundle(
    root: Path, destination: Path, source_date_epoch: int
) -> list[dict[str, object]]:
    """Archive a prepared task with stable ordering and metadata."""
    entries: list[dict[str, object]] = []
    paths: list[tuple[Path, Path]] = []
    for name in ("workspace", "hidden", "prompt.txt"):
        source = root / name
        paths.append((source, Path(name)))
        if source.is_dir():
            paths.extend(
                (child, Path(name) / child.relative_to(source))
                for child in source.rglob("*")
            )

    with tarfile.open(destination, "w", format=tarfile.PAX_FORMAT) as output:
        for source, relative in sorted(paths, key=lambda item: item[1].as_posix()):
            if excluded_bundle_path(relative):
                continue
            info = output.gettarinfo(str(source), arcname=relative.as_posix())
            info.uid = 0
            info.gid = 0
            info.uname = ""
            info.gname = ""
            info.mtime = source_date_epoch
            info.pax_headers = {}
            if info.isdir():
                info.mode = 0o755
            elif info.issym():
                info.mode = 0o777
            else:
                info.mode = 0o755 if info.mode & 0o111 else 0o644

            row: dict[str, object] = {
                "path": relative.as_posix(),
                "type": info.type.decode("ascii", errors="replace"),
                "mode": info.mode,
                "uid": info.uid,
                "gid": info.gid,
                "mtime": info.mtime,
                "size": info.size,
            }
            if info.linkname:
                row["link"] = info.linkname
            if info.isfile():
                row["sha256"] = sha256(source)
                with source.open("rb") as stream:
                    output.addfile(info, stream)
            else:
                output.addfile(info)
            entries.append(row)
    return entries


def build_bundle_once(
    image: str, record: dict, destination: Path, source_date_epoch: int
) -> list[dict[str, object]]:
    with tempfile.TemporaryDirectory(prefix="vellum-swe-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        hidden = root / "hidden"
        copy_testbed(image, workspace)
        hidden.mkdir()
        eval_script = load_eval_script(record).replace("\r\n", "\n").replace("\r", "\n")
        eval_script = eval_script.replace(
            "python -m pip install .",
            "python -m pip install --no-build-isolation .",
        )
        (hidden / "eval.sh").write_bytes(eval_script.encode("utf-8"))
        (root / "prompt.txt").write_text(
            str(record["problem_statement"]), encoding="utf-8", newline="\n"
        )
        return write_deterministic_bundle(root, destination, source_date_epoch)


def load_eval_script(record: dict) -> str:
    if os.name == "nt" and "resource" not in sys.modules:
        resource = types.ModuleType("resource")
        resource.RLIMIT_NOFILE = 7
        resource.setrlimit = lambda *_args, **_kwargs: None
        sys.modules["resource"] = resource
    from swebench.harness.test_spec.test_spec import make_test_spec

    return make_test_spec(record).eval_script


def prepare_images(selection: dict, instances: list[str], max_workers: int) -> None:
    command = [
        "python",
        "-m",
        "swebench.harness.prepare_images",
        "--dataset_name",
        selection["dataset"],
        "--split",
        "test",
        "--instance_ids",
        *instances,
        "--max_workers",
        str(max_workers),
        # swebench 4.1's CLI default is None even though make_test_spec requires
        # a concrete tag. Pin the documented default explicitly.
        "--tag",
        "latest",
        "--env_image_tag",
        "latest",
    ]
    if os.name != "nt":
        subprocess.run(command, check=True)
        return

    # SWE-bench imports POSIX `resource` only to raise RLIMIT_NOFILE. Docker
    # Desktop does not need that operation, but the unconditional import makes
    # the otherwise portable image builder fail on Windows. Supply a process-
    # local compatibility module; do not patch the installed package.
    with tempfile.TemporaryDirectory(prefix="vellum-swe-resource-") as temporary:
        compatibility = Path(temporary) / "resource.py"
        compatibility.write_text(
            "RLIMIT_NOFILE = 7\ndef setrlimit(*_args, **_kwargs):\n    return None\n",
            encoding="utf-8",
        )
        env = os.environ.copy()
        current_python_path = env.get("PYTHONPATH")
        env["PYTHONPATH"] = (
            f"{temporary}{os.pathsep}{current_python_path}"
            if current_python_path
            else temporary
        )
        subprocess.run(command, check=True, env=env)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--selection", required=True, type=Path)
    parser.add_argument("--dataset", required=True, type=Path)
    parser.add_argument("--bundle-root", required=True, type=Path)
    parser.add_argument("--suite-output", required=True, type=Path)
    parser.add_argument(
        "--bundle-base-url",
        required=True,
        help="HTTPS artifact prefix used on other machines; bundles are still written locally.",
    )
    parser.add_argument(
        "--grader-registry",
        default="ghcr.io/910488/vellum",
        help="Registry prefix for digest-pinned OCI graderImage references.",
    )
    parser.add_argument(
        "--image-archive-base-url",
        default=None,
        help="HTTPS prefix for *.image.tar.gz GitHub Release assets. When set, the suite uses archive mode instead of OCI.",
    )
    parser.add_argument(
        "--image-archive-root",
        type=Path,
        default=None,
        help="Directory containing <instance>.image.tar.gz and <instance>.artifact.json from the builder.",
    )
    parser.add_argument("--max-workers", type=int, default=2)
    parser.add_argument(
        "--reuse-existing-images",
        action="store_true",
        help="do not invoke the upstream image builder; require every local tag to exist",
    )
    parser.add_argument(
        "--instance-id",
        action="append",
        dest="instance_ids",
        help=(
            "Prepare only this pinned instance. Repeat the option to select more than "
            "one. Every requested ID must already be present in the selection file."
        ),
    )
    args = parser.parse_args()

    selection = json.loads(args.selection.read_text(encoding="utf-8"))
    if sha256(args.dataset) != selection["datasetSha256"]:
        raise SystemExit("SWE-bench parquet hash does not match the pinned selection")
    if not args.bundle_base_url.startswith("https://"):
        raise SystemExit("--bundle-base-url must be HTTPS")
    if args.image_archive_base_url and not args.image_archive_base_url.startswith(
        "https://"
    ):
        raise SystemExit("--image-archive-base-url must be HTTPS")
    if bool(args.image_archive_base_url) != bool(args.image_archive_root):
        raise SystemExit(
            "--image-archive-base-url and --image-archive-root must be provided together"
        )

    pinned_instances = selection["instances"]
    instances = args.instance_ids or pinned_instances
    unknown_instances = sorted(set(instances) - set(pinned_instances))
    if unknown_instances:
        raise SystemExit(
            "requested SWE-bench instance is not pinned: "
            + ", ".join(unknown_instances)
        )
    if args.reuse_existing_images:
        for instance_id in instances:
            run("docker", "image", "inspect", image_key(instance_id))
    else:
        prepare_images(selection, instances, args.max_workers)

    frame = pd.read_parquet(args.dataset).set_index("instance_id", drop=False)
    tasks = []
    for instance_id in instances:
        record = frame.loc[instance_id].to_dict()
        image = image_key(instance_id)
        digest = image_config_digest(image)
        if not digest.startswith("sha256:"):
            raise SystemExit(f"cannot resolve config digest for {image}")
        if args.image_archive_base_url:
            artifact_path = args.image_archive_root / f"{instance_id}.artifact.json"
            archive_path = args.image_archive_root / f"{instance_id}.image.tar.gz"
            if not artifact_path.is_file():
                artifact_path = (
                    args.image_archive_root / instance_id / f"{instance_id}.artifact.json"
                )
                archive_path = (
                    args.image_archive_root / instance_id / f"{instance_id}.image.tar.gz"
                )
            if not artifact_path.is_file() or not archive_path.is_file():
                raise SystemExit(f"missing image archive artifacts for {instance_id}")
            artifact = json.loads(artifact_path.read_text(encoding="utf-8"))
            if artifact.get("imageDigest") != digest:
                raise SystemExit(
                    f"archive config digest for {instance_id} does not match the local image"
                )
            if sha256(archive_path) != artifact["archive"]["sha256"]:
                raise SystemExit(f"archive hash mismatch for {instance_id}")
            grader_image = artifact["tag"]
            grader_archive = {
                "url": f"{args.image_archive_base_url.rstrip('/')}/{instance_id}.image.tar.gz",
                "sha256": artifact["archive"]["sha256"],
                "compression": "gzip",
                "sizeBytes": artifact["archive"]["sizeBytes"],
                "uncompressedSizeBytes": artifact["archive"]["uncompressedSizeBytes"],
            }
        else:
            manifest_digest = image_manifest_digest(image)
            grader_image = grader_oci_reference(
                args.grader_registry, instance_id, manifest_digest
            )
            grader_archive = None

        archive = args.bundle_root / selection["datasetRevision"] / f"{instance_id}.tar"
        archive.parent.mkdir(parents=True, exist_ok=True)
        source_date_epoch = int(
            run(
                "docker",
                "run",
                "--rm",
                "--network",
                "none",
                image,
                "git",
                "-C",
                "/testbed",
                "show",
                "-s",
                "--format=%ct",
                str(record["base_commit"]),
            )
        )
        first = archive.with_suffix(".run-1.tar")
        second = archive.with_suffix(".run-2.tar")
        first_manifest = build_bundle_once(image, record, first, source_date_epoch)
        second_manifest = build_bundle_once(image, record, second, source_date_epoch)
        if sha256(first) != sha256(second) or first_manifest != second_manifest:
            raise SystemExit(f"bundle is not deterministic for {instance_id}")
        first.replace(archive)
        second.unlink()
        manifest_path = archive.with_suffix(".manifest.json")
        manifest_path.write_text(
            json.dumps(first_manifest, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
            newline="\n",
        )

        bundle_hash = sha256(archive)
        repo = str(record["repo"])
        task = {
            "id": f"swe-{instance_id.lower()}",
            "category": "swe-bench",
            "tags": ["swe-bench", safe_tag(record.get("difficulty", "unknown"))],
            "source": {
                "kind": "swe_bench",
                "instance_id": instance_id,
                "dataset_revision": selection["datasetRevision"],
                "repository": f"https://github.com/{repo}.git",
                "commit": str(record["base_commit"]),
                "image_digest": digest,
                "bundle_sha256": bundle_hash,
                "bundle_url": f"{args.bundle_base_url.rstrip('/')}/{instance_id}.tar",
            },
            "prompt": str(record["problem_statement"]),
            "verification": ["bash /hidden/eval.sh"],
            # Exactly one of: digest-pinned OCI reference, or a GitHub
            # Release image archive plus a :cfg-<digest> local tag.
            "graderImage": grader_image,
            "timeoutSeconds": 2700,
            "expectedHarness": {
                "minToolCalls": 2,
                "requireTerminalSse": True,
            },
            "limits": {
                "cpu": 2,
                "memoryMb": 8192,
                "pids": 512,
                "diskMb": 12288,
                "maxInputTokens": 400000,
                "maxOutputTokens": 60000,
            },
        }
        if grader_archive is not None:
            task["graderImageArchive"] = grader_archive
        tasks.append(task)

    suite = {
        "schemaVersion": 1,
        "name": "swe-verified-12"
        if instances == pinned_instances
        else "swe-terminal-gate",
        "version": selection["datasetRevision"],
        "defaultRepeat": 3,
        "tasks": tasks,
    }
    args.suite_output.parent.mkdir(parents=True, exist_ok=True)
    args.suite_output.write_text(
        json.dumps(suite, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"Wrote {args.suite_output} with {len(tasks)} pinned tasks")


if __name__ == "__main__":
    main()
