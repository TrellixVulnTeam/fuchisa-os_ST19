/*
 * Copyright (c) 2010 Broadcom Corporation
 *
 * Permission to use, copy, modify, and/or distribute this software for any purpose with or without
 * fee is hereby granted, provided that the above copyright notice and this permission notice appear
 * in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS
 * SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE
 * AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT,
 * NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
 * OF THIS SOFTWARE.
 */

#ifndef SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_BROADCOM_BRCMFMAC_DEBUG_H_
#define SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_BROADCOM_BRCMFMAC_DEBUG_H_

#include <stdint.h>
#include <zircon/types.h>

#include <algorithm>
#include <cstring>
#include <utility>

#include <ddk/debug.h>

// Some convenience macros for error and debug printing.
#define BRCMF_ERR(fmt, ...) zxlogf(ERROR, "(%s): " fmt, __func__, ##__VA_ARGS__)

#define BRCMF_WARN(fmt, ...) zxlogf(WARNING, "(%s): " fmt, __func__, ##__VA_ARGS__)

#define BRCMF_INFO(fmt, ...) zxlogf(INFO, "(%s): " fmt, __func__, ##__VA_ARGS__)

#define BRCMF_DBG(filter, fmt, ...)                        \
  do {                                                     \
    if (BRCMF_IS_ON(filter)) {                             \
      zxlogf(INFO, "(%s): " fmt, __func__, ##__VA_ARGS__); \
    }                                                      \
  } while (0)

#define BRCMF_DBG_EVENT(ifp, event_msg, REASON_FMT, reason_formatter) \
  BRCMF_DBG_LOG_EVENT(EVENT, ifp, event_msg, REASON_FMT, reason_formatter)

#define BRCMF_DBG_LOG_EVENT(FILTER, ifp, event_msg, REASON_FMT, reason_formatter)                 \
  {                                                                                               \
    if (ifp == nullptr || event_msg == nullptr) {                                                 \
      BRCMF_DBG(FILTER, "Unable to log event %p for ifp %p", event_msg, ifp);                     \
    } else {                                                                                      \
      BRCMF_DBG(FILTER, "IF: %d event %s (%u)", ifp == nullptr ? -1 : ifp->ifidx,                 \
                brcmf_fweh_event_name(static_cast<brcmf_fweh_event_code>(event_msg->event_code)), \
                event_msg->event_code);                                                           \
      BRCMF_DBG(FILTER, "  status %s", brcmf_fweh_get_event_status_str(event_msg->status));       \
      BRCMF_DBG(FILTER, "  reason " REASON_FMT, reason_formatter(event_msg->reason));             \
      BRCMF_DBG(FILTER, "    auth %s", brcmf_fweh_get_auth_type_str(event_msg->auth_type));       \
      BRCMF_DBG(FILTER, "   flags 0x%x", event_msg->flags);                                       \
    }                                                                                             \
  }

// TODO(fxb/61311): Remove once this verbose logging is no longer needed in
// brcmf_indicate_client_disconnect().
#define BRCMF_INFO_EVENT(ifp, event_msg, REASON_FMT, reason_formatter)                             \
  {                                                                                                \
    if (ifp == nullptr || event_msg == nullptr) {                                                  \
      BRCMF_INFO("Unable to log event %p for ifp %p", event_msg, ifp);                             \
    } else {                                                                                       \
      BRCMF_INFO("IF: %d event %s (%u)", ifp == nullptr ? -1 : ifp->ifidx,                         \
                 brcmf_fweh_event_name(static_cast<brcmf_fweh_event_code>(event_msg->event_code)), \
                 event_msg->event_code);                                                           \
      BRCMF_INFO("  status %s", brcmf_fweh_get_event_status_str(event_msg->status));               \
      BRCMF_INFO("  reason " REASON_FMT, reason_formatter(event_msg->reason));                     \
      BRCMF_INFO("    auth %s", brcmf_fweh_get_auth_type_str(event_msg->auth_type));               \
      BRCMF_INFO("   flags 0x%x", event_msg->flags);                                               \
    }                                                                                              \
  }

#define BRCMF_IFDBG(FILTER, ndev, fmt, ...)                                                      \
  BRCMF_DBG(FILTER, "%s(%d): " fmt, brcmf_cfg80211_get_iface_str(ndev), ndev_to_if(ndev)->ifidx, \
            ##__VA_ARGS__);

constexpr size_t kMaxHexDumpBytes = 4096;  // point at which output will be truncated
#define BRCMF_DBG_HEX_DUMP(condition, data, length, fmt, ...)            \
  do {                                                                   \
    if (condition) {                                                     \
      zxlogf(INFO, "(%s): " fmt, __func__, ##__VA_ARGS__);               \
      ::wlan::brcmfmac::Debug::PrintHexDump(DDK_LOG_INFO, data, length); \
    }                                                                    \
  } while (0)

constexpr size_t kMaxStringDumpBytes = 256;  // point at which output will be truncated
#define BRCMF_DBG_STRING_DUMP(condition, data, length, fmt, ...)            \
  do {                                                                      \
    if (condition) {                                                        \
      zxlogf(INFO, "(%s): " fmt, __func__, ##__VA_ARGS__);                  \
      ::wlan::brcmfmac::Debug::PrintStringDump(DDK_LOG_INFO, data, length); \
    }                                                                       \
  } while (0)

#define BRCMF_IS_ON(filter) \
  ::wlan::brcmfmac::Debug::IsFilterOn(::wlan::brcmfmac::Debug::Filter::k##filter)

#define THROTTLE(count, event)            \
  do {                                    \
    static std::atomic<unsigned> counter; \
    if (counter.fetch_add(1) <= count) {  \
      event;                              \
    }                                     \
  } while (0)

namespace wlan {
namespace brcmfmac {

// This class implements debugging functionality for the brcmfmac driver.
class Debug {
 public:
  enum class Filter : uint32_t {
    kTEMP = 1 << 0,
    kTRACE = 1 << 1,
    kINFO = 1 << 2,
    kDATA = 1 << 3,
    kCTL = 1 << 4,
    kTIMER = 1 << 5,
    kHDRS = 1 << 6,
    kBYTES = 1 << 7,
    kINTR = 1 << 8,
    kGLOM = 1 << 9,
    kEVENT = 1 << 10,
    kBTA = 1 << 11,
    kFIL = 1 << 12,
    kUSB = 1 << 13,
    kSCAN = 1 << 14,
    kCONN = 1 << 15,
    kBCDC = 1 << 16,
    kSDIO = 1 << 17,
    kPCIE = 1 << 18,
    kFWCON = 1 << 19,
    kSIM = 1 << 20,
    kWLANIF = 1 << 21,
    kSIMERRINJ = 1 << 22,
    kWLANPHY = 1 << 23,
    kALL = ~0u,
  };

  // Enabled debug log categories. Include WLANIF messages in the log output (at level INFO) to
  // aid in recognizing important events.
  // http://fxbug.dev/29792 - Remove WLANIF once things have stabilized.
  static constexpr uint32_t kBrcmfMsgFilter =
      static_cast<uint32_t>(Filter::kWLANIF) | static_cast<uint32_t>(Filter::kWLANPHY);

  // Check if a given debugging filter class is turned on.
  static constexpr bool IsFilterOn(Filter filter) {
    return (static_cast<uint32_t>(filter) & kBrcmfMsgFilter) != 0;
  }

  // Print a hexdump to the debugging output.
  static void PrintHexDump(uint32_t flag, const void* data, size_t length);

  // Print a string dump to the debugging output.
  static void PrintStringDump(uint32_t flag, const void* data, size_t length);

  // Create a memory dump.
  static void CreateMemoryDump(const void* data, size_t length);
};

}  // namespace brcmfmac
}  // namespace wlan

#endif  // SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_BROADCOM_BRCMFMAC_DEBUG_H_
