#!/usr/bin/env python3
"""Generate metadata-only 0.0.0 dependency crates for configuring trust."""
import argparse
import shutil
import tomllib
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--out', type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
workspace = tomllib.loads((root / 'Cargo.toml').read_text())
for member in workspace['workspace']['members']:
    manifest = tomllib.loads((root / member / 'Cargo.toml').read_text())
    name = manifest['package']['name']
    if name == 'qk-cli':  # Already reserved by tools/crates-io/qk-cli.
        continue
    directory = args.out.resolve() / name
    (directory / 'src').mkdir(parents=True, exist_ok=True)
    (directory / 'Cargo.toml').write_text(f'''[package]
name = "{name}"
version = "0.0.0"
edition = "2024"
license = "MIT OR Apache-2.0"
repository = "https://github.com/Pajn/qk"
description = "Name reservation for {name}, a component of the qk task runner"
readme = "README.md"
include = ["Cargo.toml", "README.md", "src/lib.rs", "LICENSE-MIT", "LICENSE-APACHE"]
publish = ["crates-io"]

[workspace]
''')
    (directory / 'src/lib.rs').write_text('//! Metadata-only name reservation. No public API is provided.\n')
    (directory / 'README.md').write_text(
        f'# {name}\n\nThis 0.0.0 package reserves the name and enables trusted publishing.\n'
        'It contains no functional implementation or public API.\n'
        'The implementation is developed at https://github.com/Pajn/qk.\n'
    )
    for license_name in ['LICENSE-MIT', 'LICENSE-APACHE']:
        shutil.copyfile(root / license_name, directory / license_name)
    print(directory)
