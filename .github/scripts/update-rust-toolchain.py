#!/usr/bin/env python3
"""Update toolchain.channel without rewriting unrelated TOML."""

import argparse
import tomllib
from pathlib import Path

import tomlkit
from tomlkit.items import String


def update_channel(text, latest):
    doc = tomlkit.parse(text)
    table = doc["toolchain"]
    old = table["channel"]
    if not isinstance(old, String):
        raise TypeError("toolchain.channel must be a string")
    if old == latest:
        return text
    table["channel"] = tomlkit.string(
        latest, literal=old.type.is_literal(), multiline=old.type.is_multiline()
    )
    return tomlkit.dumps(doc)


def self_test():
    current, latest = "1.98.0", "1.99.0"
    cases = [
        '[toolchain]\nchannel = "1.98.0"\ncomponents = ["rustfmt", "clippy"]\n',
        "[review_metadata]\nchannel = \"1.99.0\"\n\n[toolchain]\nchannel = '1.98.0' # pin\n",
        "# pin\r\n[toolchain] # header\r\n\t'channel'\t=\t'1.98.0'  # retain\r\ncomponents = [ 'clippy', \"rustfmt\", ]\r\n[review_metadata]\r\nchannel=\"foreign\"\r\n",
        '[toolchain]\nchannel = "1.98.0"',
        "toolchain.channel = '1.98.0' # pin\nreview_metadata.channel = \"foreign\"\n",
        "\"toolchain\" . 'channel' = '1.98.0'\n",
        "toolchain = { channel = '1.98.0', components = [\"rustfmt\"] } # retain\n",
        '[toolchain]\nchannel = """1.98.0""" # pin\n',
        "[toolchain]\nchannel = '''1.98.0''' # pin\n",
        "[review_metadata]\ntext = '''\n[toolchain]\nchannel = \"0.0.0\"\n'''\n[toolchain]\nchannel = \"1.98.0\"\n",
    ]
    for text in cases:
        updated = update_channel(text, latest)
        expected = tomllib.loads(text)
        expected["toolchain"]["channel"] = latest
        assert tomllib.loads(updated) == expected
        assert updated == text.replace(current, latest, 1), repr(updated)
        assert update_channel(text, current) == text

    invalid = [
        '[review_metadata]\nchannel = "1.98.0"\n',
        '[toolchain]\ncomponents = ["rustfmt"]\n',
        "[toolchain]\nchannel = 199\n",
        '[toolchain]\nchannel = "1.98.0"\nchannel = "1.99.0"\n',
        '[toolchain]\nchannel = "unterminated\n',
    ]
    for text in invalid:
        try:
            update_channel(text, latest)
        except (KeyError, TypeError, ValueError, tomlkit.exceptions.TOMLKitError):
            pass
        else:
            raise AssertionError(f"invalid toolchain accepted: {text!r}")
    print(
        f"OK: {len(cases)} channel updates preserve unrelated TOML and formatting; {len(invalid)} invalid inputs rejected."
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("latest", nargs="?")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if args.latest is None:
        parser.error("latest is required")
    path = Path("rust-toolchain.toml")
    original = path.read_bytes().decode("utf-8")
    updated = update_channel(original, args.latest)
    if updated != original:
        path.write_bytes(updated.encode("utf-8"))


if __name__ == "__main__":
    main()
