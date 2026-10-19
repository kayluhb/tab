from dataclasses import dataclass
import os
import json
import datetime
import hashlib
import pathlib
import shutil
import subprocess
from typing import Dict, List, Mapping, Sequence
from util import (
    Variant,
    get_variants,
    isDarwin,
    run_cmd,
    run_cmd_output,
    info,
    version,
)
from rust import rust_targets, rust_env, get_target_triple
from test import run_cargo_tests, run_clippy
from signing import (
    CdSigningData,
    CdSigningType,
    rebundle_dmg,
    cd_sign_file,
    apple_notarize_file,
)
from importlib import import_module
from const import (
    APP_NAME,
    CLI_BINARY_NAME,
    CLI_PACKAGE_NAME,
    DESKTOP_PACKAGE_NAME,
    DESKTOP_PACKAGE_PATH,
    DMG_NAME,
    MACOS_BUNDLE_ID,
    PTY_BINARY_NAME,
    PTY_PACKAGE_NAME,
    TAURI_PRODUCT_NAME,
    URL_SCHEMA,
)

BUILD_DIR_RELATIVE = pathlib.Path(os.environ.get("BUILD_DIR") or "build")
BUILD_DIR = BUILD_DIR_RELATIVE.absolute()


# Completion specs, built from source and bundled into the app
SPECS_REPO = "https://github.com/kayluhb/tab-autocomplete.git"
SPECS_COMMIT = "aef52acff84c45edde61ae610cc2c964802b9a38"
# Our own specs, laid out like upstream's src/; they add to or replace upstream specs
SPECS_OVERRIDES_PATH = pathlib.Path("specs")

# Themes, bundled into the app
THEMES_REPO = "https://github.com/kayluhb/tab-themes.git"
THEMES_COMMIT = "ab71dc82974f4750736d2583867bf6c7219515cb"


@dataclass
class NpmBuildOutput:
    dashboard_path: pathlib.Path
    autocomplete_path: pathlib.Path
    specs_path: pathlib.Path
    vscode_path: pathlib.Path


def specs_overrides_digest() -> str:
    digest = hashlib.sha256()
    for path in sorted(SPECS_OVERRIDES_PATH.rglob("*")):
        if path.is_file():
            digest.update(str(path.relative_to(SPECS_OVERRIDES_PATH)).encode())
            digest.update(path.read_bytes())
    return digest.hexdigest()


def specs_cloned(repo: pathlib.Path) -> bool:
    if not (repo / "node_modules").exists():
        return False
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True)
    origin = subprocess.run(["git", "remote", "get-url", "origin"], cwd=repo, capture_output=True, text=True)
    return (
        head.returncode == 0
        and head.stdout.strip() == SPECS_COMMIT
        and origin.returncode == 0
        and origin.stdout.strip() == SPECS_REPO
    )


def build_specs() -> pathlib.Path:
    repo = BUILD_DIR / "autocomplete-specs"
    specs_path = repo / "build"
    # Written only after a successful build, so a build that fails part way reruns next time
    stamp = repo / "overrides.sha256"
    digest = specs_overrides_digest()
    if not specs_cloned(repo):
        shutil.rmtree(repo, ignore_errors=True)
        run_cmd(["git", "clone", "--filter=blob:none", SPECS_REPO, repo])
        run_cmd(["git", "checkout", "--quiet", SPECS_COMMIT], cwd=repo)
        # --ignore-workspace: BUILD_DIR sits inside this repo's pnpm workspace
        run_cmd(["pnpm", "install", "--frozen-lockfile", "--ignore-scripts", "--ignore-workspace"], cwd=repo)
    if (specs_path / "index.json").exists() and stamp.exists() and stamp.read_text() == digest:
        info(f"Specs already built at {SPECS_COMMIT}")
    else:
        # Reset upstream's sources first so a removed or renamed spec in specs/ doesn't linger
        run_cmd(["git", "checkout", "--quiet", "--", "src"], cwd=repo)
        run_cmd(["git", "clean", "-fdq", "--", "src"], cwd=repo)
        if SPECS_OVERRIDES_PATH.exists():
            shutil.copytree(SPECS_OVERRIDES_PATH, repo / "src", dirs_exist_ok=True)
        stamp.unlink(missing_ok=True)
        shutil.rmtree(specs_path, ignore_errors=True)
        run_cmd(["pnpm", "--ignore-workspace", "build"], cwd=repo)

    # The spec compiler exits 0 even when specs fail to bundle, so check the output
    index = json.loads((specs_path / "index.json").read_text())
    missing = [
        name
        for name in index["completions"]
        # diff-versioned specs live at {name}/index.js
        if not (specs_path / f"{name}.js").exists() and not (specs_path / name / "index.js").exists()
    ]
    if missing:
        raise RuntimeError(f"{len(missing)} completion specs failed to build, e.g. {missing[:5]}")
    stamp.write_text(digest)
    return specs_path


@dataclass
class MacOSBuildOutput:
    dmg_path: pathlib.Path
    app_gztar_path: pathlib.Path


def build_npm_packages(run_test: bool = True) -> NpmBuildOutput:
    run_cmd(["pnpm", "install", "--frozen-lockfile"])

    # set the version of extensions/vscode
    package_json_path = pathlib.Path("extensions/vscode/package.json")
    package_json_text = package_json_path.read_text()
    package_json = json.loads(package_json_text)
    package_json["version"] = version()
    package_json_path.write_text(json.dumps(package_json, indent=2))

    run_cmd(["pnpm", "build"])
    if run_test:
        run_cmd(["pnpm", "test", "--run"])

    # revert the package.json
    package_json_path.write_text(package_json_text)

    # copy to output
    dashboard_path = BUILD_DIR / "dashboard"
    shutil.rmtree(dashboard_path, ignore_errors=True)
    shutil.copytree("packages/dashboard-app/dist", dashboard_path)

    autocomplete_path = BUILD_DIR / "autocomplete"
    shutil.rmtree(autocomplete_path, ignore_errors=True)
    shutil.copytree("packages/autocomplete-app/dist", autocomplete_path)

    vscode_path = BUILD_DIR / "vscode-plugin.vsix"
    shutil.rmtree(vscode_path, ignore_errors=True)
    shutil.copy2(f"extensions/vscode/codewhisperer-for-command-line-companion-{version()}.vsix", vscode_path)
    shutil.copy2(
        f"extensions/vscode/codewhisperer-for-command-line-companion-{version()}.vsix",
        "crates/tab_integrations/src/vscode/vscode-plugin.vsix",
    )

    return NpmBuildOutput(
        dashboard_path=dashboard_path,
        autocomplete_path=autocomplete_path,
        specs_path=build_specs(),
        vscode_path=vscode_path,
    )


def build_cargo_bin(
    variant: Variant,
    release: bool,
    package: str,
    output_name: str | None = None,
    features: Mapping[str, Sequence[str]] | None = None,
    targets: Sequence[str] = [],
) -> pathlib.Path:
    args = ["cargo", "build", "--locked", "--package", package]

    if release:
        args.append("--release")

    for target in targets:
        args.extend(["--target", target])

    if features and features.get(package):
        args.extend(["--features", ",".join(features[package])])

    run_cmd(
        args,
        env={
            **os.environ,
            **rust_env(release=release, variant=variant),
        },
    )

    if release:
        target_subdir = "release"
    else:
        target_subdir = "debug"

    # create the macOS binary, universal when building for several targets
    if isDarwin():
        out_path = BUILD_DIR / f"{output_name or package}-{get_target_triple()}"
        args = [
            "lipo",
            "-create",
            "-output",
            out_path,
        ]
        for target in targets:
            args.append(pathlib.Path("target") / target / target_subdir / package)
        run_cmd(args)
        return out_path
    else:
        # linux does not cross compile arch
        target = targets[0]
        target_path = pathlib.Path("target") / target / target_subdir / package
        out_path = BUILD_DIR / "bin" / f"{(output_name or package)}-{target}"
        out_path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(target_path, out_path)
        return out_path


def gen_manifest() -> str:
    return json.dumps(
        {
            "managed_by": "dmg",
            "packaged_at": datetime.datetime.now().isoformat(),
            "packaged_by": "amazon",
            "variant": "full",
            "version": version(),
            "kind": "dmg",
            "default_channel": "stable",
        }
    )


def build_macos_ime(
    release: bool,
    is_prod: bool,
    signing_data: CdSigningData | None,
    targets: Sequence[str] = [],
) -> pathlib.Path:
    fig_input_method_bin = build_cargo_bin(
        release=release, variant=Variant.FULL, package="tab_input_method", targets=targets
    )
    input_method_app = pathlib.Path("build/TabInputMethod.app")

    (input_method_app / "Contents/MacOS").mkdir(parents=True, exist_ok=True)

    shutil.copy2(
        fig_input_method_bin,
        input_method_app / "Contents/MacOS/tab_input_method",
    )
    shutil.copy2(
        "crates/tab_input_method/Info.plist",
        input_method_app / "Contents/Info.plist",
    )
    shutil.copytree(
        "crates/tab_input_method/resources",
        input_method_app / "Contents/Resources",
        dirs_exist_ok=True,
    )

    if signing_data:
        info("Signing macos ime")
        cd_sign_file(input_method_app, CdSigningType.IME, signing_data, is_prod=is_prod)
        apple_notarize_file(input_method_app, signing_data)

    return input_method_app


def macos_tauri_config(cli_path: pathlib.Path, pty_path: pathlib.Path, target: str) -> str:
    config = {
        "tauri": {
            "bundle": {
                "externalBin": [
                    # Note that tauri bundling expects the target triple to be appended to each binary,
                    # but in the config it must be strippled.
                    str(cli_path).removesuffix(f"-{target}"),
                    str(pty_path).removesuffix(f"-{target}"),
                ],
                "resources": ["manifest.json"],
            }
        }
    }
    return json.dumps(config)


def build_macos_desktop_app(
    release: bool,
    pty_path: pathlib.Path,
    cli_path: pathlib.Path,
    npm_packages: NpmBuildOutput,
    is_prod: bool,
    signing_data: CdSigningData | None,
    features: Mapping[str, Sequence[str]] | None = None,
    targets: Sequence[str] = [],
) -> MacOSBuildOutput:
    target = get_target_triple()

    info("Building macos ime")
    ime_app = build_macos_ime(release=release, signing_data=signing_data, targets=targets, is_prod=is_prod)

    info("Writing manifest")
    manifest_path = pathlib.Path(DESKTOP_PACKAGE_PATH) / "manifest.json"
    manifest_path.write_text(gen_manifest())

    info("Building tauri config")
    tauri_config_path = pathlib.Path(DESKTOP_PACKAGE_PATH) / "build-config.json"
    tauri_config_path.write_text(macos_tauri_config(cli_path=cli_path, pty_path=pty_path, target=target))

    info("Building", DESKTOP_PACKAGE_NAME)

    cargo_tauri_args = [
        "cargo-tauri",
        "build",
        "--config",
        "build-config.json",
        "--target",
        target,
    ]

    if features and features.get(DESKTOP_PACKAGE_NAME):
        cargo_tauri_args.extend(["--features", ",".join(features[DESKTOP_PACKAGE_NAME])])

    run_cmd(
        cargo_tauri_args,
        cwd=DESKTOP_PACKAGE_PATH,
        env={**os.environ, **rust_env(release=release, variant=Variant.FULL), "BUILD_DIR": BUILD_DIR},
    )

    # clean up
    manifest_path.unlink(missing_ok=True)
    tauri_config_path.unlink(missing_ok=True)

    target_bundle = pathlib.Path(f"target/{target}/release/bundle/macos/{TAURI_PRODUCT_NAME}.app")
    app_path = BUILD_DIR / f"{APP_NAME}.app"
    shutil.rmtree(app_path, ignore_errors=True)
    shutil.copytree(target_bundle, app_path)

    info_plist_path = app_path / "Contents/Info.plist"

    # Change the display name of the app
    run_cmd(["defaults", "write", info_plist_path, "CFBundleDisplayName", APP_NAME])
    run_cmd(["defaults", "write", info_plist_path, "CFBundleName", APP_NAME])

    # Specifies the app is an "agent app"
    run_cmd(["defaults", "write", info_plist_path, "LSUIElement", "-bool", "TRUE"])

    # Add q:// association to bundle
    run_cmd(
        [
            "plutil",
            "-insert",
            "CFBundleURLTypes",
            "-xml",
            f"""<array>
    <dict>
        <key>CFBundleURLName</key>
        <string>{MACOS_BUNDLE_ID}</string>
        <key>CFBundleURLSchemes</key>
        <array>
        <string>{URL_SCHEMA}</string>
        </array>
    </dict>
    </array>
    """,
            info_plist_path,
        ]
    )

    info("Copying TabInputMethod.app into bundle")
    helpers_dir = app_path / "Contents/Helpers"
    helpers_dir.mkdir(parents=True, exist_ok=True)
    shutil.copytree(ime_app, helpers_dir.joinpath("TabInputMethod.app"))

    info("Grabbing themes")
    theme_repo = BUILD_DIR / "themes"
    shutil.rmtree(theme_repo, ignore_errors=True)
    run_cmd(["git", "clone", "--filter=blob:none", THEMES_REPO, theme_repo])
    run_cmd(["git", "checkout", "--quiet", THEMES_COMMIT], cwd=theme_repo)
    shutil.copytree(theme_repo / "themes", app_path / "Contents/Resources/themes")

    info("Copying dashboard into bundle")
    shutil.copytree(npm_packages.dashboard_path, app_path / "Contents/Resources/dashboard")

    info("Copying autocomplete into bundle")
    shutil.copytree(npm_packages.autocomplete_path, app_path / "Contents/Resources/autocomplete")

    info("Copying completion specs into bundle")
    shutil.copytree(npm_packages.specs_path, app_path / "Contents/Resources/specs")

    # Add symlinks
    # os.symlink(f"./{CLI_BINARY_NAME}", app_path / "Contents/MacOS/cli")
    # os.symlink(f"./{PTY_BINARY_NAME}", app_path / "Contents/MacOS/pty")

    dmg_path = BUILD_DIR / f"{DMG_NAME}.dmg"
    dmg_path.unlink(missing_ok=True)

    dmg_resources_dir = pathlib.Path("bundle/dmg")
    background_path = dmg_resources_dir / "background.png"
    icon_path = dmg_resources_dir / "VolumeIcon.icns"

    # we use a dynamic import here so that we dont use this dep
    # on other platforms
    dmgbuild = import_module("dmgbuild")

    dmgbuild.build_dmg(
        volume_name=APP_NAME,
        filename=dmg_path,
        settings={
            "format": "ULFO",
            "background": str(background_path),
            "icon": str(icon_path),
            "text_size": 12,
            "icon_size": 160,
            "window_rect": ((100, 100), (660, 400)),
            "files": [str(app_path)],
            "symlinks": {"Applications": "/Applications"},
            "icon_locations": {
                app_path.name: (180, 170),
                "Applications": (480, 170),
            },
        },
    )

    info(f"Created dmg at {dmg_path}")

    if signing_data:
        sign_and_rebundle_macos(app_path=app_path, dmg_path=dmg_path, signing_data=signing_data, is_prod=is_prod)

    app_gztar_path = shutil.make_archive(str(BUILD_DIR / APP_NAME), "gztar", app_path.parent, app_path.name)
    info(f"Created app tar.gz at {app_gztar_path}")

    return MacOSBuildOutput(dmg_path=dmg_path, app_gztar_path=pathlib.Path(app_gztar_path))


def sign_and_rebundle_macos(app_path: pathlib.Path, dmg_path: pathlib.Path, signing_data: CdSigningData, is_prod: bool):
    info("Signing app and dmg")

    # Sign the application
    cd_sign_file(app_path, CdSigningType.APP, signing_data, is_prod=is_prod)

    # Notarize the application

    apple_notarize_file(app_path, signing_data)

    # Rebundle the dmg file with the signed and notarized application
    rebundle_dmg(app_path=app_path, dmg_path=dmg_path)

    # Sign the dmg
    cd_sign_file(dmg_path, CdSigningType.DMG, signing_data, is_prod=is_prod)

    # Notarize the dmg
    apple_notarize_file(dmg_path, signing_data)

    info("Done signing!!")


def generate_sha(path: pathlib.Path) -> pathlib.Path:
    shasum_output = run_cmd_output(["shasum", "-a", "256", path])

    sha = shasum_output.split(" ")[0]
    path = path.with_name(f"{path.name}.sha256")
    path.write_text(sha)
    info(f"Wrote sha256sum to {path}:", sha)
    return path


@dataclass
class BinaryPaths:
    cli_path: pathlib.Path
    pty_path: pathlib.Path


BuildOutput = Dict[Variant, BinaryPaths]


def build(
    release: bool,
    variants: List[Variant] | None = None,
    output_bucket: str | None = None,
    signing_bucket: str | None = None,
    aws_account_id: str | None = None,
    apple_id_secret: str | None = None,
    signing_role_name: str | None = None,
    stage_name: str | None = None,
    run_lints: bool = True,
    run_test: bool = True,
) -> BuildOutput:
    variants = variants or get_variants()

    if signing_bucket and aws_account_id and apple_id_secret and signing_role_name:
        signing_data = CdSigningData(
            bucket_name=signing_bucket,
            aws_account_id=aws_account_id,
            notarizing_secret_id=apple_id_secret,
            signing_role_name=signing_role_name,
        )
    else:
        signing_data = None

    cargo_features: Mapping[str, Sequence[str]] = {}

    match stage_name:
        case "prod" | None:
            info("Building for prod")
        case "gamma":
            info("Building for gamma")
        case _:
            raise ValueError(f"Unknown stage name: {stage_name}")

    info(f"Release: {release}")
    info(f"Cargo features: {cargo_features}")
    info(f"Signing app: {signing_data is not None}")
    info(f"Variants: {[variant.name for variant in variants]}")

    BUILD_DIR.mkdir(parents=True, exist_ok=True)

    npm_packages = None
    if Variant.FULL in variants:
        info("Building npm packages")
        npm_packages = build_npm_packages(run_test=run_test)

    targets = rust_targets()

    # Mac has multiple targets, so just use the default for the platform
    # for testing and linting.
    cargo_test_target = None if isDarwin() else targets[0]

    if run_test:
        info("Running cargo tests")
        run_cargo_tests(variants=variants, features=cargo_features, target=cargo_test_target)

    if run_lints:
        run_clippy(variants=variants, features=cargo_features, target=cargo_test_target)

    build_output: BuildOutput = {}
    for variant in variants:
        info(f"Building variant: {variant.name}")

        info("Building", CLI_PACKAGE_NAME)
        cli_path = build_cargo_bin(
            variant=variant,
            release=release,
            package=CLI_PACKAGE_NAME,
            output_name=CLI_BINARY_NAME,
            features=cargo_features,
            targets=targets,
        )

        info("Building", PTY_PACKAGE_NAME)
        pty_path = build_cargo_bin(
            variant=variant,
            release=release,
            package=PTY_PACKAGE_NAME,
            output_name=PTY_BINARY_NAME,
            features=cargo_features,
            targets=targets,
        )

        if isDarwin():
            info(f"Building {DMG_NAME}.dmg")
            if not npm_packages:
                raise RuntimeError("npm packages must be built for Mac")

            build_paths = build_macos_desktop_app(
                release=release,
                cli_path=cli_path,
                pty_path=pty_path,
                npm_packages=npm_packages,
                signing_data=signing_data,
                features=cargo_features,
                targets=targets,
                is_prod=stage_name == "prod" or stage_name is None,
            )

            sha_path = generate_sha(build_paths.dmg_path)

            if output_bucket:
                staging_location = f"s3://{output_bucket}/staging/"
                info(f"Build complete, sending to {staging_location}")

                run_cmd(["aws", "s3", "cp", build_paths.dmg_path, staging_location])
                run_cmd(["aws", "s3", "cp", build_paths.app_gztar_path, staging_location])
                run_cmd(["aws", "s3", "cp", sha_path, staging_location])

        build_output[variant] = BinaryPaths(cli_path=cli_path, pty_path=pty_path)

    return build_output
