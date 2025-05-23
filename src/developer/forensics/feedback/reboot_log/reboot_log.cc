// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/reboot_log.h"

#include <lib/fit/defer.h>
#include <lib/syslog/cpp/macros.h>

#include <unordered_set>
#include <utility>

#include "src/developer/forensics/feedback/reboot_log/graceful_reboot_reason.h"
#include "src/developer/forensics/feedback/reboot_log/reboot_reason.h"
#include "src/developer/forensics/feedback_data/constants.h"
#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/fxl/strings/split_string.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/lib/fxl/strings/trim.h"

namespace forensics {
namespace feedback {
namespace {

// The kernel adds this line to indicate which process caused the root job to terminate.
//
// It can be found at
// https://osscs.corp.google.com/fuchsia/fuchsia/+/main:zircon/kernel/lib/crashlog/crashlog.cc;l=146;drc=e81b291e80479976c2cca9f87b600917fda48475
constexpr std::string_view kCriticalProcessPrefix =
    "ROOT JOB TERMINATED BY CRITICAL PROCESS DEATH: ";
constexpr std::string_view kBeginDlog = "--- BEGIN DLOG DUMP ---";
constexpr std::string_view kEndDlog = "--- END DLOG DUMP ---";

enum class ZirconRebootReason {
  kNotSet,
  kCold,
  kNoCrash,
  kKernelPanic,
  kOOM,
  kHwWatchdog,
  kSwWatchdog,
  kBrownout,
  kUnknown,
  kRootJobTermination,
  kNotParseable,
};

zx::duration ExtractTime(const std::string_view line) {
  const std::string line_copy(line);
  return zx::msec(std::stoll(line_copy));
}

ZirconRebootReason ExtractZirconRebootReason(const std::string_view line) {
  if (line == "ZIRCON REBOOT REASON (NO CRASH)") {
    return ZirconRebootReason::kNoCrash;
  } else if (line == "ZIRCON REBOOT REASON (KERNEL PANIC)") {
    return ZirconRebootReason::kKernelPanic;
  } else if (line == "ZIRCON REBOOT REASON (OOM)") {
    return ZirconRebootReason::kOOM;
  } else if (line == "ZIRCON REBOOT REASON (SW WATCHDOG)") {
    return ZirconRebootReason::kSwWatchdog;
  } else if (line == "ZIRCON REBOOT REASON (HW WATCHDOG)") {
    return ZirconRebootReason::kHwWatchdog;
  } else if (line == "ZIRCON REBOOT REASON (BROWNOUT)") {
    return ZirconRebootReason::kBrownout;
  } else if (line == "ZIRCON REBOOT REASON (UNKNOWN)") {
    return ZirconRebootReason::kUnknown;
  } else if (line == "ZIRCON REBOOT REASON (USERSPACE ROOT JOB TERMINATION)") {
    return ZirconRebootReason::kRootJobTermination;
  }

  FX_LOGS(ERROR) << "Failed to extract a reboot reason from Zircon reboot log";
  return ZirconRebootReason::kNotParseable;
}

ZirconRebootReason ExtractZirconRebootInfo(const std::string& path,
                                           std::optional<std::string>* content,
                                           std::optional<zx::duration>* uptime,
                                           std::optional<zx::duration>* runtime,
                                           std::optional<std::string>* crashed_process) {
  if (!files::IsFile(path)) {
    return ZirconRebootReason::kCold;
  }

  std::string file_content;
  if (!files::ReadFileToString(path, &file_content)) {
    FX_LOGS(ERROR) << "Failed to read Zircon reboot log from " << path;
    return ZirconRebootReason::kNotParseable;
  }

  if (file_content.empty()) {
    FX_LOGS(ERROR) << "Found empty Zircon reboot log at " << path;
    return ZirconRebootReason::kNotParseable;
  }

  *content = file_content;
  (*content)->erase(std::find((*content)->begin(), (*content)->end(), '\0'), (*content)->end());

  const std::vector<std::string_view> lines =
      fxl::SplitString(content->value(), "\n", fxl::WhiteSpaceHandling::kTrimWhitespace,
                       fxl::SplitResult::kSplitWantNonEmpty);

  if (lines.size() == 0) {
    FX_LOGS(ERROR) << "Zircon reboot log has no content";
    return ZirconRebootReason::kNotSet;
  }

  // We expect the format to be:
  //
  // ZIRCON REBOOT REASON (<SOME REASON>)
  // <empty>
  // UPTIME (ms)
  // <SOME UPTIME>
  // RUNTIME (ms)
  // <SOME RUNTIME>
  const auto reason = ExtractZirconRebootReason(lines[0]);

  if (lines.size() < 3) {
    FX_LOGS(ERROR) << "Zircon reboot log is missing uptime information";
  } else if (lines[1] != "UPTIME (ms)") {
    FX_LOGS(ERROR) << "'UPTIME(ms)' not present, found '" << lines[1] << "'";
  } else {
    *uptime = ExtractTime(lines[2]);
  }

  if (lines.size() < 5) {
    FX_LOGS(ERROR) << "Zircon reboot log is missing runtime information";
  } else if (lines[3] != "RUNTIME (ms)") {
    FX_LOGS(ERROR) << "'RUNTIME(ms)' not present, found '" << lines[3] << "'";
  } else {
    *runtime = ExtractTime(lines[4]);
  }

  // We expect the critical process to look like:
  //
  // ROOT JOB TERMINATED BY CRITICAL PROCESS DEATH: <PROCESS> (<KOID>)
  for (auto line : lines) {
    if (line.substr(0, kCriticalProcessPrefix.size()) != kCriticalProcessPrefix) {
      continue;
    }

    line.remove_prefix(kCriticalProcessPrefix.size());

    if (const auto r_paren = line.find_last_of('('); r_paren == line.npos) {
      continue;
    } else {
      line.remove_suffix(line.size() - r_paren);
    }

    if (line.empty() || line.back() != ' ') {
      continue;
    }
    line.remove_suffix(1);

    *crashed_process = line;
    break;
  }

  return reason;
}

// Prints |reboot_log| with the DLOG removed. Returns the removed DLOG, if present.
std::optional<std::string> ExtractDlogAndLogRebootLog(const std::string& reboot_log) {
  auto fallback_log =
      fit::defer([&reboot_log] { FX_LOGS(INFO) << "Reboot info:\n"
                                               << reboot_log; });

  const size_t begin_header_pos = reboot_log.find(kBeginDlog);
  if (begin_header_pos == std::string::npos) {
    return std::nullopt;
  }

  const size_t payload_begin = begin_header_pos + kBeginDlog.size();
  const size_t payload_end = reboot_log.find(kEndDlog, begin_header_pos);

  if (payload_end == std::string::npos) {
    // For some reason the DLOG dump started, but never finished.
    return std::nullopt;
  }

  const size_t end_footer_pos = payload_end + kEndDlog.size();

  fallback_log.cancel();
  FX_LOGS(INFO) << "Reboot info:\n"
                << reboot_log.substr(0, begin_header_pos)
                << "DLOG dump can be found in the snapshot file: "
                << feedback_data::kAttachmentLogKernelPrevious << reboot_log.substr(end_footer_pos);

  const std::string dlog = reboot_log.substr(payload_begin, payload_end - payload_begin);

  return std::string(fxl::TrimString(dlog, " \f\n\r\t\v"));
}

std::vector<GracefulRebootReason> ExtractGracefulRebootInfo(
    const std::string& graceful_reboot_log_path) {
  if (!files::IsFile(graceful_reboot_log_path)) {
    return {};
  }

  std::string file_content;
  if (!files::ReadFileToString(graceful_reboot_log_path, &file_content)) {
    return {GracefulRebootReason::kNotParseable};
  }

  if (file_content.empty()) {
    return {GracefulRebootReason::kNotParseable};
  }

  return FromFileContent(file_content);
}

RebootReason FromGracefulRebootReason(const GracefulRebootReason& reason) {
  switch (reason) {
    case GracefulRebootReason::kUserRequest:
      return RebootReason::kUserRequest;
    case GracefulRebootReason::kSystemUpdate:
      return RebootReason::kSystemUpdate;
    case GracefulRebootReason::kRetrySystemUpdate:
      return RebootReason::kRetrySystemUpdate;
    case GracefulRebootReason::kHighTemperature:
      return RebootReason::kHighTemperature;
    case GracefulRebootReason::kSessionFailure:
      return RebootReason::kSessionFailure;
    case GracefulRebootReason::kSysmgrFailure:
      return RebootReason::kSysmgrFailure;
    case GracefulRebootReason::kCriticalComponentFailure:
      return RebootReason::kCriticalComponentFailure;
    case GracefulRebootReason::kFdr:
      return RebootReason::kFdr;
    case GracefulRebootReason::kZbiSwap:
      return RebootReason::kZbiSwap;
    case GracefulRebootReason::kNotSupported:
    case GracefulRebootReason::kNotParseable:
      return RebootReason::kGenericGraceful;
    case GracefulRebootReason::kNetstackMigration:
      return RebootReason::kNetstackMigration;
    case GracefulRebootReason::kOutOfMemory:
      return RebootReason::kOOM;
    case GracefulRebootReason::kNotSet:
      FX_LOGS(FATAL) << "Graceful reboot reason must be set";
      return RebootReason::kNotParseable;
  }
}

RebootReason ConsolidateGracefulRebootReasons(const std::vector<GracefulRebootReason>& reasons) {
  if (reasons.empty()) {
    return RebootReason::kGenericGraceful;
  }

  // If there's only one reason, consolidation is trivial.
  if (reasons.size() == 1) {
    return FromGracefulRebootReason(reasons[0]);
  }

  // Otherwise, verify it's an expected combination of reasons.
  std::unordered_set<GracefulRebootReason> reasons_set(reasons.begin(), reasons.end());
  if (reasons_set.size() == 2 && reasons_set.contains(GracefulRebootReason::kNetstackMigration) &&
      reasons_set.contains(GracefulRebootReason::kSystemUpdate)) {
    // Netstack Migration + System Update is consolidated to System Update.
    return RebootReason::kSystemUpdate;
  }

  FX_LOGS(WARNING) << "Unexpected combination of graceful reboot reasons: " << ToLog(reasons);
  return RebootReason::kUnexpectedReasonGraceful;
}

RebootReason DetermineRebootReason(const ZirconRebootReason zircon_reason,
                                   const std::vector<GracefulRebootReason>& graceful_reasons,
                                   const bool not_a_fdr) {
  switch (zircon_reason) {
    case ZirconRebootReason::kCold:
      return RebootReason::kCold;
    case ZirconRebootReason::kKernelPanic:
      return RebootReason::kKernelPanic;
    case ZirconRebootReason::kOOM:
      return RebootReason::kOOM;
    case ZirconRebootReason::kHwWatchdog:
      return RebootReason::kHardwareWatchdogTimeout;
    case ZirconRebootReason::kSwWatchdog:
      return RebootReason::kSoftwareWatchdogTimeout;
    case ZirconRebootReason::kBrownout:
      return RebootReason::kBrownout;
    case ZirconRebootReason::kUnknown:
      return RebootReason::kSpontaneous;
    case ZirconRebootReason::kRootJobTermination:
      return RebootReason::kRootJobTermination;
    case ZirconRebootReason::kNotParseable:
      return RebootReason::kNotParseable;
    case ZirconRebootReason::kNoCrash:
      if (!not_a_fdr) {
        return RebootReason::kFdr;
      }
      return ConsolidateGracefulRebootReasons(graceful_reasons);
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "|zircon_reason| must be set";
      return RebootReason::kNotParseable;
  }
}

std::string MakeRebootLog(const std::optional<std::string>& zircon_reboot_log,
                          const std::vector<GracefulRebootReason>& graceful_reasons,
                          const RebootReason reboot_reason) {
  std::vector<std::string> lines;

  if (zircon_reboot_log.has_value()) {
    lines.push_back(zircon_reboot_log.value());
  }

  lines.push_back(
      fxl::StringPrintf("GRACEFUL REBOOT REASONS: (%s)\n", ToLog(graceful_reasons).c_str()));

  lines.push_back(fxl::StringPrintf("FINAL REBOOT REASON (%s)", ToString(reboot_reason).c_str()));

  return fxl::JoinStrings(lines, "\n");
}

}  // namespace

// static
RebootLog RebootLog::ParseRebootLog(const std::string& zircon_reboot_log_path,
                                    const std::string& graceful_reboot_log_path,
                                    const bool not_a_fdr) {
  std::optional<std::string> zircon_reboot_log;
  std::optional<zx::duration> last_boot_uptime;
  std::optional<zx::duration> last_boot_runtime;
  std::optional<std::string> critical_process;
  const auto zircon_reason =
      ExtractZirconRebootInfo(zircon_reboot_log_path, &zircon_reboot_log, &last_boot_uptime,
                              &last_boot_runtime, &critical_process);

  const auto graceful_reasons = ExtractGracefulRebootInfo(graceful_reboot_log_path);

  const auto reboot_reason = DetermineRebootReason(zircon_reason, graceful_reasons, not_a_fdr);
  const auto reboot_log = MakeRebootLog(zircon_reboot_log, graceful_reasons, reboot_reason);
  const std::optional<std::string> dlog = ExtractDlogAndLogRebootLog(reboot_log);

  return RebootLog(reboot_reason, reboot_log, dlog, last_boot_uptime, last_boot_runtime,
                   critical_process);
}

RebootLog::RebootLog(enum RebootReason reboot_reason, std::string reboot_log_str,
                     std::optional<std::string> dlog, std::optional<zx::duration> last_boot_uptime,
                     std::optional<zx::duration> last_boot_runtime,
                     std::optional<std::string> critical_process)
    : reboot_reason_(reboot_reason),
      reboot_log_str_(reboot_log_str),
      dlog_(std::move(dlog)),
      last_boot_uptime_(last_boot_uptime),
      last_boot_runtime_(last_boot_runtime),
      critical_process_(critical_process) {}

}  // namespace feedback
}  // namespace forensics
