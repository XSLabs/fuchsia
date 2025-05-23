# bt-affordances

The bt-affordances are a Rust library with C FFI bindings that expose basic
Bluetooth workflows for automated testing and tooling.

## How to update the C FFI bindings to the Rust affordances

The bindings are generated using [cbindgen](https://github.com/mozilla/cbindgen).
The Rust library is contained in `src/rust_affordances`. The bindings are located
at `ffi_c/bindings.h`.

To (re)generate these bindings,

1. Link the Rust toolchain in your environment ([details on fuchsia.dev](https://fuchsia.dev/fuchsia-src/development/languages/rust/cargo#cargo-config-gen)):
```
$ rustup toolchain link fuchsia $($FUCHSIA_DIR/scripts/youcompleteme/paths.py VSCODE_RUST_TOOLCHAIN)
$ rustup default fuchsia
```

2. Install `cbindgen`. The latest version with which we've validated compatibility
is 0.28.0:
```
$ cargo install --version 0.28.0 --force cbindgen
```

3. Generate a `Cargo.toml` manifest for the Rust library ([details on fuchsia.dev](https://fuchsia.dev/fuchsia-src/development/languages/rust/cargo#cargo-toml-gen)):
```
fx args # then append "//build/rust:cargo_toml_gen" to `host_labels`
fx args # then append "//src/connectivity/bluetooth/testing/bt-affordances:_affordances_c_rustc_static"
        # to `build_only_labels`

fx build //build/rust:cargo_toml_gen
fx gen-cargo //src/connectivity/bluetooth/testing/bt-affordances:_affordances_c_rustc_static
```

4. Create/update the `bindings.h` header. It's normal to see some warnings when running this command:
```
RUSTUP_TOOLCHAIN=fuchsia cbindgen $FUCHSIA_DIR/src/connectivity/bluetooth/testing/bt-affordances/ -o $FUCHSIA_DIR/src/connectivity/bluetooth/testing/bt-affordances/ffi_c/bindings.h
```

5. Add Fuchsia copyright and header guards to `bindings.h`:
```
// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_BLUETOOTH_TESTING_BT_AFFORDANCES_FFI_C_BINDINGS_H_
#define SRC_CONNECTIVITY_BLUETOOTH_TESTING_BT_AFFORDANCES_FFI_C_BINDINGS_H_

... (don't modify the autogenerated code) ...

#endif  // SRC_CONNECTIVITY_BLUETOOTH_TESTING_BT_AFFORDANCES_FFI_C_BINDINGS_H_
```
This enables you to run `fx format-code` to format `bindings.h`.