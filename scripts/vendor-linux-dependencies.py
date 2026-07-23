#!/usr/bin/env python3
"""Create a checksum-preserving Cargo directory source for supported Linux targets."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
from pathlib import PurePosixPath
import shutil
import subprocess
import tarfile
import tomllib


ROOT = Path(__file__).resolve().parent.parent
VENDOR = Path(os.environ.get("BOXUP_VENDOR_DIR", ROOT / "vendor"))
CARGO_CONFIG = Path(
    os.environ.get("BOXUP_CARGO_CONFIG", ROOT / ".cargo" / "config.toml")
)
NOTICES = Path(
    os.environ.get("BOXUP_NOTICES", ROOT / "vendor" / "THIRD_PARTY_NOTICES")
)
TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu")
LEGAL_PREFIXES = ("COPYING", "COPYRIGHT", "LICENSE", "NOTICE", "UNLICENSE")


def metadata(target: str) -> dict[str, object]:
    environment = os.environ.copy()
    environment["CARGO_NET_OFFLINE"] = "true"
    result = subprocess.run(
        [
            "cargo",
            "metadata",
            "--offline",
            "--locked",
            "--filter-platform",
            target,
            "--format-version",
            "1",
        ],
        cwd=ROOT,
        env=environment,
        check=True,
        capture_output=True,
    )
    return json.loads(result.stdout)


def checksum(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_checksum(package: Path, package_checksum: str) -> None:
    files = {
        path.relative_to(package).as_posix(): checksum(path)
        for path in sorted(package.rglob("*"))
        if path.is_file() and path.name != ".cargo-checksum.json"
    }
    data = {"files": files, "package": package_checksum}
    (package / ".cargo-checksum.json").write_text(
        json.dumps(data, sort_keys=True, separators=(",", ":")) + "\n",
        encoding="ascii",
    )


def cached_archive(name: str, version: str, expected_checksum: str) -> Path:
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    candidates = sorted(
        (cargo_home / "registry" / "cache").glob(f"*/{name}-{version}.crate")
    )
    for candidate in candidates:
        if checksum(candidate) == expected_checksum:
            return candidate
    raise RuntimeError(
        f"no checksum-valid cached crate archive for {name} {version}; "
        "run cargo fetch --locked before vendoring"
    )


def extract_crate(archive: Path, destination: Path) -> None:
    expected_root = destination.name
    with tarfile.open(archive, "r:gz") as source:
        for member in source:
            relative = PurePosixPath(member.name)
            if (
                relative.is_absolute()
                or not relative.parts
                or relative.parts[0] != expected_root
                or any(part in ("", ".", "..") for part in relative.parts)
            ):
                raise RuntimeError(f"unsafe path in {archive}: {member.name}")
            # Dependency lockfiles are unused and often ignored by crate-local
            # rules, which would leave a future Git clone checksum-incomplete.
            if relative.name == "Cargo.lock":
                continue
            target = VENDOR.joinpath(*relative.parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
            elif member.isfile():
                target.parent.mkdir(parents=True, exist_ok=True)
                contents = source.extractfile(member)
                if contents is None:
                    raise RuntimeError(f"failed to read {member.name} from {archive}")
                with target.open("xb") as output:
                    shutil.copyfileobj(contents, output)
                target.chmod(member.mode & 0o777)
            else:
                raise RuntimeError(f"unsupported entry in {archive}: {member.name}")


def index_relative_path(name: str) -> Path:
    lowered = name.lower()
    if len(lowered) == 1:
        return Path("1") / lowered
    if len(lowered) == 2:
        return Path("2") / lowered
    if len(lowered) == 3:
        return Path("3") / lowered[0] / lowered
    return Path(lowered[:2]) / lowered[2:4] / lowered


def registry_entry(name: str, version: str) -> dict[str, object]:
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    relative = index_relative_path(name)
    candidates = sorted((cargo_home / "registry" / "index").glob(f"*/.cache/{relative}"))
    for candidate in candidates:
        for record in candidate.read_bytes().split(b"\0"):
            if not record.startswith(b"{"):
                continue
            entry = json.loads(record)
            if entry.get("name") == name and entry.get("vers") == version:
                return entry
    raise RuntimeError(f"no cached registry metadata for {name} {version}")


def archive_metadata(
    archive: Path, name: str, version: str
) -> tuple[dict[str, object], list[tuple[str, bytes]]]:
    root = f"{name}-{version}"
    manifest_name = f"{root}/Cargo.toml"
    with tarfile.open(archive, "r:gz") as source:
        manifest_member = source.getmember(manifest_name)
        manifest_file = source.extractfile(manifest_member)
        if manifest_file is None:
            raise RuntimeError(f"failed to read {manifest_name} from {archive}")
        manifest = tomllib.loads(manifest_file.read().decode("utf-8"))["package"]

        documents = []
        for member in sorted(source.getmembers(), key=lambda item: item.name):
            relative = PurePosixPath(member.name)
            if (
                not member.isfile()
                or len(relative.parts) < 2
                or relative.parts[0] != root
                or not relative.name.upper().startswith(LEGAL_PREFIXES)
            ):
                continue
            contents = source.extractfile(member)
            if contents is None:
                raise RuntimeError(f"failed to read {member.name} from {archive}")
            document_path = PurePosixPath(*relative.parts[1:]).as_posix()
            documents.append((document_path, contents.read()))
    return manifest, documents


def placeholder_manifest(
    name: str, version: str, package_metadata: dict[str, object]
) -> str:
    entry = registry_entry(name, version)
    license_expression = package_metadata.get("license")
    if not isinstance(license_expression, str) or not license_expression:
        raise RuntimeError(f"crate archive has no license expression for {name} {version}")
    lines = [
        "[package]",
        f"name = {json.dumps(name)}",
        f"version = {json.dumps(version)}",
        'edition = "2021"',
        f"license = {json.dumps(license_expression)}",
    ]
    if entry.get("links"):
        lines.append(f"links = {json.dumps(entry['links'])}")
    if entry.get("rust_version"):
        lines.append(f"rust-version = {json.dumps(entry['rust_version'])}")
    lines.extend(["", "[lib]", 'path = "src/lib.rs"'])

    features = dict(entry.get("features", {}))
    features.update(entry.get("features2", {}))
    if features:
        lines.extend(["", "[features]"])
        for feature, activations in sorted(features.items()):
            lines.append(f"{json.dumps(feature)} = {json.dumps(activations)}")

    section_names = {
        "normal": "dependencies",
        "build": "build-dependencies",
        "dev": "dev-dependencies",
    }
    for dependency in entry.get("deps", []):
        section = section_names[dependency.get("kind") or "normal"]
        target = dependency.get("target")
        prefix = f"target.{json.dumps(target)}." if target else ""
        alias = dependency["name"]
        lines.extend(["", f"[{prefix}{section}.{json.dumps(alias)}]"])
        lines.append(f"version = {json.dumps(dependency['req'])}")
        package = dependency.get("package")
        if package and package != alias:
            lines.append(f"package = {json.dumps(package)}")
        if dependency.get("features"):
            lines.append(f"features = {json.dumps(dependency['features'])}")
        if dependency.get("optional"):
            lines.append("optional = true")
        if not dependency.get("default_features", True):
            lines.append("default-features = false")

    return "\n".join(lines) + "\n"


def write_notices(
    packages: dict[
        tuple[str, str], tuple[dict[str, object], list[tuple[str, bytes]]]
    ],
    selected: set[tuple[str, str]],
) -> None:
    output = bytearray(
        b"BOXUP THIRD-PARTY NOTICES\n"
        b"===========================\n\n"
        b"This file is generated deterministically by "
        b"scripts/vendor-linux-dependencies.py from Cargo.lock and "
        b"checksum-verified crate archives. Do not edit it manually.\n\n"
        b"The source distribution includes the Cargo packages inventoried below "
        b"under vendor/. License expressions reproduce the package metadata in "
        b"the corresponding published crate archives. Legal documents supplied "
        b"by those archives are reproduced between file markers without "
        b"modification.\n\n"
        b"A resolution placeholder contains generated Cargo metadata and an empty "
        b"library, but no upstream source code. Such placeholders allow Cargo to "
        b"resolve locked packages excluded from Boxup's supported Linux target "
        b"graphs.\n"
    )
    for (name, version), (package_metadata, documents) in sorted(packages.items()):
        license_expression = package_metadata.get("license")
        if not isinstance(license_expression, str) or not license_expression:
            raise RuntimeError(
                f"crate archive has no license expression for {name} {version}"
            )
        content = (
            "full crate source"
            if (name, version) in selected
            else "resolution placeholder"
        )
        legal_files = ", ".join(filename for filename, _ in documents)
        if not legal_files:
            legal_files = "none in published crate archive"
        output.extend(
            (
                "\n\n"
                "========================================================================\n"
                f"Package: {name}\n"
                f"Version: {version}\n"
                f"Source: https://crates.io/crates/{name}/{version}\n"
                f"License expression: {license_expression}\n"
                f"Vendored content: {content}\n"
                f"Legal files: {legal_files}\n"
                "========================================================================\n"
            ).encode("utf-8")
        )
        for filename, document in documents:
            output.extend(f"\n----- BEGIN {filename} -----\n".encode("utf-8"))
            output.extend(document)
            if not document.endswith(b"\n"):
                output.extend(b"\n")
            output.extend(f"----- END {filename} -----\n".encode("utf-8"))

    NOTICES.parent.mkdir(parents=True, exist_ok=True)
    NOTICES.write_bytes(output)


def main() -> None:
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    locked = {
        (package["name"], package["version"]): package["checksum"]
        for package in lock["package"]
        if str(package.get("source", "")).startswith("registry+")
    }

    selected: set[tuple[str, str]] = set()
    for target in TARGETS:
        target_metadata = metadata(target)
        packages = {package["id"]: package for package in target_metadata["packages"]}
        resolve = target_metadata.get("resolve")
        if resolve is None:
            raise RuntimeError(f"cargo metadata returned no dependency graph for {target}")
        for node in resolve["nodes"]:
            package = packages[node["id"]]
            if str(package.get("source", "")).startswith("registry+"):
                key = (package["name"], package["version"])
                selected.add(key)

    VENDOR.mkdir()
    package_metadata = {}
    for key in sorted(selected):
        destination = VENDOR / f"{key[0]}-{key[1]}"
        archive = cached_archive(*key, locked[key])
        package_metadata[key] = archive_metadata(archive, *key)
        extract_crate(archive, destination)
        write_checksum(destination, locked[key])

    # Cargo's directory source must resolve locked packages even when their target
    # predicates are false. Preserve their indexed metadata without claiming to
    # provide source support for non-Linux targets.
    for key in sorted(locked.keys() - selected):
        package_checksum = locked[key]
        destination = VENDOR / f"{key[0]}-{key[1]}"
        archive = cached_archive(*key, package_checksum)
        package_metadata[key] = archive_metadata(archive, *key)
        (destination / "src").mkdir(parents=True)
        (destination / "Cargo.toml").write_text(
            placeholder_manifest(*key, package_metadata[key][0]),
            encoding="ascii",
        )
        (destination / "src/lib.rs").write_text("\n", encoding="ascii")
        write_checksum(destination, package_checksum)

    CARGO_CONFIG.parent.mkdir(parents=True, exist_ok=True)
    CARGO_CONFIG.write_text(
        '[source.crates-io]\nreplace-with = "vendored-sources"\n\n'
        '[source.vendored-sources]\ndirectory = "vendor"\n',
        encoding="ascii",
    )
    write_notices(package_metadata, selected)


if __name__ == "__main__":
    main()
