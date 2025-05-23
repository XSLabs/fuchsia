// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BUS_LIB_VIRTIO_INCLUDE_LIB_VIRTIO_BACKENDS_PCI_H_
#define SRC_DEVICES_BUS_LIB_VIRTIO_INCLUDE_LIB_VIRTIO_BACKENDS_PCI_H_

#include <fidl/fuchsia.hardware.pci/cpp/fidl.h>
#include <lib/mmio/mmio.h>
#include <lib/virtio/backends/backend.h>
#include <lib/zx/port.h>
#include <zircon/compiler.h>

#include <mutex>
#include <optional>

#include <fbl/macros.h>

namespace virtio {

class PciBackend : public Backend {
 public:
  PciBackend(fidl::ClientEnd<fuchsia_hardware_pci::Device> pci,
             fuchsia_hardware_pci::DeviceInfo info);
  zx_status_t Bind() final;
  virtual zx_status_t Init() = 0;
  const char* tag() { return tag_; }

  zx_status_t ConfigureInterruptMode();
  zx::result<uint32_t> WaitForInterrupt() final;
  void InterruptAck(uint32_t key) final;

  // Virtio spec 4.1.5.1.2 - MSI-X Vector Configuration
  static constexpr uint16_t kVirtioMsiNoVector = 0xFFFF;
  static constexpr uint16_t kMsiConfigVector = 0;
  static constexpr uint16_t kMsiQueueVector = 1;

 protected:
  fidl::UnownedClientEnd<fuchsia_hardware_pci::Device> pci() { return pci_.borrow(); }
  const fuchsia_hardware_pci::DeviceInfo& info() const { return info_; }
  std::mutex& lock() { return lock_; }
  zx::port& wait_port() { return wait_port_; }

 private:
  fidl::ClientEnd<fuchsia_hardware_pci::Device> pci_;
  fuchsia_hardware_pci::DeviceInfo info_;
  std::mutex lock_;
  zx::port wait_port_;
  char tag_[16];  // pci[XX:XX.X] + \0, aligned to 8
  DISALLOW_COPY_ASSIGN_AND_MOVE(PciBackend);
};

// The interface for accessing IO is abstracted out to allow for test mocking.
// Otherwise, dealing with IO instructions is difficult.
class LegacyIoInterface {
 public:
  LegacyIoInterface() = default;
  virtual ~LegacyIoInterface() = default;

  virtual void Read(uint16_t offset, uint8_t* val) const = 0;
  virtual void Read(uint16_t offset, uint16_t* val) const = 0;
  virtual void Read(uint16_t offset, uint32_t* val) const = 0;
  virtual void Write(uint16_t offset, uint8_t val) const = 0;
  virtual void Write(uint16_t offset, uint16_t val) const = 0;
  virtual void Write(uint16_t offset, uint32_t val) const = 0;
};

// "Real" virtual hardware will use this interface to access Virtio
// over IO bar 0.
class PciLegacyIoInterface : public LegacyIoInterface {
 public:
  PciLegacyIoInterface() = default;
  ~PciLegacyIoInterface() override = default;

  void Read(uint16_t offset, uint8_t* val) const override;
  void Read(uint16_t offset, uint16_t* val) const override;
  void Read(uint16_t offset, uint32_t* val) const override;
  void Write(uint16_t offset, uint8_t val) const override;
  void Write(uint16_t offset, uint16_t val) const override;
  void Write(uint16_t offset, uint32_t val) const override;

  static PciLegacyIoInterface* Get() {
    static PciLegacyIoInterface interface{};
    return &interface;
  }
};

// PciLegacyBackend corresponds to the Virtio Legacy interface utilizing port IO
// and the IO Bar 0. It has additional complications around offsets and
// configuration structures when MSI-X is enabled.
class PciLegacyBackend : public PciBackend {
 public:
  PciLegacyBackend(fidl::ClientEnd<fuchsia_hardware_pci::Device> pci,
                   fuchsia_hardware_pci::DeviceInfo info)
      : PciBackend(std::move(pci), std::move(info)), legacy_io_(PciLegacyIoInterface::Get()) {}
  PciLegacyBackend(fidl::ClientEnd<fuchsia_hardware_pci::Device> pci,
                   fuchsia_hardware_pci::DeviceInfo info, LegacyIoInterface* interface)
      : PciBackend(std::move(pci), std::move(info)), legacy_io_(interface) {}
  PciLegacyBackend(const PciLegacyBackend&) = delete;
  PciLegacyBackend& operator=(const PciLegacyBackend&) = delete;
  ~PciLegacyBackend() override = default;

  zx_status_t Init() final;

  void DriverStatusOk() final;
  void DriverStatusAck() final;
  void DeviceReset() final;
  void WaitForDeviceReset() final;
  uint32_t IsrStatus() final;
  uint64_t ReadFeatures() final;
  void SetFeatures(uint64_t bitmap) final;
  zx_status_t ConfirmFeatures() final;

  // These handle reading and writing a device's device config to allow derived
  // virtio devices to work with fields only they know about. For most virtio
  // devices they will have their device config copied over via
  // CopyDeviceConfig when device config interrupts are asserted and will not
  // need to call these directly.
  void ReadDeviceConfig(uint16_t offset, uint8_t* value) final __TA_EXCLUDES(lock());
  void ReadDeviceConfig(uint16_t offset, uint16_t* value) final __TA_EXCLUDES(lock());
  void ReadDeviceConfig(uint16_t offset, uint32_t* value) final __TA_EXCLUDES(lock());
  void ReadDeviceConfig(uint16_t offset, uint64_t* value) final __TA_EXCLUDES(lock());
  void WriteDeviceConfig(uint16_t offset, uint8_t value) final __TA_EXCLUDES(lock());
  void WriteDeviceConfig(uint16_t offset, uint16_t value) final __TA_EXCLUDES(lock());
  void WriteDeviceConfig(uint16_t offset, uint32_t value) final __TA_EXCLUDES(lock());
  void WriteDeviceConfig(uint16_t offset, uint64_t value) final __TA_EXCLUDES(lock());

  // Handle the virtio queues for the device. Due to configuration layouts changing
  // depending on backend this has to be handled by the backend itself.
  uint16_t GetRingSize(uint16_t index) final;
  zx_status_t SetRing(uint16_t index, uint16_t count, zx_paddr_t pa_desc, zx_paddr_t pa_avail,
                      zx_paddr_t pa_used) final;
  void RingKick(uint16_t ring_index) final;

 private:
  void SetStatusBits(uint8_t bits);
  uint16_t bar0_base_ __TA_GUARDED(lock());
  uint16_t device_cfg_offset_ __TA_GUARDED(lock());
  const LegacyIoInterface* legacy_io_ __TA_GUARDED(lock());
};

// PciModernBackend is for v1.0+ Virtio using MMIO mapped bars and PCI capabilities.
class PciModernBackend : public PciBackend {
 public:
  PciModernBackend(fidl::ClientEnd<fuchsia_hardware_pci::Device> pci,
                   fuchsia_hardware_pci::DeviceInfo info)
      : PciBackend(std::move(pci), std::move(info)) {}
  // The dtor handles cleanup of allocated bars because we cannot tear down
  // the mappings safely while the virtio device is being used by a driver.
  ~PciModernBackend() override = default;
  zx_status_t Init() final;

  void DriverStatusOk() final;
  void DriverStatusAck() final;
  void DeviceReset() final;
  void WaitForDeviceReset() final;
  uint32_t IsrStatus() final;
  uint64_t ReadFeatures() final;
  void SetFeatures(uint64_t bitmap) final;
  zx_status_t ConfirmFeatures() final;
  zx_status_t ReadVirtioCap(uint8_t offset, virtio_pci_cap* cap);
  zx_status_t ReadVirtioCap64(uint8_t cap_config_offset, virtio_pci_cap& cap,
                              virtio_pci_cap64* cap64_out);

  zx_status_t GetSharedMemoryVmo(zx::vmo* vmo_out) override;

  // These handle writing to/from a device's device config to allow derived
  // virtio devices to work with fields only they know about.
  void ReadDeviceConfig(uint16_t offset, uint8_t* value) final __TA_EXCLUDES(lock());
  void ReadDeviceConfig(uint16_t offset, uint16_t* value) final __TA_EXCLUDES(lock());
  void ReadDeviceConfig(uint16_t offset, uint32_t* value) final __TA_EXCLUDES(lock());
  void ReadDeviceConfig(uint16_t offset, uint64_t* value) final __TA_EXCLUDES(lock());
  void WriteDeviceConfig(uint16_t offset, uint8_t value) final __TA_EXCLUDES(lock());
  void WriteDeviceConfig(uint16_t offset, uint16_t value) final __TA_EXCLUDES(lock());
  void WriteDeviceConfig(uint16_t offset, uint32_t value) final __TA_EXCLUDES(lock());
  void WriteDeviceConfig(uint16_t offset, uint64_t value) final __TA_EXCLUDES(lock());

  // Callbacks called during PciBackend's parsing of capabilities in Bind()
  void CommonCfgCallbackLocked(const virtio_pci_cap_t& cap) __TA_REQUIRES(lock());
  void NotifyCfgCallbackLocked(const virtio_pci_cap_t& cap) __TA_REQUIRES(lock());
  void IsrCfgCallbackLocked(const virtio_pci_cap_t& cap) __TA_REQUIRES(lock());
  void DeviceCfgCallbackLocked(const virtio_pci_cap_t& cap) __TA_REQUIRES(lock());
  void PciCfgCallbackLocked(const virtio_pci_cap_t& cap) __TA_REQUIRES(lock());
  void SharedMemoryCfgCallbackLocked(const virtio_pci_cap_t& cap, uint64_t offset, uint64_t length)
      __TA_REQUIRES(lock());

  // Handle the virtio queues for the device. Due to configuration layouts changing
  // depending on backend this has to be handled by the backend itself.
  uint16_t GetRingSize(uint16_t index) final;
  zx_status_t SetRing(uint16_t index, uint16_t count, zx_paddr_t pa_desc, zx_paddr_t pa_avail,
                      zx_paddr_t pa_used) final;
  void RingKick(uint16_t ring_index) final;

 private:
  zx_status_t GetBarVmo(uint8_t bar, zx::vmo* vmo_out);
  zx_status_t MapBar(uint8_t bar);

  std::optional<fdf::MmioBuffer> bar_[6];

  uintptr_t notify_base_ = 0;
  volatile uint32_t* isr_status_ = nullptr;
  uintptr_t device_cfg_ __TA_GUARDED(lock()) = 0;
  MMIO_PTR volatile virtio_pci_common_cfg_t* common_cfg_ __TA_GUARDED(lock()) = nullptr;
  uint32_t notify_off_mul_;
  std::optional<uint8_t> shared_memory_bar_;

  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(PciModernBackend);
};

}  // namespace virtio

#endif  // SRC_DEVICES_BUS_LIB_VIRTIO_INCLUDE_LIB_VIRTIO_BACKENDS_PCI_H_
