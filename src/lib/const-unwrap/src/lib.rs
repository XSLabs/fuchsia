// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `const-unwrap` provides functions which can be used to unwrap [`Result`]s in
//! a const context.

#![no_std]

/// Unwraps a [`Result`]'s [`Ok`] variant, or panics.
///
/// `const_unwrap_result` is a `const fn`, and may be called in a const context.
/// Note that, if called from a non-const context, `const_unwrap_result` will
/// execute at runtime, not at compile time.
pub const fn const_unwrap_result<T: Copy, E: Copy + core::fmt::Debug>(opt: Result<T, E>) -> T {
    // Require `E: Debug` because `Result::unwrap` does, and we don't want to
    // allow code to use `const_unwrap_result` which can't later be transitioned
    // to `Result::unwrap` once it's const-stable.
    //
    // We can't include the error itself in the panic message because
    // const-panicking only supports string literals, and does not support
    // format string arguments. There does not appear to be a tracking issue for
    // this, but the following links may be relevant:
    // - https://doc.rust-lang.org/beta/unstable-book/library-features/const-format-args.html
    // - https://doc.rust-lang.org/std/macro.const_format_args.html
    match opt {
        Ok(x) => x,
        Err(_) => panic!("const_unwrap_result called on err `Err` value"),
    }
}

const _UNWRAP_OK: usize = const_unwrap_result(Result::<_, bool>::Ok(0));

// The following don't compile. Remove `#[cfg(ignore)]` and compile to verify.
#[cfg(ignore)]
mod dont_compile {
    const _UNWRAP_ERR: usize = const_unwrap_result!(Err(0));
}
