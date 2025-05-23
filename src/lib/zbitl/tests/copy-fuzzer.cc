// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zbi-format/zbi.h>
#include <lib/zbitl/error-string.h>
#include <lib/zbitl/view.h>
#include <zircon/assert.h>

#include <cstddef>
#include <iterator>
#include <span>
#include <string>

#include <fbl/array.h>
#include <fuzzer/FuzzedDataProvider.h>

#include "traits.h"

namespace {

// This value subdivides the following enum value space: values greater or equal
// to this one correspond strictly to copy-creation codepaths.
constexpr uint8_t kMinCreationCopyCodepath = 0xf;

// Corresponds to a particular `Copy` `View` method. See usage below for the
// exact mapping.
enum class CopyCodepath : uint8_t {
  kDirectRawItem,
  kDirectRawItemWithHeader,
  kDirectStorageItem,
  kDirectIteratorRange,
  kDirectByteRange,

  //
  // Only copy-creation codepaths past this point (with the exception of
  // kMaxValue).
  //
  kCreationRawItem = kMinCreationCopyCodepath,
  kCreationRawItemWithHeader,
  kCreationStorageItem,
  kCreationIteratorRange,
  kCreationByteRange,

  kMaxValue,  // Required by FuzzedDataProvider::ConsumeEnum().
};

constexpr bool IsCreationCopyCodepath(CopyCodepath codepath) {
  return static_cast<uint8_t>(codepath) >= kMinCreationCopyCodepath;
}

template <typename Storage>
int Fuzz(FuzzedDataProvider& provider) {
  using Traits = FuzzTraits<Storage>;

  // Compute some parameters up front (not necessarily used), so that we can
  // consume the rest of the data to create the ZBI.
  const auto codepath = provider.ConsumeEnum<CopyCodepath>();
  const auto from_offset = provider.ConsumeIntegral<uint32_t>();
  const auto from_length = provider.ConsumeIntegral<uint32_t>();
  const auto to_offset = provider.ConsumeIntegralInRange<uint32_t>(0, Traits::kRoughSizeMax);

  std::string zbi = provider.ConsumeRemainingBytesAsString();
  auto storage = Traits::FromString(zbi);
  zbitl::View view(std::move(storage));

  // Storage destination (only used in the kDirect* codepaths).
  std::unique_ptr<std::byte[]> buff(new std::byte[zbi.size()]);
  std::span<std::byte> to{buff.get(), zbi.size()};

  // These two codepaths are per-view (and not per-iterator) and should not
  // affect the view's internal error state.
  if (codepath == CopyCodepath::kDirectByteRange) {
    static_cast<void>(view.Copy(to, from_offset, from_length, to_offset));
    return 0;
  }
  if (codepath == CopyCodepath::kCreationByteRange) {
    static_cast<void>(view.Copy(from_offset, from_length, to_offset));
    return 0;
  }

  for (auto it = view.begin(); it != view.end(); ++it) {
    // If we are dealing with a copy-creation codepath with a pathological
    // header length, skip the copy altogether to avoid a potentially excessive
    // allocation of that same size. In contrast, the direct copy codepaths can
    // gracefully fail in such cases without any allocation side-effects.
    if (IsCreationCopyCodepath(codepath) &&
        it->header->length > view.size_bytes() - it.item_offset()) {
      continue;
    }

    switch (codepath) {
      case CopyCodepath::kDirectRawItem:
        static_cast<void>(view.CopyRawItem(to, it));
        break;
      case CopyCodepath::kDirectRawItemWithHeader:
        static_cast<void>(view.CopyRawItemWithHeader(to, it));
        break;
      case CopyCodepath::kDirectStorageItem:
        static_cast<void>(view.CopyStorageItem(to, it));
        break;
      case CopyCodepath::kDirectIteratorRange:
        static_cast<void>(view.Copy(to, view.begin(), it));
        static_cast<void>(view.Copy(to, it, view.end()));
        break;
      case CopyCodepath::kCreationRawItem:
        static_cast<void>(view.CopyRawItem(it));
        break;
      case CopyCodepath::kCreationRawItemWithHeader:
        static_cast<void>(view.CopyRawItemWithHeader(it));
        break;
      case CopyCodepath::kCreationStorageItem:
        static_cast<void>(view.CopyStorageItem(it));
        break;
      case CopyCodepath::kCreationIteratorRange:
        static_cast<void>(view.Copy(view.begin(), it));
        static_cast<void>(view.Copy(it, view.end()));
        break;
      case CopyCodepath::kMaxValue:  // Just a placeholder.
        break;
      case CopyCodepath::kDirectByteRange:
      case CopyCodepath::kCreationByteRange:
        ZX_ASSERT_MSG(false, "byte range codepaths should have been handled separately");
    };
  }

  view.ignore_error();
  return 0;
}

}  // namespace

extern "C" int LLVMFuzzerTestOneInput(const uint8_t* data, size_t size) {
  FuzzedDataProvider provider(data, size);

  switch (provider.ConsumeEnum<StorageType>()) {
    case StorageType::kFblByteArray:
      return Fuzz<fbl::Array<std::byte>>(provider);
#ifdef __Fuchsia__
    case StorageType::kVmo:
      return Fuzz<zx::vmo>(provider);
#endif
    case StorageType::kMaxValue:  // Placeholder.
      return 0;
  };
}
