// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_INSPECT_CPP_VMO_LIMITS_H_
#define LIB_INSPECT_CPP_VMO_LIMITS_H_

#include <zircon/types.h>

#include <climits>
#include <cstdint>

namespace inspect {
namespace internal {

// The size for order 0.
constexpr size_t kMinOrderShift = 4;
constexpr size_t kMinOrderSize = 1 << kMinOrderShift;  // 16 bytes

// The total number of orders in the buddy allocator.
constexpr size_t kNumOrders = 8;

// `kEmptyStringSlotIndex` a special value semantically representing
// an empty string in a string array. It is used in place of a string reference
// index, and is read as `""`.
constexpr uint64_t kEmptyStringSlotIndex = 0;

// The size of the maximum order.
constexpr size_t kMaxOrderShift = kMinOrderShift + kNumOrders - 1;
constexpr size_t kMaxOrderSize = 1 << kMaxOrderShift;

// The minimum size for the inspection VMO.
constexpr size_t kMinVmoSize = 4096;
static_assert(kMinVmoSize >= kMaxOrderSize, "Maximum order size must fit in the smallest VMO");

// The maximum size for the inspection VMO.
constexpr size_t kMaxVmoSize = 128L * 1024L * 1024L;
static_assert(kMaxVmoSize >= kMinVmoSize,
              "Maximum VMO size must be greater or equal to minimum VMO size");

// The magic number for verifying the VMO format.
constexpr char kMagicNumber[5] = "INSP";

// The version of Inspect Format we support.
constexpr size_t kVersion = 2;

constexpr size_t kVmoFrozen = 0xFFFFFFFFFFFFFFFE;

// The order of Inspect VMO header.
constexpr size_t kVmoHeaderOrder = 1;

// The size of Inspect VMO header block.
constexpr size_t kVmoHeaderBlockSize = kMinOrderSize * 2;

template <typename T>
constexpr size_t OrderToSize(T order) {
  return kMinOrderSize << order;
}

constexpr size_t IndexForOffset(size_t offset) { return offset / kMinOrderSize; }

}  // namespace internal
}  // namespace inspect

#endif  // LIB_INSPECT_CPP_VMO_LIMITS_H_
