#!/bin/bash

# source myenv.sh to get the env flags

export PINVIO_RUST_CHANNEL=nightly-2025-08-01
export RUSTFLAGS="-L $HOME/.rustup/toolchains/${PINVIO_RUST_CHANNEL}-x86_64-unknown-linux-gnu/lib"
export LD_LIBRARY_PATH="${LD_LIBRARY_PATH}:$HOME/.rustup/toolchains/${PINVIO_RUST_CHANNEL}-x86_64-unknown-linux-gnu/lib"
export RUST_SYSROOT="$HOME/.rustup/toolchains/${PINVIO_RUST_CHANNEL}-x86_64-unknown-linux-gnu"
export PINVIO_LOG_PATH="/dev/shm/pinvio.log"
