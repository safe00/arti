#!/bin/sh

set -e
cd "$(dirname "$0")/.."

for subcargo in */Cargo.toml ; do

    cd "$(dirname "$subcargo")"
    cargo readme > README.md
    cd ..

done
