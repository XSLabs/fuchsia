#!/usr/bin/env fuchsia-vendored-python
#
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Generates various data blobs needed by Gigaboot

Usage:
    <script>
    <path to permanent attributes files>
    [--gbl <path to GBL EFI app file>]
    <output source file>
"""

import argparse
import hashlib
import sys
import textwrap

PERMANENT_ATTRIBUTE_ARRAY_NAME = "kPermanentAttributes"
PERMANENT_ATTRIBUTE_HASH_ARRAY_NAME = "kPermanentAttributesHash"
GBL_EFI_APP_ARRAY_NAME = "kGblEfiApp"


def byte_array_declaration(data: bytes, name: str) -> str:
    """Generates a C array declaration for bytes"""
    type_name = "const uint8_t"
    byte_str = "".join(f"0x{b:02x}," for b in data)
    array_body = "{%s}" % byte_str
    return f"{type_name} {name}[] = {array_body};"


def parse_args():
    parser = argparse.ArgumentParser()

    parser.add_argument(
        "permanent_attributes", help="path to permanent attributes file"
    )
    parser.add_argument("out", help="path to output file")
    parser.add_argument("--gbl", help="path to the GBL EFI app file")

    return parser.parse_args()


def main() -> int:
    args = parse_args()
    with open(args.permanent_attributes, "rb") as input_file:
        attr_data = input_file.read()
        attr_data_hash = hashlib.sha256(attr_data).digest()

    # Don't care. Prevents compiler from complaining about zero size array.
    gbl = b"\x00"
    if args.gbl:
        with open(args.gbl, "rb") as input_file:
            gbl = input_file.read()

    with open(args.out, "w") as output_file:
        output_file.write(
            textwrap.dedent(
                """\
                // Copyright 2022 The Fuchsia Authors. All rights reserved.
                // Use of this source code is governed by a BSD-style license that can be
                // found in the LICENSE file.

                #include <stdint.h>
                #include <span>

                namespace {
                    %s
                    %s
                    %s
                }

                namespace gigaboot {
                    const std::span<const uint8_t> GetPermanentAttributes() {
                        return std::span{%s};
                    }

                    const std::span<const uint8_t> GetPermanentAttributesHash() {
                        return std::span{%s};
                    }

                    const std::span<const uint8_t> GetGblEfiApp() {
                        return std::span{%s};
                    }
                }
                """
                % (
                    byte_array_declaration(
                        attr_data, PERMANENT_ATTRIBUTE_ARRAY_NAME
                    ),
                    byte_array_declaration(
                        attr_data_hash, PERMANENT_ATTRIBUTE_HASH_ARRAY_NAME
                    ),
                    byte_array_declaration(gbl, GBL_EFI_APP_ARRAY_NAME),
                    PERMANENT_ATTRIBUTE_ARRAY_NAME,
                    PERMANENT_ATTRIBUTE_HASH_ARRAY_NAME,
                    GBL_EFI_APP_ARRAY_NAME,
                )
            )
        )

    return 0


if __name__ == "__main__":
    sys.exit(main())
