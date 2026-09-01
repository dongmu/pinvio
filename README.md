Pinvio
------
Pinvio is a static analysis tool analyzing Rust crates to identify "Pin Violation" bugs.

### Rust toolchain
- `nightly-2025-08-01`
- Components
  1. `rustc-dev`
  2. `llvm-tools`

### Usage

```shell
# setup env arg
source ./setup_env.sh

# build pinvio
cargo build

# add the compiled pinvio and cargo-pinvio into PATH
export PATH=xxx

# test pinvio
cd tests/yyy
cargo pinvio
```

