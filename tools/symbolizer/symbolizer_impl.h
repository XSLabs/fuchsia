// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef TOOLS_SYMBOLIZER_SYMBOLIZER_IMPL_H_
#define TOOLS_SYMBOLIZER_SYMBOLIZER_IMPL_H_

#include <cstdint>
#include <iostream>
#include <memory>
#include <string_view>
#include <unordered_map>

#include <rapidjson/document.h>

#include "src/developer/debug/shared/message_loop_poll.h"
#include "src/developer/debug/zxdb/client/download_observer.h"
#include "src/developer/debug/zxdb/client/pretty_stack_manager.h"
#include "src/developer/debug/zxdb/client/process_observer.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/source_file_provider_impl.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/system_observer.h"
#include "src/developer/debug/zxdb/symbols/module_symbols.h"
#include "tools/symbolizer/analytics.h"
#include "tools/symbolizer/command_line_options.h"
#include "tools/symbolizer/symbolizer.h"

namespace symbolizer {

// This is the core logic of the symbolizer. We provide a MockSymbolizer and a SymbolizerImpl for
// better testing.
class SymbolizerImpl : public Symbolizer,
                       public zxdb::DownloadObserver,
                       public zxdb::ProcessObserver,
                       public zxdb::SystemObserver {
 public:
  explicit SymbolizerImpl(const CommandLineOptions& options);
  ~SymbolizerImpl() override;

  struct ModuleInfo {
    std::string name;
    std::string build_id;
    uint64_t base = 0;  // Load address of the module.
    uint64_t size = 0;  // Range of the module.

    // Zircon on x64 has a negative base address, i.e. the module offset is larger than the load
    // address. Since zxdb doesn't support that, we load the module at 0 and modify the pc for all
    // frames.
    //
    // At least one of the base and the negative_base must be zero.
    uint64_t negative_base = 0;
    bool printed = false;  // Whether we've printed the module info.
  };

  enum class MMapStatus : uint8_t {
    // No problems were encountered.
    kOk,
    // The module ID was invalid and no updates were made.
    kInvalidModuleId,
    // The mapping was recorded but the base address was inconsistent with the provided module.
    kInconsistentBaseAddress,
  };

  enum class BacktraceStatus : uint8_t {
    // No problems were encountered.
    kOk,
    // The corresponding symbol file is not available.
    kSymbolFileUnavailable,
    // The requested address is not covered by any mapping.
    kNoOverlappingModule,
  };

  // Provides location information as a callback, along with its offset within a frame.
  using LocationOutputFn = fit::function<void(size_t, const zxdb::Location&, const ModuleInfo&)>;

  // Methods which allow C++ callers to directly symbolize addresses without relying on string
  // outputs.
  MMapStatus MMap(uint64_t address, uint64_t size, uint64_t module_id, std::string_view flags,
                  uint64_t module_offset);
  BacktraceStatus Backtrace(uint64_t address, AddressType type, LocationOutputFn output);

  // |Symbolizer| implementation.
  void Reset(bool symbolizing_dart, ResetType type) override;
  void Module(uint64_t id, std::string_view name, std::string_view build_id) override;
  void MMap(uint64_t address, uint64_t size, uint64_t module_id, std::string_view flags,
            uint64_t module_offset, StringOutputFn output) override;
  void Backtrace(uint64_t frame_id, uint64_t address, AddressType type, std::string_view message,
                 StringOutputFn output) override;
  void DumpFile(std::string_view type, std::string_view name) override;

  // |DownloadObserver| implementation.
  void OnDownloadsStarted() override;
  void OnDownloadsStopped(size_t num_succeeded, size_t num_failed) override;

  // |SystemObserver| implementation.
  void DidCreateSymbolServer(zxdb::SymbolServer* server) override;
  void OnSymbolServerStatusChanged(zxdb::SymbolServer* server) override;

  // |ProcessObserver| implementation.
  void DidCreateProcess(zxdb::Process* process, uint64_t timestamp) override;
  void WillDestroyProcess(zxdb::Process* process, DestroyReason reason, int exit_code,
                          uint64_t timestamp) override;
  void WillLoadModuleSymbols(zxdb::Process* process, int num_modules) override;
  void DidLoadModuleSymbols(zxdb::Process* process, zxdb::LoadedModuleSymbols* module) override;
  void DidLoadAllModuleSymbols(zxdb::Process* process) override;
  void WillUnloadModuleSymbols(zxdb::Process* process, zxdb::LoadedModuleSymbols* module) override;
  void OnSymbolLoadFailure(zxdb::Process* process, const zxdb::Err& err) override;

 private:
  // Ensures a process is created on target_. Should be called before each Bactrace().
  void InitProcess();

  // Resets dumpfile_current_object_.
  void ResetDumpfileCurrentObject();

  // Output the backtrace in batch mode.
  void OutputBatchedBacktrace();

  // If we receive invalid markup, we need to flush all of the buffered stack frames in
  // |frames_in_batch_mode_|, which must be destructed in the same order they were constructed. The
  // rest of the associated frames from this backtrace will not be symbolized.
  // |context| will be logged to stderr as a warning.
  void FlushBufferedFramesWithContext(const std::string& context);

  // Helper to convert a string_view to a rapidjson string.
  rapidjson::Value ToJSONString(std::string_view str);

  // Whether prettify is enabled.
  bool prettify_enabled_ = false;

  // The main message loop.
  debug::MessageLoopPoll loop_;

  // The entry for interacting with zxdb.
  zxdb::Session session_;

  // Owned by session_. Holds the process we're working on.
  zxdb::Target* target_;

  // Whether there are symbol servers and we're waiting for authentication.
  bool waiting_auth_ = false;

  // Whether there are symbol downloads in progress.
  bool is_downloading_ = false;

  // Mapping from module_id (available in the log) to module info.
  //
  // module_id is usually a sequence from 0 used to associate "mmap" commands with "module"
  // commands. It's different from build_id.
  std::unordered_map<uint64_t, ModuleInfo> modules_;

  // Holds symbol data from the previously handled stack trace.
  // Replaced immediately once a new stack trace is handled.
  std::vector<fxl::RefPtr<zxdb::ModuleSymbols>> previous_modules_;

  // Mapping from base address of each module to the module_id.
  // Useful when doing binary search for the module from an address.
  std::map<uint64_t, uint64_t> address_to_module_id_;

  // Whether to omit the [[[ELF module]]] lines.
  bool omit_module_lines_ = false;

  // The JSON file to write the dumpfile output. If it's empty then nothing will be written and
  // dumpfile_array_ and dumpfile_current_object_ will be useless.
  std::string dumpfile_output_;
  // The JSON document/array that holds the dumpfile output. The content will be written to
  // dumpfile_output_ when we destruct.
  rapidjson::Document dumpfile_document_;
  // Object that will be appended to dumpfile_output_array_ on the next DumpFile(). It'll be like {
  //   "modules": [
  //     {
  //       "name": "libsyslog.so",
  //       "build": "3552581785f71a08",
  //       "id": 7
  //     },
  //     ...
  //   ],
  //   "segments": [
  //     {
  //       "mod": 0,
  //       "vaddr": 38628535922688,
  //       "size": 61440,
  //       "flags": "r",
  //       "mod_rel_addr": 0
  //     },
  //     ...
  //   ],
  // }.
  // Each Module() will be appended to the modules array and each MMap() will be appended to the
  // segments array.  We're keeping a separate copy of those info because
  // 1) not all mmap info is kept in ModuleInfo.
  // 2) the dumpfile feature might be removed in the future.
  rapidjson::Value dumpfile_current_object_;

  // Analytics. Instead of keeping a unique_ptr, we depends on the valid() method to know if
  // the analytics is not empty and worth sending.
  SymbolizationAnalyticsBuilder analytics_builder_;
  bool remote_symbol_lookup_enabled_ = false;

  // Whether we're symbolizing a Dart stack trace.
  bool symbolizing_dart_ = false;

  // These are used to prettify backtraces and require initialization.
  fxl::RefPtr<zxdb::PrettyStackManager> pretty_stack_manager_;
  std::unique_ptr<zxdb::SourceFileProviderImpl> source_file_provider_;

  // Whether we're processing in batch mode. The batch mode is triggered by {{{reset:begin}}} and
  // will cause all the inputs to be cached so that multi-line optimization could be performed.
  bool in_batch_mode_ = false;

  // The frames cached if we're in batch mode.
  struct Frame {
    uint64_t address;
    AddressType type;
    StringOutputFn output;
  };
  std::deque<Frame> frames_in_batch_mode_;
};

}  // namespace symbolizer

#endif  // TOOLS_SYMBOLIZER_SYMBOLIZER_IMPL_H_
