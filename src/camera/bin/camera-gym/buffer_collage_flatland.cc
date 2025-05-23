// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/camera/bin/camera-gym/buffer_collage_flatland.h"

#include <fuchsia/images/cpp/fidl.h>
#include <fuchsia/sysmem/cpp/fidl.h>
#include <fuchsia/ui/composition/cpp/fidl.h>
#include <fuchsia/ui/views/cpp/fidl.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async/cpp/wait.h>
#include <lib/async/default.h>
#include <lib/async/dispatcher.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/ui/scenic/cpp/view_creation_tokens.h>
#include <lib/ui/scenic/cpp/view_identity.h>
#include <lib/ui/scenic/cpp/view_token_pair.h>
#include <lib/zx/eventpair.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <cmath>
#include <memory>
#include <optional>

#include "src/camera/bin/camera-gym/screen_util.h"
#include "src/ui/scenic/lib/allocation/buffer_collection_import_export_tokens.h"

namespace camera_flatland {

using Command = fuchsia::camera::gym::Command;

using SetDescriptionCommand = fuchsia::camera::gym::SetDescriptionCommand;
using CaptureFrameCommand = fuchsia::camera::gym::CaptureFrameCommand;

constexpr uint32_t kViewRequestTimeoutMs = 5000;

BufferCollageFlatland::BufferCollageFlatland() : loop_(&kAsyncLoopConfigNoAttachToCurrentThread) {
  SetStopOnError(sysmem_allocator_);
}

BufferCollageFlatland::~BufferCollageFlatland() {
  zx_status_t status =
      async::PostTask(loop_.dispatcher(), fit::bind_member(this, &BufferCollageFlatland::Stop));
  ZX_ASSERT(status == ZX_OK);
  loop_.JoinThreads();
}

fpromise::result<std::unique_ptr<BufferCollageFlatland>, zx_status_t> BufferCollageFlatland::Create(
    std::unique_ptr<simple_present::FlatlandConnection> flatland_connection,
    fuchsia::ui::composition::AllocatorHandle flatland_allocator,
    fuchsia::element::GraphicalPresenterHandle graphical_presenter,
    fuchsia::sysmem2::AllocatorHandle sysmem_allocator, fit::closure stop_callback) {
  auto collage = std::unique_ptr<BufferCollageFlatland>(new BufferCollageFlatland);
  collage->start_time_ = zx::clock::get_monotonic();
  collage->stop_callback_ = std::move(stop_callback);
  collage->flatland_connection_ = std::move(flatland_connection);
  zx_status_t status =
      collage->flatland_allocator_.Bind(std::move(flatland_allocator), collage->loop_.dispatcher());
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status);
    return fpromise::error(status);
  }
  status = collage->graphical_presenter_.Bind(std::move(graphical_presenter),
                                              collage->loop_.dispatcher());
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status);
    return fpromise::error(status);
  }
  status =
      collage->sysmem_allocator_.Bind(std::move(sysmem_allocator), collage->loop_.dispatcher());
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status);
    return fpromise::error(status);
  }
  collage->flatland_ = collage->flatland_connection_->flatland();

  // Start a thread and begin processing messages.
  status = collage->loop_.StartThread("BufferCollage Loop");
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status);
    return fpromise::error(status);
  }
  return fpromise::ok(std::move(collage));
}

// Called by stream_cycler when a new camera stream is available.
fpromise::promise<uint32_t> BufferCollageFlatland::AddCollection(
    fuchsia::sysmem2::BufferCollectionTokenHandle token, fuchsia::images2::ImageFormat image_format,
    std::string description) {
  TRACE_DURATION("camera", "BufferCollageFlatland::AddCollection");
  ZX_ASSERT(image_format.has_size());
  ZX_ASSERT(image_format.size().width > 0);
  ZX_ASSERT(image_format.size().height > 0);
  ZX_ASSERT(image_format.has_bytes_per_row());
  ZX_ASSERT(image_format.bytes_per_row() > 0);
  ZX_ASSERT(flatland_connection_);

  fpromise::bridge<uint32_t> task_bridge;

  fit::closure add_collection = [this, token = std::move(token),
                                 image_format = std::move(image_format), description,
                                 result = std::move(task_bridge.completer)]() mutable {
    auto collection_id = next_collection_id_++;
    FX_LOGS(DEBUG) << "Adding collection with ID " << collection_id << ".";

    ZX_ASSERT(collection_views_.find(collection_id) == collection_views_.end());
    auto& view = collection_views_[collection_id];
    auto name = "Collection (" + std::to_string(collection_id) + ")";

    SetRemoveCollectionViewOnError(view.buffer_collection, collection_id, name);
    view.image_format = fidl::Clone(image_format);

    // Bind token to |loop_| and duplicate the token.
    fuchsia::sysmem2::BufferCollectionTokenPtr token_ptr;
    SetStopOnError(token_ptr, "BufferCollectionToken");
    zx_status_t status = token_ptr.Bind(std::move(token), loop_.dispatcher());
    if (status != ZX_OK) {
      FX_LOGS(ERROR) << "Failed to bind token. " << status;
      Stop();
      result.complete_error();
      return;
    }
    fuchsia::sysmem2::BufferCollectionTokenHandle scenic_token;

    fuchsia::sysmem2::BufferCollectionTokenDuplicateRequest dup_request;
    dup_request.set_rights_attenuation_mask(ZX_RIGHT_SAME_RIGHTS);
    dup_request.set_token_request(scenic_token.NewRequest());
    token_ptr->Duplicate(std::move(dup_request));

    fuchsia::sysmem2::AllocatorBindSharedCollectionRequest bind_shared_request;
    bind_shared_request.set_token(std::move(token_ptr));
    bind_shared_request.set_buffer_collection_request(
        view.buffer_collection.NewRequest(loop_.dispatcher()));
    sysmem_allocator_->BindSharedCollection(std::move(bind_shared_request));

    // Sync the collection and create flatland image using token provided by camera stream.
    view.buffer_collection->Sync([this, collection_id, token = std::move(scenic_token),
                                  result = std::move(result)](
                                     fuchsia::sysmem2::Node_Sync_Result sync_result) mutable {
      ZX_ASSERT(collection_views_.find(collection_id) != collection_views_.end());
      auto& view = collection_views_[collection_id];

      // Set minimal constraints then wait for buffer allocation.
      fuchsia::sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
      set_constraints_request.mutable_constraints()->mutable_usage()->set_none(
          fuchsia::sysmem2::NONE_USAGE);
      view.buffer_collection->SetConstraints(std::move(set_constraints_request));
      view.ref_pair = allocation::BufferCollectionImportExportTokens::New();
      fuchsia::ui::composition::RegisterBufferCollectionArgs args = {};

      args.set_export_token(std::move(view.ref_pair.export_token));
      args.set_buffer_collection_token2(std::move(token));
      flatland_allocator_->RegisterBufferCollection(
          std::move(args), [this, collection_id, result = std::move(result)](
                               fuchsia::ui::composition::Allocator_RegisterBufferCollection_Result
                                   register_result) mutable {
            if (register_result.is_err()) {
              FX_LOGS(ERROR) << "Failed to register buffers.";
              Stop();
              result.complete_error();
              return;
            }
            ZX_ASSERT(collection_views_.find(collection_id) != collection_views_.end());
            auto& view = collection_views_[collection_id];
            view.buffer_collection->WaitForAllBuffersAllocated(
                [this, collection_id, result = std::move(result)](
                    fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result
                        wait_result) mutable {
                  if (wait_result.is_framework_err()) {
                    FX_PLOGS(ERROR, fidl::ToUnderlying(wait_result.framework_err()))
                        << "Failed to allocate buffers (framework err).";
                    Stop();
                    result.complete_error();
                    return;
                  }
                  if (wait_result.is_err()) {
                    FX_PLOGS(ERROR, static_cast<uint32_t>(wait_result.err()))
                        << "Failed to allocate buffers (err).";
                    Stop();
                    result.complete_error();
                    return;
                  }
                  ZX_ASSERT(collection_views_.find(collection_id) != collection_views_.end());
                  auto& view = collection_views_[collection_id];
                  fuchsia::ui::composition::TransformId transform_id{.value = next_transform_id++};
                  view.transform_id = transform_id;
                  uint32_t buffer_count = static_cast<uint32_t>(
                      wait_result.response().buffer_collection_info().buffers().size());
                  view.buffer_count = buffer_count;

                  // Rearranges layout to add the new view. Content doesn't get updated until
                  // PostShowBuffer is called.
                  UpdateLayout();
                  for (uint32_t buffer_id = 0; buffer_id < buffer_count; ++buffer_id) {
                    fuchsia::ui::composition::ImageProperties image_properties = {};
                    image_properties.set_size(
                        {view.image_format.size().width, view.image_format.size().height});
                    fuchsia::ui::composition::BufferCollectionImportToken import_token_copy;
                    view.ref_pair.import_token.value.duplicate(ZX_RIGHT_SAME_RIGHTS,
                                                               &import_token_copy.value);
                    // TODO: Should we try to reuse content_id?
                    fuchsia::ui::composition::ContentId content_id{.value = next_content_id++};
                    view.buffer_id_to_content_id[buffer_id] = content_id;
                    flatland_->CreateImage({content_id}, std::move(import_token_copy), buffer_id,
                                           std::move(image_properties));
                  }
                  FX_LOGS(DEBUG) << "Successfully added collection " << collection_id << ".";
                  result.complete_ok(collection_id);
                });
          });
    });
  };
  async::PostTask(loop_.dispatcher(), std::move(add_collection));
  return task_bridge.consumer.promise();
}

void BufferCollageFlatland::RemoveCollection(uint32_t collection_id) {
  TRACE_DURATION("camera", "BufferCollage::RemoveCollection");
  if (async_get_default_dispatcher() != loop_.dispatcher()) {
    // Marshal the task to our own thread if called from elsewhere.
    auto nonce = TRACE_NONCE();
    TRACE_FLOW_BEGIN("camera", "post_remove_collection", nonce);
    async::PostTask(loop_.dispatcher(), [this, collection_id, nonce] {
      TRACE_DURATION("camera", "BufferCollage::RemoveCollection.task");
      TRACE_FLOW_END("camera", "post_remove_collection", nonce);
      RemoveCollection(collection_id);
    });
    return;
  }

  auto it = collection_views_.find(collection_id);
  if (it == collection_views_.end()) {
    FX_LOGS(INFO) << "Skipping RemoveCollection for already-removed collection ID "
                  << collection_id;
    return;
  }

  auto& view = it->second;
  if (view.buffer_collection.is_bound()) {
    view.buffer_collection->Release();
  }

  for (uint32_t buffer_id = 0; buffer_id < view.buffer_count; ++buffer_id) {
    auto buffer_info_it = view.buffer_id_to_content_id.find(buffer_id);
    if (buffer_info_it != view.buffer_id_to_content_id.end()) {
      auto& content_id = view.buffer_id_to_content_id[buffer_id];
      flatland_->ReleaseImage(content_id);
    }
  }
  flatland_->RemoveChild(kRootTransformId, view.transform_id);
  flatland_->ReleaseTransform(view.transform_id);
  if (view.buffer_collection.is_bound()) {
    view.buffer_collection->Release();
  }
  collection_views_.erase(it);
  UpdateLayout();
}

void BufferCollageFlatland::PostShowBuffer(uint32_t collection_id, uint32_t buffer_index,
                                           zx::eventpair* release_fence,
                                           std::optional<fuchsia::math::RectF> subregion) {
  auto nonce = TRACE_NONCE();
  TRACE_DURATION("camera", "BufferCollage::PostShowBuffer");
  TRACE_FLOW_BEGIN("camera", "post_show_buffer", nonce);
  async::PostTask(loop_.dispatcher(), [=, release_fence = release_fence]() mutable {
    TRACE_DURATION("camera", "BufferCollage::PostShowBuffer.task");
    TRACE_FLOW_END("camera", "post_show_buffer", nonce);
    ShowBuffer(collection_id, buffer_index, release_fence, subregion);
  });
}

void BufferCollageFlatland::Stop() {
  flatland_->Clear();
  collection_views_.clear();
  loop_.Quit();
  if (stop_callback_) {
    stop_callback_();
    stop_callback_ = nullptr;
  }
  sysmem_allocator_ = nullptr;
}

template <typename T>
void BufferCollageFlatland::SetStopOnError(fidl::InterfacePtr<T>& p, std::string name) {
  p.set_error_handler([this, name, &p](zx_status_t status) {
    FX_PLOGS(ERROR, status) << name << " disconnected unexpectedly.";
    p = nullptr;
    Stop();
  });
}

template <typename T>
void BufferCollageFlatland::SetRemoveCollectionViewOnError(fidl::InterfacePtr<T>& p,
                                                           uint32_t view_id, std::string name) {
  p.set_error_handler([this, view_id, name](zx_status_t status) {
    FX_PLOGS(WARNING, status) << name << " view_id=" << view_id << " disconnected unexpectedly.";
    if (collection_views_.find(view_id) == collection_views_.end()) {
      FX_LOGS(INFO) << name << " view_id=" << view_id << " already removed.";
      return;
    }
    RemoveCollection(view_id);
  });
}

void BufferCollageFlatland::ShowBuffer(uint32_t collection_id, uint32_t buffer_index,
                                       zx::eventpair* release_fence,
                                       std::optional<fuchsia::math::RectF> subregion) {
  TRACE_DURATION("camera", "BufferCollage::ShowBuffer");
  auto it = collection_views_.find(collection_id);
  if (it == collection_views_.end()) {
    FX_LOGS(ERROR) << "Invalid collection ID " << collection_id << " out of "
                   << collection_views_.size();
    Stop();
    return;
  }
  auto& view = it->second;
  if (buffer_index >= view.buffer_count) {
    FX_LOGS(ERROR) << "Invalid buffer index " << buffer_index << ".";
    Stop();
    return;
  }
  auto caller_event = MakeEventBridge(loop_.dispatcher(), release_fence);
  if (caller_event.is_error()) {
    FX_PLOGS(ERROR, caller_event.error());
    Stop();
    return;
  }
  TRACE_FLOW_BEGIN("gfx", "flatlant_set_content", buffer_index);
  flatland_->SetContent(view.transform_id, view.buffer_id_to_content_id[buffer_index]);

  std::vector<zx::event> scenic_fences;
  scenic_fences.push_back(caller_event.take_value());

  fuchsia::ui::composition::PresentArgs present_args;
  present_args.set_release_fences(std::move(scenic_fences));
  present_args.set_unsquashable(false);

  flatland_connection_->Present(std::move(present_args), [](auto) {});
}

void BufferCollageFlatland::UpdateLayout() {
  FX_DCHECK(loop_.dispatcher() == async_get_default_dispatcher());
  ZX_ASSERT(width_ > 0);
  ZX_ASSERT(height_ > 0);

  auto [rows, cols] = screen_util::GetGridSize(static_cast<uint32_t>(collection_views_.size()));

  constexpr float kPadding = 4.0f;

  // cell_width and cell_height is the maximum size allowed to fit a single camera stream view in
  // the grid.
  float cell_width = static_cast<float>(width_) / static_cast<float>(cols) - kPadding;
  float cell_height = static_cast<float>(height_) / static_cast<float>(rows) - kPadding;
  uint32_t index = 0;
  for (auto& [id, view] : collection_views_) {
    float display_width = static_cast<float>(view.image_format.display_rect().width);
    float display_height = static_cast<float>(view.image_format.display_rect().height);
    if (!view.view_created) {
      flatland_->CreateTransform(view.transform_id);
      flatland_->AddChild(kRootTransformId, view.transform_id);
      view.view_created = true;
    }
    // scale display width and height to fix inside the cell boundary.
    auto scale = screen_util::Scale(display_width, display_height, cell_width, cell_height);
    auto [x_center, y_center] =
        screen_util::GetCenter(index++, static_cast<uint32_t>(collection_views_.size()));
    // Find center then shift up by half of scaled width.
    auto translated_x =
        static_cast<int32_t>(static_cast<float>(width_) * x_center) - (scale * display_width * 0.5);
    // Find center then shift left by half of scaled height.
    auto translated_y = static_cast<int32_t>(static_cast<float>(height_) * y_center) -
                        (scale * display_height * 0.5);
    flatland_->SetScale(view.transform_id, {scale, scale});
    flatland_->SetTranslation(view.transform_id, {static_cast<int32_t>(translated_x),
                                                  static_cast<int32_t>(translated_y)});
  }
}

void BufferCollageFlatland::SetupBaseView() {
  ZX_ASSERT(flatland_);
  flatland_->CreateTransform(kRootTransformId);
  flatland_->SetRootTransform(kRootTransformId);
}

void BufferCollageFlatland::PresentView() {
  async::PostTask(loop_.dispatcher(), [this]() mutable {
    parent_watcher_.set_error_handler([this](zx_status_t status) {
      FX_LOGS(ERROR) << "Error from fuchsia::ui::composition::ParentViewportWatcher: "
                     << zx_status_get_string(status);
      Stop();
    });
    auto view_identity = scenic::NewViewIdentityOnCreation();
    auto [view_token, parent_viewport_token] = scenic::ViewCreationTokenPair::New();

    flatland_->CreateView2(std::move(view_token), std::move(view_identity),
                           /* protocols = */ {}, parent_watcher_.NewRequest());

    parent_watcher_->GetLayout([this](auto layout_info) {
      width_ = layout_info.logical_size().width;
      height_ = layout_info.logical_size().height;
      FX_LOGS(DEBUG) << "Received layout info: w=" << width_ << ", h=" << height_;
      SetupBaseView();
      flatland_connection_->Present({}, [](auto) {});
    });

    fuchsia::element::ViewSpec view_spec;
    view_spec.set_viewport_creation_token(std::move(parent_viewport_token));
    graphical_presenter_->PresentView(
        std::move(view_spec), nullptr, nullptr,
        [](fuchsia::element::GraphicalPresenter_PresentView_Result result) {
          if (result.is_err())
            FX_LOGS(ERROR) << "PresentView failed";
        });
  });
}
}  // namespace camera_flatland
