// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2016, Google, Inc. All rights reserved
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <debug.h>
#include <inttypes.h>
#include <platform.h>
#include <string.h>
#include <trace.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <dev/interrupt.h>
#include <dev/pci_config.h>
#include <dev/pcie_bridge.h>
#include <fbl/alloc_checker.h>
#include <kernel/spinlock.h>
#include <vm/vm.h>

#define LOCAL_TRACE 0

PcieBridge::PcieBridge(PcieBusDriver& bus_drv, uint bus_id, uint dev_id, uint func_id, uint mbus_id)
    : PcieDevice(bus_drv, bus_id, dev_id, func_id, true),
      PcieUpstreamNode(bus_drv, PcieUpstreamNode::Type::BRIDGE, mbus_id) {
  /* Assign the driver-wide region pool to this bridge's allocators. */
  DEBUG_ASSERT(driver().region_bookkeeping() != nullptr);
  pf_mmio_regions_.SetRegionPool(driver().region_bookkeeping());
  mmio_lo_regions_.SetRegionPool(driver().region_bookkeeping());
  mmio_hi_regions_.SetRegionPool(driver().region_bookkeeping());
  pio_regions_.SetRegionPool(driver().region_bookkeeping());
}

fbl::RefPtr<PcieDevice> PcieBridge::Create(PcieUpstreamNode& upstream, uint dev_id, uint func_id,
                                           uint managed_bus_id) {
  fbl::AllocChecker ac;
  auto raw_bridge = new (&ac)
      PcieBridge(upstream.driver(), upstream.managed_bus_id(), dev_id, func_id, managed_bus_id);
  if (!ac.check()) {
    DEBUG_ASSERT(raw_bridge == nullptr);
    TRACEF("Out of memory attemping to create PCIe bridge %02x:%02x.%01x.\n",
           upstream.managed_bus_id(), dev_id, func_id);
    return nullptr;
  }

  auto bridge = fbl::AdoptRef(static_cast<PcieDevice*>(raw_bridge));
  zx_status_t res = raw_bridge->Init(upstream);
  if (res != ZX_OK) {
    TRACEF("Failed to initialize PCIe bridge %02x:%02x.%01x. (res %d)\n", upstream.managed_bus_id(),
           dev_id, func_id, res);
    return nullptr;
  }

  return bridge;
}

zx_status_t PcieBridge::Init(PcieUpstreamNode& upstream) {
  Guard<Mutex> guard{&bridge_lock_};
  zx_status_t res;
  // Initialize the device portion of ourselves first.
  {
    Guard<Mutex> _{dev_lock()};
    res = PcieDevice::InitLocked(upstream);
    if (res != ZX_OK) {
      return res;
    }
  }

  // Sanity checks of bus allocation.
  //
  // TODO(johngro) : Strengthen sanity checks around bridge topology and
  // handle the need to reconfigure bridge topology if a bridge happens to be
  // misconfigured.  Right now, we just assume that the BIOS/Bootloader has
  // taken care of bridge configuration.  In the short term, it would be good
  // to add some protection against cycles in the bridge configuration which
  // could lead to infinite recursion.
  uint primary_id = cfg_->Read(PciConfig::kPrimaryBusId);
  uint secondary_id = cfg_->Read(PciConfig::kSecondaryBusId);

  if (primary_id == secondary_id) {
    TRACEF(
        "PCI-to-PCI bridge detected at %02x:%02x.%01x claims to be bridged to itsef "
        "(primary %02x == secondary %02x)... skipping scan.\n",
        bus_id_, dev_id_, func_id_, primary_id, secondary_id);
    return ZX_ERR_BAD_STATE;
  }

  if (primary_id != bus_id_) {
    TRACEF(
        "PCI-to-PCI bridge detected at %02x:%02x.%01x has invalid primary bus id "
        "(%02x)... skipping scan.\n",
        bus_id_, dev_id_, func_id_, primary_id);
    return ZX_ERR_BAD_STATE;
  }

  if (secondary_id != managed_bus_id()) {
    TRACEF(
        "PCI-to-PCI bridge detected at %02x:%02x.%01x has invalid secondary bus id "
        "(%02x)... skipping scan.\n",
        bus_id_, dev_id_, func_id_, secondary_id);
    return ZX_ERR_BAD_STATE;
  }

  // Parse the state of its I/O and Memory windows.
  res = ParseBusWindowsLocked();
  if (res != ZX_OK) {
    return res;
  }

  // Things went well, flag the device as plugged in and link ourselves up to
  // the graph.
  plugged_in_ = true;
  driver().LinkDeviceToUpstream(*this, upstream);
  // Release the device lock, then recurse and scan for downstream devices.
  ScanDownstream();
  return ZX_OK;
}

zx_status_t PcieBridge::EnableBusMasterUpstream(bool enabled) {
  // If being asked to disable Bus Mastering then we should ensure that no other
  // devices downstream of this bridge still have it enabled. If any do then we
  // leave BusMastering enabled.
  Guard<Mutex> guard{&bridge_lock_};
  if (enabled) {
    downstream_bus_mastering_cnt_++;
  } else {
    if (downstream_bus_mastering_cnt_ == 0) {
      return ZX_ERR_BAD_STATE;
    }
    downstream_bus_mastering_cnt_--;
  }

  LTRACEF("UpstreamNode bm cnt: %zu\n", downstream_bus_mastering_cnt_);
  // Only make a change to the bridge's bus mastering configuration in a case
  // where the state of the children has changed meaningfully.
  if (downstream_bus_mastering_cnt_ == 0) {
    LTRACEF("Disabling BusMastering\n");
    return PcieDevice::EnableBusMaster(false);
  }

  if (downstream_bus_mastering_cnt_ == 1 && enabled) {
    LTRACEF("Enabling BusMastering\n");
    return PcieDevice::EnableBusMaster(true);
  }

  return ZX_OK;
}

zx_status_t PcieBridge::ParseBusWindowsLocked() {
  // Parse the currently configured windows used to determine MMIO/PIO
  // forwarding policy for this bridge.
  //
  // See The PCI-to-PCI Bridge Architecture Specification Revision 1.2,
  // section 3.2.5 and chapter 4 for detail.
  uint32_t base, limit;

  // I/O window
  base = cfg_->Read(PciConfig::kIoBase);
  limit = cfg_->Read(PciConfig::kIoLimit);

  supports_32bit_pio_ = ((base & 0xF) == 0x1) && ((base & 0xF) == (limit & 0xF));
  io_base_ = (base & ~0xF) << 8;
  io_limit_ = (limit << 8) | 0xFFF;
  if (supports_32bit_pio_) {
    io_base_ |= static_cast<uint32_t>(cfg_->Read(PciConfig::kIoBaseUpper)) << 16;
    io_limit_ |= static_cast<uint32_t>(cfg_->Read(PciConfig::kIoLimitUpper)) << 16;
  }

  // Non-prefetchable memory window
  mem_base_ = (static_cast<uint32_t>(cfg_->Read(PciConfig::kMemoryBase)) << 16) & ~0xFFFFF;
  mem_limit_ = (static_cast<uint32_t>(cfg_->Read(PciConfig::kMemoryLimit)) << 16) | 0xFFFFF;

  // Prefetchable memory window
  base = cfg_->Read(PciConfig::kPrefetchableMemoryBase);
  limit = cfg_->Read(PciConfig::kPrefetchableMemoryLimit);

  bool supports_64bit_pf_mem = ((base & 0xF) == 0x1) && ((base & 0xF) == (limit & 0xF));
  pf_mem_base_ = (base & ~0xF) << 16;
  pf_mem_limit_ = (limit << 16) | 0xFFFFF;
  if (supports_64bit_pf_mem) {
    pf_mem_base_ |= static_cast<uint64_t>(cfg_->Read(PciConfig::kPrefetchableMemoryBaseUpper))
                    << 32;
    pf_mem_limit_ |= static_cast<uint64_t>(cfg_->Read(PciConfig::kPrefetchableMemoryLimitUpper))
                     << 32;
  }

  return ZX_OK;
}

void PcieBridge::Dump() const {
  PcieDevice::Dump();

  printf("\tbridge managed bus id %#02x\n", managed_bus_id());
  printf("\tio base %#x limit %#x\n", io_base(), io_limit());
  printf("\tmem base %#x limit %#x\n", mem_base(), mem_limit());
  printf("\tprefectable base %#" PRIx64 " limit %#" PRIx64 "\n", pf_mem_base(), pf_mem_limit());
}

void PcieBridge::Unplug() {
  PcieDevice::Unplug();
  PcieUpstreamNode::UnplugDownstream();
}

zx_status_t PcieBridge::AllocateBars() {
  {
    Guard<Mutex> guard{&bridge_lock_};

    // Start by making sure we can allocate our bridge windows.
    zx_status_t res = AllocateBridgeWindowsLocked();
    if (res != ZX_OK) {
      return res;
    }

    // Now, attempt to allocate our device BARs.
    {
      Guard<Mutex> _{dev_lock()};
      res = PcieDevice::AllocateBarsLocked();
      if (res != ZX_OK) {
        return res;
      }
    }
    // Great, we are good to go.  Leave our device lock and attempt to allocate
    // our downstream devices' resources.
  }
  PcieUpstreamNode::AllocateDownstreamBars();
  return ZX_OK;
}

zx_status_t PcieBridge::AllocateBridgeWindowsLocked() {
  zx_status_t ret;

  // Hold a reference to our upstream node while we do this.  If we cannot
  // obtain a reference, then our upstream node has become unplugged and we
  // should just fail out now.
  auto upstream = GetUpstream();
  if (upstream == nullptr) {
    return ZX_ERR_UNAVAILABLE;
  }

  // We are configuring a bridge.  We need to be able to allocate the MMIO and
  // PIO regions this bridge is configured to manage.  Currently, we don't
  // support re-allocating a bridge's MMIO/PIO windows.
  //
  // TODO(johngro) : support dynamic configuration of bridge windows.  Its
  // going to be important when we need to support hot-plugging.  See https://fxbug.dev/42105128
  //
  if (io_base_ <= io_limit_) {
    uint64_t size = static_cast<uint64_t>(io_limit_) - io_base_ + 1;
    ret = upstream->pio_regions().GetRegion({.base = io_base_, .size = size}, pio_window_);

    if (ret != ZX_OK) {
      TRACEF("Failed to allocate bridge PIO window [0x%08x, 0x%08x]\n", io_base_, io_limit_);
      return ret;
    }

    DEBUG_ASSERT(pio_window_ != nullptr);
    pio_regions().AddRegion(*pio_window_);
  }

  if (mem_base_ <= mem_limit_) {
    uint64_t size = mem_limit_ - mem_base_ + 1;
    ret = upstream->mmio_lo_regions().GetRegion({.base = mem_base_, .size = size}, mmio_window_);

    if (ret != ZX_OK) {
      TRACEF("Failed to allocate bridge MMIO window [0x%08x, 0x%08x]\n", mem_base_, mem_limit_);
      return ret;
    }

    DEBUG_ASSERT(mmio_window_ != nullptr);
    mmio_lo_regions().AddRegion(*mmio_window_);
  }

  if (pf_mem_base_ <= pf_mem_limit_) {
    uint64_t size = pf_mem_limit_ - pf_mem_base_ + 1;

    // Attempt to allocate out of the upstream's prefetchable region.
    ret = upstream->pf_mmio_regions().GetRegion({.base = pf_mem_base_, .size = size},
                                                pf_mmio_window_);
    if (ret != ZX_OK) {
      // We failed. If it's the root bridge try to allocate from its MMIO regions.
      if (upstream->type() == PcieUpstreamNode::Type::ROOT) {
        ret = upstream->mmio_lo_regions().GetRegion({.base = pf_mem_base_, .size = size},
                                                    pf_mmio_window_);
        if (ret != ZX_OK) {
          ret = upstream->mmio_hi_regions().GetRegion({.base = pf_mem_base_, .size = size},
                                                      pf_mmio_window_);
        }
      }
    }

    if (ret != ZX_OK) {
      TRACEF(
          "Failed to allocate bridge prefetcable MMIO window "
          "[%#" PRIx64 ", %#" PRIx64 "]\n",
          pf_mem_base_, pf_mem_limit_);
      return ret;
    }

    DEBUG_ASSERT(pf_mmio_window_ != nullptr);
    pf_mmio_regions().AddRegion(*pf_mmio_window_);
  }

  return ZX_OK;
}

void PcieBridge::Disable() {
  // Immediately enter the device lock and enter the disabled state.  We want
  // to be outside of the device lock as we disable our downstream devices,
  // but we don't want any new devices to be able to plug into us as we do so.
  {
    Guard<Mutex> guard{&dev_lock_};
    disabled_ = true;

    // Start by disabling all of our downstream devices.  This should prevent
    // the from bothering us moving forward.  Do not hold the device lock while
    // we do this.
    guard.CallUnlocked([this]() { PcieUpstreamNode::DisableDownstream(); });

    // Enter the device lock again and finish shooting ourselves in the head.

    // Disable the device portion of ourselves.
    PcieDevice::DisableLocked();

    // Close all of our IO windows at the HW level and update the internal
    // bookkeeping to indicate that they are closed.
    cfg_->Write(PciConfig::kIoBase, 0xF0);
    cfg_->Write(PciConfig::kIoLimit, 0);
    cfg_->Write(PciConfig::kIoBaseUpper, 0);
    cfg_->Write(PciConfig::kIoLimitUpper, 0);

    cfg_->Write(PciConfig::kMemoryBase, 0xFFF0);
    cfg_->Write(PciConfig::kMemoryLimit, 0);

    cfg_->Write(PciConfig::kPrefetchableMemoryBase, 0xFFF0);
    cfg_->Write(PciConfig::kPrefetchableMemoryLimit, 0);
    cfg_->Write(PciConfig::kPrefetchableMemoryBaseUpper, 0);
    cfg_->Write(PciConfig::kPrefetchableMemoryLimitUpper, 0);
  }

  Guard<Mutex> _{&bridge_lock_};
  pf_mem_limit_ = mem_limit_ = io_limit_ = 0u;
  pf_mem_base_ = mem_base_ = io_base_ = 1u;

  // Release our internal bookkeeping
  mmio_lo_regions().Reset();
  mmio_hi_regions().Reset();
  pio_regions().Reset();

  mmio_window_.reset();
  pio_window_.reset();
}
