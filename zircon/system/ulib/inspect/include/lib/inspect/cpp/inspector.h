// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_INSPECT_CPP_INSPECTOR_H_
#define LIB_INSPECT_CPP_INSPECTOR_H_

#include <lib/fpromise/result.h>
#include <lib/inspect/cpp/vmo/types.h>
#include <lib/zx/vmo.h>

#include <mutex>
#include <string>

namespace inspect {

class Inspector;

namespace internal {
class State;

// Internal accessor for obtaining fields from an Inspector.
std::shared_ptr<State> GetState(const Inspector* inspector);

}  // namespace internal

// Settings to configure a specific Inspector.
struct InspectSettings final {
  // The maximum size of the created VMO, in bytes.
  //
  // The size must be non-zero, and it will be rounded up to the next page size.
  size_t maximum_size;
};

// Stats about an inspector.
struct InspectStats final {
  // The current number of bytes to store Inspect data.
  size_t size;

  // The maximum number of bytes that can be used to store Inspect data.
  size_t maximum_size;

  // The number of dynamic children linked to an Inspector.
  size_t dynamic_child_count;

  // The number of blocks allocated over the lifetime of the inspector.
  size_t allocated_blocks;

  // The number of blocks deallocated over the lifetime of the inspector.
  size_t deallocated_blocks;

  // The number of failed allocations over the lifetime of the inspector.
  size_t failed_allocations;
};

// The entry point into the Inspection API.
//
// An Inspector wraps a particular tree of Inspect data.
//
// This class is thread safe and copyable.
class Inspector final {
 public:
  // Construct a new Inspector.
  Inspector();

  // Construct a new Inspector with the given settings.
  explicit Inspector(const InspectSettings& settings);

  // Construct a new Inspector backed by the given VMO.
  //
  // The VMO must support ZX_RIGHT_WRITE, ZX_VM_CAN_MAP_WRITE, ZX_VM_CAN_MAP_READ
  // permissions, and must be exclusively written to via the constructed Inspector.
  //
  // If an invalid VMO is passed all Node operations will have no effect.
  explicit Inspector(zx::vmo vmo);

  // Returns a duplicated read-only version of the VMO backing this inspector.
  zx::vmo DuplicateVmo() const;

  // Returns a read-only, page-by-page copy-on-write duplicate of the backing VMO.
  std::optional<zx::vmo> FrozenVmoCopy() const;

  // Returns a copied version of the VMO backing this inspector.
  //
  // The returned copy will always be a consistent snapshot of the inspector state, truncated to
  // include only relevant pages from the underlying VMO.
  std::optional<zx::vmo> CopyVmo() const;

  // Returns a copy of the bytes of the VMO backing this inspector.
  //
  // The returned bytes will always be a consistent snapshot of the inspector state, truncated to
  // include only relevant bytes from the underlying VMO.
  std::optional<std::vector<uint8_t>> CopyBytes() const;

  // Returns stats about this Inspector.
  InspectStats GetStats() const;

  // Returns a reference to the root node owned by this inspector.
  Node& GetRoot() const;

  // Adds a lazy node to this Inspector that will collect stats data about this
  // Inspector when accessed.
  void CreateStatsNode();

  // Boolean value of an Inspector is whether it is actually backed by a VMO.
  //
  // This method returns false if and only if Node operations on the Inspector are no-ops.
  explicit operator bool() const { return state_ != nullptr; }

  // Emplace a value to be owned by this Inspector.
  template <typename T>
  void emplace(T value) {
    std::lock_guard<std::mutex> guard(*value_mutex_);
    value_list_->emplace(std::move(value));
  }

  // Clear the recorded values owned by this Inspector.
  void ClearRecorded() {
    std::lock_guard<std::mutex> guard(*value_mutex_);
    value_list_->clear();
  }

  // Gets the names of the inspectors linked off of this inspector.
  std::vector<std::string> GetChildNames() const;

  // Open a child of this inspector by name.
  //
  // Returns a promise for the opened inspector.
  fpromise::promise<Inspector> OpenChild(const std::string& name) const;

  // Execute |callback| under a single lock of the Inspect VMO.
  //
  // This callback receives a reference to the root of the inspect hierarchy.
  void AtomicUpdate(AtomicUpdateCallbackFn callback);

 private:
  friend std::shared_ptr<internal::State> internal::GetState(const Inspector* inspector);

  // The root node for the Inspector.
  //
  // Shared pointers are used so Inspector is copyable.
  std::shared_ptr<Node> root_;

  // The internal state for this inspector.
  //
  // Shared pointers are used so Inspector is copyable.
  std::shared_ptr<internal::State> state_;

  // Internally stored values owned by this Inspector.
  //
  // Shared pointers are used so Inspector is copyable.
  std::shared_ptr<ValueList> value_list_;

  // Mutex for the value list.
  std::shared_ptr<std::mutex> value_mutex_;
};

}  // namespace inspect

#endif  // LIB_INSPECT_CPP_INSPECTOR_H_
