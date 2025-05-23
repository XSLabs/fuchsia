// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use thiserror::Error;

/// Errors that can be produced while encoding FIDL messages.
#[derive(Clone, Error, Debug)]
pub enum EncodeError {
    /// A required handle was invalid.
    #[error("required handle was invalid")]
    InvalidRequiredHandle,

    /// An encoded union had an unknown ordinal
    #[error("cannot encode unknown union ordinal of {0}")]
    UnknownUnionOrdinal(usize),

    /// Attempted to encode a value larger than 4 bytes in an inline envelope
    #[error("cannot encode a {0}-byte value in a 4-byte inline envelope")]
    ExpectedInline(usize),

    /// Attempted to encode a driver handle with an encoder that does not support them.
    #[error("cannot encode driver handles with this encoder")]
    DriverHandlesUnsupported,

    /// Expected a driver handle but was given a normal zircon handle
    #[error("expected a driver handle but was given a zircon handle")]
    ExpectedDriverHandle,

    /// Expected a zircon handle but was given a driver handle
    #[error("expected a zircon handle but was given a driver handle")]
    ExpectedZirconHandle,
}
