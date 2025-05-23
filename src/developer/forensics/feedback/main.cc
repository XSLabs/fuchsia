// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/feedback/cpp/fidl.h>
#include <fuchsia/process/lifecycle/cpp/fidl.h>
#include <lib/fidl/cpp/interface_request.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/processargs.h>

#include <cstdlib>
#include <memory>

#include <fbl/unique_fd.h>

#include "src/developer/forensics/feedback/annotations/startup_annotations.h"
#include "src/developer/forensics/feedback/constants.h"
#include "src/developer/forensics/feedback/main_service.h"
#include "src/developer/forensics/feedback/namespace_init.h"
#include "src/developer/forensics/feedback/reboot_log/annotations.h"
#include "src/developer/forensics/feedback/reboot_log/reboot_log.h"
#include "src/developer/forensics/utils/cobalt/logger.h"
#include "src/developer/forensics/utils/component/component.h"
#include "src/developer/forensics/utils/storage_size.h"
#include "src/lib/files/file.h"
#include "src/lib/uuid/uuid.h"

namespace forensics::feedback {

int main() {
  forensics::component::Component component;
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({"forensics", "feedback"}).BuildAndInitialize();

  const std::optional<SnapshotConfig> snapshot_config = GetSnapshotConfig();
  if (!snapshot_config) {
    FX_LOGS(FATAL) << "Failed to get config for snapshot";
    return EXIT_FAILURE;
  }

  // Assembly will add an empty snapshot exclusion file even if the product didn't specify a
  // snapshot exclusion config.
  const std::optional<SnapshotExclusionConfig> snapshot_exclusion_config =
      GetSnapshotExclusionConfig();
  if (!snapshot_config) {
    FX_LOGS(FATAL) << "Failed to get config for snapshot exclusion";
    return EXIT_FAILURE;
  }

  const std::optional<BuildTypeConfig> build_type_config = GetBuildTypeConfig();
  if (!build_type_config) {
    FX_LOGS(FATAL) << "Failed to get config for build type";
    return EXIT_FAILURE;
  }

  const std::optional<feedback::ProductConfig> product_config = feedback::GetProductConfig();
  if (!product_config.has_value()) {
    FX_LOGS(FATAL) << "Failed to parse product config";
    return EXIT_FAILURE;
  }

  std::unique_ptr<cobalt::Logger> cobalt = std::make_unique<cobalt::Logger>(
      component.Dispatcher(), component.Services(), component.Clock());

  const bool run_log_persistence = product_config->persisted_logs_num_files.has_value() &&
                                   product_config->persisted_logs_total_size.has_value();

  if (component.IsFirstInstance()) {
    MovePreviousRebootReason();
    if (run_log_persistence) {
      CreatePreviousLogsFile(cobalt.get(), *product_config->persisted_logs_total_size);
    } else {
      // This is mostly done to preserve the previous boot logs when the device is migrating from
      // "log persistence turned on" to "log persistence turned off" and then clean up the
      // directory. It's a no-op if the directory doesn't exist in the first place.
      // It does mean we need to give it a hint as to how large the total size now that the config
      // no longer contains a number for it though.
      CreatePreviousLogsFile(cobalt.get(), StorageSize::Kilobytes(512));
    }
    MoveAndRecordBootId(uuid::Generate());
    if (std::string build_version; files::ReadFileToString(kBuildVersionPath, &build_version)) {
      MoveAndRecordBuildVersion(build_version, kPreviousBuildVersionPath, kCurrentBuildVersionPath);
    }

    if (std::string build_platform_version;
        files::ReadFileToString(kBuildPlatformVersionPath, &build_platform_version)) {
      MoveAndRecordBuildVersion(build_platform_version, kPreviousBuildPlatformVersionPath,
                                kCurrentBuildPlatformVersionPath);
    }

    if (std::string build_product_version;
        files::ReadFileToString(kBuildProductVersionPath, &build_product_version)) {
      MoveAndRecordBuildVersion(build_product_version, kPreviousBuildProductVersionPath,
                                kCurrentBuildProductVersionPath);
    }
  }

  ExposeConfig(*component.InspectRoot(), *build_type_config, *product_config);

  auto reboot_log = RebootLog::ParseRebootLog(
      "/boot/log/last-panic.txt", kPreviousGracefulRebootReasonFile, TestAndSetNotAFdr());

  std::optional<std::string> local_device_id_path = kDeviceIdPath;
  if (files::IsFile(kUseRemoteDeviceIdProviderPath)) {
    local_device_id_path = std::nullopt;
  }

  std::optional<zx::duration> delete_previous_boot_logs_time(std::nullopt);
  if (files::IsFile(kPreviousLogsFilePath)) {
    delete_previous_boot_logs_time = zx::hour(24);
  }

  const auto startup_annotations = GetStartupAnnotations(reboot_log);
  zx::channel lifecycle_channel(zx_take_startup_handle(PA_LIFECYCLE));

  std::unique_ptr<MainService> main_service = std::make_unique<MainService>(
      component.Dispatcher(), component.Services(), component.Clock(), component.InspectRoot(),
      cobalt.get(), startup_annotations,
      fidl::InterfaceRequest<fuchsia::process::lifecycle::Lifecycle>(std::move(lifecycle_channel)),
      reboot_log.Dlog(),
      MainService::Options{*build_type_config, local_device_id_path,
                           kCurrentGracefulRebootReasonFile,
                           LastReboot::Options{
                               .is_first_instance = component.IsFirstInstance(),
                               .reboot_log = reboot_log,
                               .oom_crash_reporting_delay = kOOMCrashReportingDelay,
                           },
                           CrashReports::Options{
                               .build_type_config = *build_type_config,
                               .snapshot_store_max_archives_size = kSnapshotArchivesMaxSize,
                               .snapshot_persistence_max_tmp_size =
                                   product_config->snapshot_persistence_max_tmp_size,
                               .snapshot_persistence_max_cache_size =
                                   product_config->snapshot_persistence_max_cache_size,
                               .snapshot_collector_window_duration = kSnapshotSharedRequestWindow,
                           },
                           FeedbackData::Options{
                               .snapshot_config = *snapshot_config,
                               .snapshot_exclusion_config = *snapshot_exclusion_config,
                               .is_first_instance = component.IsFirstInstance(),
                               .limit_inspect_data = build_type_config->enable_limit_inspect_data,
                               .run_log_persistence = run_log_persistence,
                               .delete_previous_boot_logs_time = delete_previous_boot_logs_time,
                           }});

  component.AddPublicService(main_service->GetHandler<fuchsia::feedback::LastRebootInfoProvider>());
  component.AddPublicService(main_service->GetHandler<fuchsia::feedback::CrashReporter>());
  component.AddPublicService(
      main_service->GetHandler<fuchsia::feedback::CrashReportingProductRegister>());
  component.AddPublicService(main_service->GetHandler<fuchsia::feedback::ComponentDataRegister>());
  component.AddPublicService(main_service->GetHandler<fuchsia::feedback::DataProvider>());

  component.RunLoop();
  return EXIT_SUCCESS;
}

}  // namespace forensics::feedback
