// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CAMERA_BIN_DEVICE_STREAM_IMPL_H_
#define SRC_CAMERA_BIN_DEVICE_STREAM_IMPL_H_

#include <fuchsia/camera2/cpp/fidl.h>
#include <fuchsia/camera2/hal/cpp/fidl.h>
#include <fuchsia/camera3/cpp/fidl.h>
#include <lib/async/cpp/wait.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/fit/result.h>
#include <zircon/status.h>

#include <memory>
#include <queue>
#include <set>
#include <vector>

#include "src/camera/bin/device/util.h"
#include "src/camera/lib/hanging_get_helper/hanging_get_helper.h"

// Represents a specific stream in a camera device's configuration. Serves multiple clients of the
// camera3.Stream protocol.
class StreamImpl {
 public:
  // Called by the stream on its thread when it needs to connect to its associated legacy stream.
  using StreamRequestedCallback = fit::function<void(
      fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken>,
      fidl::InterfaceRequest<fuchsia::camera2::Stream>, fit::function<void(uint32_t)>, uint32_t)>;

  // Called by the stream on its thread when it receives a new BufferCollectionToken, passing the
  // server-side koid of the token and a callback function that receives the token validity. The
  // parent should check the token validity and invoke the callback to inform the client. The
  // callback may be invoked from any thread.
  using CheckTokenCallback = fit::function<void(zx_koid_t, fit::function<void(bool)>)>;

  StreamImpl(async_dispatcher_t* dispatcher, const fuchsia::camera3::StreamProperties2& properties,
             const fuchsia::camera2::hal::StreamConfig& legacy_config,
             fidl::InterfaceRequest<fuchsia::camera3::Stream> request,
             CheckTokenCallback check_token, StreamRequestedCallback on_stream_requested,
             fit::closure on_no_clients);
  ~StreamImpl();

  void SetMuteState(MuteState mute_state);

 private:
  // Called when a client calls Rebind.
  void OnNewRequest(fidl::InterfaceRequest<fuchsia::camera3::Stream> request);

  // Called if the underlying legacy stream disconnects.
  void OnLegacyStreamDisconnected(zx_status_t status);

  // Remove the client with the given id.
  void RemoveClient(uint64_t id);

  // Called when the legacy stream's OnFrameAvailable event fires.
  void OnFrameAvailable(fuchsia::camera2::FrameAvailableInfo info);

  // Renegotiate buffers or opt out of buffer renegotiation for the client with the given id.
  void SetBufferCollection(uint64_t id,
                           fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token);

  // Change the resolution of the stream.
  void SetResolution(uint64_t id, fuchsia::math::Size coded_size);

  // Change the crop region of the stream.
  void SetCropRegion(uint64_t id, std::unique_ptr<fuchsia::math::RectF> region);

  // Restores previously-sent state to the legacy stream.
  void RestoreLegacyStreamState();

  // Represents a single client connection to the StreamImpl class.
  class Client : public fuchsia::camera3::Stream {
   public:
    Client(StreamImpl& stream, uint64_t id,
           fidl::InterfaceRequest<fuchsia::camera3::Stream> request);
    ~Client() override;

    // Add a frame to the queue of available frames.
    void AddFrame(fuchsia::camera3::FrameInfo2 frame);

    // Send a frame to the client if one is available and has been requested.
    void MaybeSendFrame();

    // Closes |binding_| with the provided |status| epitaph, and removes the client instance from
    // the parent |clients_| map.
    void CloseConnection(zx_status_t status);

    // Add the given token to the client's token queue.
    void ReceiveBufferCollection(
        fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token);

    // Update the client's resolution.
    void ReceiveResolution(fuchsia::math::Size coded_size);

    // Update the client's crop region.
    void ReceiveCropRegion(std::unique_ptr<fuchsia::math::RectF> region);

    // Returns a mutable reference to this client's state as a participant in buffer renegotiation.
    // This state must be managed by the parent stream's thread, not the client thread.
    bool& Participant();

    // Clears the client's queue of unsent frames.
    void ClearFrames();

   private:
    // Called when the client endpoint of |binding_| is closed.
    void OnClientDisconnected(zx_status_t status);

    // |fuchsia::camera3::Stream|
    void GetProperties(GetPropertiesCallback callback) override;
    void GetProperties2(GetProperties2Callback callback) override;
    void SetCropRegion(std::unique_ptr<fuchsia::math::RectF> region) override;
    void WatchCropRegion(WatchCropRegionCallback callback) override;
    void SetResolution(fuchsia::math::Size coded_size) override;
    void WatchResolution(WatchResolutionCallback callback) override;
    void SetBufferCollection(
        fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token) override;
    void WatchBufferCollection(WatchBufferCollectionCallback callback) override;
    void WatchOrientation(WatchOrientationCallback callback) override;
    void GetNextFrame(GetNextFrameCallback callback) override;
    void GetNextFrame2(GetNextFrame2Callback callback) override;
    void Rebind(fidl::InterfaceRequest<Stream> request) override;

    StreamImpl& stream_;
    uint64_t id_;
    fidl::Binding<fuchsia::camera3::Stream> binding_;
    camera::HangingGetHelper<fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken>>
        buffers_;
    camera::HangingGetHelper<fuchsia::math::Size,
                             fit::function<bool(fuchsia::math::Size, fuchsia::math::Size)>>
        resolution_;
    camera::HangingGetHelper<std::unique_ptr<fuchsia::math::RectF>> crop_region_;
    GetNextFrame2Callback frame_callback_;
    bool participant_ = false;
    std::queue<fuchsia::camera3::FrameInfo2> frames_;
  };

  async_dispatcher_t* dispatcher_;
  const fuchsia::camera3::StreamProperties2& properties_;
  const fuchsia::camera2::hal::StreamConfig& legacy_config_;
  fuchsia::camera2::StreamPtr legacy_stream_;
  uint32_t legacy_stream_format_index_ = 0;
  std::map<uint64_t, std::unique_ptr<Client>> clients_;
  uint64_t client_id_next_ = 1;
  CheckTokenCallback check_token_;
  StreamRequestedCallback on_stream_requested_;
  fit::closure on_no_clients_;
  uint32_t max_camping_buffers_ = 0;
  uint64_t frame_counter_ = 0;
  std::map<uint32_t, std::unique_ptr<FrameWaiter>> frame_waiters_;
  fuchsia::math::Size current_resolution_;
  MuteState mute_state_;
  std::unique_ptr<fuchsia::math::RectF> current_crop_region_;
  friend class Client;
};

#endif  // SRC_CAMERA_BIN_DEVICE_STREAM_IMPL_H_
