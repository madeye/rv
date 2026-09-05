#!/usr/bin/env python3
"""Package RV, optionally with the signing/profile needed for Touch ID Keychain access."""

import argparse
import datetime
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--version", default="0.1.0")
    parser.add_argument("--identity", help="Developer ID Application signing identity")
    parser.add_argument("--profile", type=Path, help="Matching Developer ID provisioning profile")
    args = parser.parse_args()
    if bool(args.identity) != bool(args.profile):
        parser.error("--identity and --profile must be supplied together")
    if not args.binary.is_file():
        parser.error("--binary must name an existing executable")
    if args.output.suffix != ".app" or args.output.exists():
        parser.error("--output must name a new .app bundle")

    bundle_id = "io.github.madeye.rv"
    entitlements = None
    if args.profile:
        profile = plistlib.loads(subprocess.check_output([
            "security", "cms", "-D", "-i", str(args.profile)
        ]))
        allowed = profile["Entitlements"]
        app_id = allowed["com.apple.application-identifier"]
        if app_id.split(".", 1)[1] != bundle_id:
            parser.error("the profile must belong to io.github.madeye.rv")
        if profile["ExpirationDate"] <= datetime.datetime.now(datetime.timezone.utc).replace(tzinfo=None):
            parser.error("the provisioning profile has expired")
        if "OSX" not in profile.get("Platform", []) or not profile.get("ProvisionsAllDevices"):
            parser.error("a Developer ID macOS distribution profile is required")
        groups = allowed.get("keychain-access-groups", [])
        if not any(app_id == group or (group.endswith(".*") and app_id.startswith(group[:-1])) for group in groups):
            parser.error("the profile does not allow the app's Keychain access group")
        entitlements = {
            "com.apple.application-identifier": app_id,
            "com.apple.developer.team-identifier": allowed["com.apple.developer.team-identifier"],
            "keychain-access-groups": [app_id],
        }

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rv-package-", dir=args.output.parent) as work:
        app = Path(work) / "RV.app"
        contents = app / "Contents"
        (contents / "MacOS").mkdir(parents=True)
        (contents / "Resources").mkdir()
        shutil.copy2(args.binary, contents / "MacOS/rv")
        assets = Path(__file__).resolve().parent.parent / "assets"
        shutil.copy2(assets / "app-icon.icns", contents / "Resources/app-icon.icns")
        info = {
            "CFBundleName": "RV", "CFBundleDisplayName": "RV",
            "CFBundleIdentifier": bundle_id, "CFBundleExecutable": "rv",
            "CFBundleIconFile": "app-icon.icns", "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": args.version, "CFBundleVersion": args.version,
            "LSMinimumSystemVersion": "12.0", "NSHighResolutionCapable": True,
            "NSSupportsAutomaticGraphicsSwitching": True,
        }
        with (contents / "Info.plist").open("wb") as file:
            plistlib.dump(info, file)
        command = ["codesign", "--force", "--sign", args.identity or "-"]
        if entitlements:
            shutil.copy2(args.profile, contents / "embedded.provisionprofile")
            entitlement_file = Path(work) / "entitlements.plist"
            with entitlement_file.open("wb") as file:
                plistlib.dump(entitlements, file)
            command += ["--options", "runtime", "--timestamp", "--entitlements", str(entitlement_file)]
        subprocess.run(command + [str(app)], check=True)
        subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
        shutil.move(str(app), args.output)
    print(f"Packaged {args.output}")


if __name__ == "__main__":
    main()
