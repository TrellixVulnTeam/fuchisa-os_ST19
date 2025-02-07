// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "low_energy_discovery_manager.h"

#include <lib/async/default.h>
#include <lib/fit/function.h>
#include <zircon/assert.h>

#include "peer.h"
#include "peer_cache.h"
#include "src/connectivity/bluetooth/core/bt-host/hci/transport.h"

namespace bt::gap {

LowEnergyDiscoverySession::LowEnergyDiscoverySession(
    bool active, fxl::WeakPtr<LowEnergyDiscoveryManager> manager)
    : alive_(true), active_(active), manager_(manager) {
  ZX_ASSERT(manager_);
}

LowEnergyDiscoverySession::~LowEnergyDiscoverySession() {
  ZX_DEBUG_ASSERT(thread_checker_.is_thread_valid());
  if (alive_) {
    Stop();
  }
}

void LowEnergyDiscoverySession::SetResultCallback(PeerFoundCallback callback) {
  peer_found_callback_ = std::move(callback);
  if (!manager_)
    return;
  for (PeerId cached_peer_id : manager_->cached_scan_results()) {
    auto peer = manager_->peer_cache()->FindById(cached_peer_id);
    ZX_DEBUG_ASSERT(peer);
    NotifyDiscoveryResult(*peer);
  }
}

void LowEnergyDiscoverySession::Stop() {
  ZX_DEBUG_ASSERT(thread_checker_.is_thread_valid());
  ZX_DEBUG_ASSERT(alive_);
  if (manager_) {
    manager_->RemoveSession(this);
  }
  alive_ = false;
}

void LowEnergyDiscoverySession::NotifyDiscoveryResult(const Peer& peer) const {
  ZX_ASSERT(peer.le());

  if (!alive_ || !peer_found_callback_) {
    return;
  }

  if (filter_.MatchLowEnergyResult(peer.le()->advertising_data(), peer.connectable(),
                                   peer.rssi())) {
    peer_found_callback_(peer);
  }
}

void LowEnergyDiscoverySession::NotifyError() {
  alive_ = false;
  if (error_callback_) {
    error_callback_();
  }
}

LowEnergyDiscoveryManager::LowEnergyDiscoveryManager(fxl::WeakPtr<hci::Transport> hci,
                                                     hci::LowEnergyScanner* scanner,
                                                     PeerCache* peer_cache)
    : dispatcher_(async_get_default_dispatcher()),
      peer_cache_(peer_cache),
      scanner_(scanner),
      weak_ptr_factory_(this) {
  ZX_DEBUG_ASSERT(hci);
  ZX_DEBUG_ASSERT(dispatcher_);
  ZX_DEBUG_ASSERT(thread_checker_.is_thread_valid());
  ZX_DEBUG_ASSERT(peer_cache_);
  ZX_DEBUG_ASSERT(scanner_);

  scanner_->set_delegate(this);
}

LowEnergyDiscoveryManager::~LowEnergyDiscoveryManager() {
  scanner_->set_delegate(nullptr);

  DeactivateAndNotifySessions();
}

void LowEnergyDiscoveryManager::StartDiscovery(bool active, SessionCallback callback) {
  ZX_ASSERT(thread_checker_.is_thread_valid());
  ZX_ASSERT(callback);
  bt_log(INFO, "gap-le", "start %s discovery", active ? "active" : "passive");

  // If a request to start or stop is currently pending then this one will
  // become pending until the HCI request completes. This does NOT include the
  // state in which we are stopping and restarting scan in between scan
  // periods, in which case session_ will not be empty.
  //
  // If the scan needs to be upgraded to an active scan, it will be handled in OnScanStatus() when
  // the HCI request completes.
  if (!pending_.empty() ||
      (scanner_->state() == hci::LowEnergyScanner::State::kStopping && sessions_.empty())) {
    ZX_ASSERT(!scanner_->IsScanning());
    pending_.push_back(DiscoveryRequest{.active = active, .callback = std::move(callback)});
    return;
  }

  // If a peer scan is already in progress, then the request succeeds (this
  // includes the state in which we are stopping and restarting scan in between
  // scan periods).
  if (!sessions_.empty()) {
    if (active) {
      // If this is the first active session, stop scanning and wait for OnScanStatus() to initiate
      // active scan.
      if (!std::any_of(sessions_.begin(), sessions_.end(), [](auto s) { return s->active_; })) {
        scanner_->StopScan();
      }
    }

    auto session = AddSession(active);
    async::PostTask(dispatcher_,
                    [callback = std::move(callback), session = std::move(session)]() mutable {
                      callback(std::move(session));
                    });
    return;
  }

  pending_.push_back({.active = active, .callback = std::move(callback)});

  if (paused()) {
    return;
  }

  // If the scanner is not idle, it is starting/stopping, and the appropriate scanning will be
  // initiated in OnScanStatus().
  if (scanner_->IsIdle()) {
    StartScan(active);
  }
}

LowEnergyDiscoveryManager::PauseToken LowEnergyDiscoveryManager::PauseDiscovery() {
  if (!paused()) {
    bt_log(TRACE, "gap-le", "Pausing discovery");
    scanner_->StopScan();
  }

  paused_count_++;

  return PauseToken([this, self = weak_ptr_factory_.GetWeakPtr()]() {
    if (!self) {
      return;
    }

    ZX_ASSERT(paused());
    paused_count_--;
    if (paused_count_ == 0) {
      ResumeDiscovery();
    }
  });
}

bool LowEnergyDiscoveryManager::discovering() const {
  return std::any_of(sessions_.begin(), sessions_.end(), [](auto& s) { return s->active(); });
}

std::unique_ptr<LowEnergyDiscoverySession> LowEnergyDiscoveryManager::AddSession(bool active) {
  // Cannot use make_unique here since LowEnergyDiscoverySession has a private
  // constructor.
  std::unique_ptr<LowEnergyDiscoverySession> session(
      new LowEnergyDiscoverySession(active, weak_ptr_factory_.GetWeakPtr()));
  sessions_.push_back(session.get());
  return session;
}

void LowEnergyDiscoveryManager::RemoveSession(LowEnergyDiscoverySession* session) {
  ZX_ASSERT(thread_checker_.is_thread_valid());
  ZX_ASSERT(session);

  // Only alive sessions are allowed to call this method. If there is at least
  // one alive session object out there, then we MUST be scanning.
  ZX_ASSERT(session->alive());

  auto iter = std::find(sessions_.begin(), sessions_.end(), session);
  ZX_ASSERT(iter != sessions_.end());

  bool active = session->active();

  sessions_.erase(iter);

  bool last_active = active && std::none_of(sessions_.begin(), sessions_.end(),
                                            [](auto& s) { return s->active_; });

  // Stop scanning if the session count has dropped to zero or the scan type needs to be downgraded
  // to passive.
  if (sessions_.empty() || last_active) {
    bt_log(TRACE, "gap-le", "Last %sdiscovery session removed, stopping scan (sessions: %zu)",
           last_active ? "active " : "", sessions_.size());
    scanner_->StopScan();
    return;
  }
}

void LowEnergyDiscoveryManager::OnPeerFound(const hci::LowEnergyScanResult& result,
                                            const ByteBuffer& data) {
  ZX_DEBUG_ASSERT(thread_checker_.is_thread_valid());
  bt_log(DEBUG, "gap-le", "peer found: %s", bt_str(result.address));

  auto peer = peer_cache_->FindByAddress(result.address);
  if (peer && peer->connectable() && peer->le() && connectable_cb_) {
    bt_log(TRACE, "gap-le", "found connectable peer (id: %s)", bt_str(peer->identifier()));
    connectable_cb_(peer);
  }

  // Don't notify sessions of unknown LE peers during passive scan.
  if (scanner_->IsPassiveScanning() && (!peer || !peer->le())) {
    return;
  }

  // Create a new entry if we found the device during general discovery.
  if (!peer) {
    peer = peer_cache_->NewPeer(result.address, result.connectable);
  }
  peer->MutLe().SetAdvertisingData(result.rssi, data);

  cached_scan_results_.insert(peer->identifier());

  for (auto iter = sessions_.begin(); iter != sessions_.end();) {
    // The session may be erased by the result handler, so we need to get the next iterator before
    // iter is invalidated.
    auto next = std::next(iter);
    auto session = *iter;
    session->NotifyDiscoveryResult(*peer);
    iter = next;
  }
}

void LowEnergyDiscoveryManager::OnDirectedAdvertisement(const hci::LowEnergyScanResult& result) {
  ZX_DEBUG_ASSERT(thread_checker_.is_thread_valid());

  bt_log(TRACE, "gap-le", "Received directed advertisement (address: %s, %s)",
         result.address.ToString().c_str(), (result.resolved ? "resolved" : "not resolved"));

  auto peer = peer_cache_->FindByAddress(result.address);
  if (!peer) {
    bt_log(DEBUG, "gap-le", "ignoring connection request from unknown peripheral: %s",
           result.address.ToString().c_str());
    return;
  }

  if (!peer->le()) {
    bt_log(DEBUG, "gap-le", "rejecting connection request from non-LE peripheral: %s",
           result.address.ToString().c_str());
    return;
  }

  if (peer->connectable() && connectable_cb_) {
    connectable_cb_(peer);
  }

  // Only notify passive sessions.
  for (auto iter = sessions_.begin(); iter != sessions_.end();) {
    // The session may be erased by the result handler, so we need to get the next iterator before
    // iter is invalidated.
    auto next = std::next(iter);
    auto session = *iter;
    if (!session->active()) {
      session->NotifyDiscoveryResult(*peer);
    }
    iter = next;
  }
}

void LowEnergyDiscoveryManager::OnScanStatus(hci::LowEnergyScanner::ScanStatus status) {
  switch (status) {
    case hci::LowEnergyScanner::ScanStatus::kFailed:
      OnScanFailed();
      return;
    case hci::LowEnergyScanner::ScanStatus::kPassive:
      OnPassiveScanStarted();
      return;
    case hci::LowEnergyScanner::ScanStatus::kActive:
      OnActiveScanStarted();
      return;
    case hci::LowEnergyScanner::ScanStatus::kStopped:
      OnScanStopped();
      return;
    case hci::LowEnergyScanner::ScanStatus::kComplete:
      OnScanComplete();
      return;
  }
}

void LowEnergyDiscoveryManager::OnScanFailed() {
  bt_log(ERROR, "gap-le", "failed to initiate scan!");

  DeactivateAndNotifySessions();

  // Report failure on all currently pending requests. If any of the
  // callbacks issue a retry the new requests will get re-queued and
  // notified of failure in the same loop here.
  while (!pending_.empty()) {
    auto request = std::move(pending_.back());
    pending_.pop_back();
    request.callback(nullptr);
  }
}

void LowEnergyDiscoveryManager::OnPassiveScanStarted() {
  bt_log(TRACE, "gap-le", "passive scan started");

  // Stop the passive scan if an active scan was requested while the scan was starting.
  // The active scan will start in OnScanStatus() once the passive scan stops.
  if (std::any_of(sessions_.begin(), sessions_.end(), [](auto& s) { return s->active_; }) ||
      std::any_of(pending_.begin(), pending_.end(), [](auto& p) { return p.active; })) {
    bt_log(TRACE, "gap-le", "active scan requested while passive scan was starting");
    scanner_->StopScan();
    return;
  }

  NotifyPending();
}

void LowEnergyDiscoveryManager::OnActiveScanStarted() {
  bt_log(TRACE, "gap-le", "active scan started");

  NotifyPending();
}

void LowEnergyDiscoveryManager::OnScanStopped() {
  bt_log(DEBUG, "gap-le", "stopped scanning (paused: %d, pending: %zu, sessions: %zu)", paused(),
         pending_.size(), sessions_.size());

  cached_scan_results_.clear();

  if (paused()) {
    return;
  }

  if (!sessions_.empty()) {
    bt_log(DEBUG, "gap-le", "initiating scanning");
    bool active =
        std::any_of(sessions_.begin(), sessions_.end(), [](auto& s) { return s->active_; });
    StartScan(active);
    return;
  }

  // Some clients might have requested to start scanning while we were
  // waiting for it to stop. Restart scanning if that is the case.
  if (!pending_.empty()) {
    bt_log(DEBUG, "gap-le", "initiating scanning");
    bool active = std::any_of(pending_.begin(), pending_.end(), [](auto& p) { return p.active; });
    StartScan(active);
    return;
  }
}

void LowEnergyDiscoveryManager::OnScanComplete() {
  bt_log(TRACE, "gap-le", "end of scan period");
  cached_scan_results_.clear();

  if (paused()) {
    return;
  }

  // If |sessions_| is empty this is because sessions were stopped while the
  // scanner was shutting down after the end of the scan period. Restart the
  // scan as long as clients are waiting for it.
  ResumeDiscovery();
}

void LowEnergyDiscoveryManager::NotifyPending() {
  // Create and register all sessions before notifying the clients. We do
  // this so that the reference count is incremented for all new sessions
  // before the callbacks execute, to prevent a potential case in which a
  // callback stops its session immediately which could cause the reference
  // count to drop the zero before all clients receive their session object.
  if (!pending_.empty()) {
    size_t count = pending_.size();
    std::vector<std::unique_ptr<LowEnergyDiscoverySession>> new_sessions(count);
    std::generate(new_sessions.begin(), new_sessions.end(),
                  [this, i = size_t{0}]() mutable { return AddSession(pending_[i++].active); });

    for (size_t i = count - 1; i < count; i--) {
      auto cb = std::move(pending_.back().callback);
      pending_.pop_back();
      cb(std::move(new_sessions[i]));
    }
  }
  ZX_ASSERT(pending_.empty());
}

void LowEnergyDiscoveryManager::StartScan(bool active) {
  auto cb = [self = weak_ptr_factory_.GetWeakPtr()](auto status) {
    if (self)
      self->OnScanStatus(status);
  };

  // TODO(armansito): A client that is interested in scanning nearby beacons and
  // calculating proximity based on RSSI changes may want to disable duplicate
  // filtering. We generally shouldn't allow this unless a client has the
  // capability for it. Processing all HCI events containing advertising reports
  // will both generate a lot of bus traffic and performing duplicate filtering
  // on the host will take away CPU cycles from other things. It's a valid use
  // case but needs proper management. For now we always make the controller
  // filter duplicate reports.
  hci::LowEnergyScanner::ScanOptions options{
      .active = active,
      .filter_duplicates = true,
      .filter_policy = hci::LEScanFilterPolicy::kNoWhiteList,
      .period = scan_period_,
      .scan_response_timeout = kLEScanResponseTimeout,
  };

  // See Vol 3, Part C, 9.3.11 "Connection Establishment Timing Parameters".
  if (active) {
    options.interval = kLEScanFastInterval;
    options.window = kLEScanFastWindow;
  } else {
    options.interval = kLEScanSlowInterval1;
    options.window = kLEScanSlowWindow1;
    // TODO(armansito): Use the controller whitelist to filter advertisements.
  }

  // Since we use duplicate filtering, we stop and start the scan periodically
  // to re-process advertisements. We use the minimum required scan period for
  // general discovery (by default; |scan_period_| can be modified, e.g. by unit
  // tests).
  scanner_->StartScan(options, std::move(cb));
}

void LowEnergyDiscoveryManager::ResumeDiscovery() {
  ZX_ASSERT(!paused());

  if (!scanner_->IsIdle()) {
    bt_log(TRACE, "gap-le", "attempt to resume discovery when it is not idle");
    return;
  }

  if (!sessions_.empty()) {
    bt_log(TRACE, "gap-le", "resuming scan");
    bool active =
        std::any_of(sessions_.begin(), sessions_.end(), [](auto& s) { return s->active_; });
    StartScan(active);
    return;
  }

  if (!pending_.empty()) {
    bt_log(TRACE, "gap-le", "starting scan");
    bool active = std::any_of(pending_.begin(), pending_.end(), [](auto& s) { return s.active; });
    StartScan(active);
    return;
  }
}

void LowEnergyDiscoveryManager::DeactivateAndNotifySessions() {
  // If there are any active sessions we invalidate by notifying of an error.

  // We move the initial set and notify those, if any error callbacks create
  // additional sessions they will be added to pending_
  auto sessions = std::move(sessions_);
  for (const auto& session : sessions) {
    if (session->alive()) {
      session->NotifyError();
    }
  }

  // Due to the move, sessions_ should be empty before the loop and any
  // callbacks will add sessions to pending_ so it should be empty
  // afterwards as well.
  ZX_ASSERT(sessions_.empty());
}

}  // namespace bt::gap
