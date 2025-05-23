// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/zbi-format/memory.h>
#include <lib/zbi-format/zbi.h>
#include <lib/zbitl/error-stdio.h>
#include <lib/zbitl/view.h>
#include <zircon/assert.h>

#include <ktl/optional.h>
#include <ktl/span.h>
#include <phys/main.h>

#include <ktl/enforce.h>

void InitMemory(const void* zbi_ptr, ktl::optional<EarlyBootZbi> zbi, AddressSpace* aspace) {
  ktl::span<zbi_mem_range_t> zbi_ranges;
  ktl::optional<memalloc::Range> nvram_range;

  ZX_DEBUG_ASSERT(zbi);

  for (auto [header, wrapped_payload] : *zbi) {
    switch (header->type) {
      case ZBI_TYPE_MEM_CONFIG: {
        ktl::span payload = wrapped_payload.get();
        zbi_ranges = {
            const_cast<zbi_mem_range_t*>(reinterpret_cast<const zbi_mem_range_t*>(payload.data())),
            payload.size_bytes() / sizeof(zbi_mem_range_t)};
        break;
      }

      case ZBI_TYPE_NVRAM: {
        ktl::span payload = wrapped_payload.get();
        ZX_ASSERT(payload.size_bytes() >= sizeof(zbi_nvram_t));
        const zbi_nvram_t* nvram = reinterpret_cast<const zbi_nvram_t*>(payload.data());
        nvram_range = {
            .addr = nvram->base,
            .size = nvram->length,
            .type = memalloc::Type::kNvram,
        };
        break;
      }
    }
  }
  if (auto result = zbi->take_error(); result.is_error()) {
    zbitl::PrintViewError(result.error_value());
    ZX_PANIC("error occurred while parsing the data ZBI");
  }

  ZX_ASSERT_MSG(!zbi_ranges.empty(), "no MEM_CONFIG item found in the data ZBI");

  ZbiInitMemory(zbi_ptr, *zbi, zbi_ranges, nvram_range, aspace);
}
