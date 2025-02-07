// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "hid-buttons.h"

#include <string.h>
#include <threads.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <cstdint>
#include <memory>

#include <ddk/debug.h>
#include <ddk/device.h>
#include <ddk/metadata.h>
#include <ddk/metadata/buttons.h>
#include <ddk/platform-defs.h>
#include <ddk/protocol/buttons.h>
#include <ddktl/protocol/composite.h>
#include <fbl/algorithm.h>
#include <fbl/alloc_checker.h>
#include <fbl/auto_call.h>
#include <hid/descriptor.h>

#include "src/ui/input/drivers/hid-buttons/hid-buttons-bind.h"

namespace buttons {

namespace {

uint32_t to_bit_mask(ButtonType type) { return 1 << static_cast<uint8_t>(type); }

// Takes in a BUTTON_ID_ value and returns a bitmask of ButtonTypes that are associated with this
// button id. Bit position corresponds to ButtonType e.g (1 << ButtonType::VOLUME_UP) is the bit
// for the volume_up button type.
uint32_t ButtonIdToButtonTypeBitMask(uint8_t button_id) {
  switch (button_id) {
    case BUTTONS_ID_VOLUME_UP:
      return to_bit_mask(ButtonType::VOLUME_UP);
    case BUTTONS_ID_VOLUME_DOWN:
      return to_bit_mask(ButtonType::VOLUME_DOWN);
    case BUTTONS_ID_FDR:
      return to_bit_mask(ButtonType::RESET);
    case BUTTONS_ID_MIC_MUTE:
      return to_bit_mask(ButtonType::MUTE);
    case BUTTONS_ID_PLAY_PAUSE:
      return to_bit_mask(ButtonType::PLAY_PAUSE);
    case BUTTONS_ID_KEY_A:
      return to_bit_mask(ButtonType::KEY_A);
    case BUTTONS_ID_KEY_M:
      return to_bit_mask(ButtonType::KEY_M);
    case BUTTONS_ID_CAM_MUTE:
      return to_bit_mask(ButtonType::CAM_MUTE);
    case BUTTONS_ID_MIC_AND_CAM_MUTE:
      return to_bit_mask(ButtonType::CAM_MUTE) | to_bit_mask(ButtonType::MUTE);
    default:
      return 0;
  }
}

bool input_reports_are_equal(const buttons_input_rpt_t& lhs, const buttons_input_rpt_t& rhs) {
  return (lhs.rpt_id == rhs.rpt_id && lhs.volume_up == rhs.volume_up &&
          lhs.volume_down == rhs.volume_down && lhs.reset == rhs.reset && lhs.mute == rhs.mute &&
          lhs.camera_access_disabled == rhs.camera_access_disabled);
}

}  // namespace

void HidButtonsDevice::Notify(uint32_t button_index) {
  // HID Report
  buttons_input_rpt_t input_rpt;
  size_t out_len;
  zx_status_t status =
      HidbusGetReport(0, BUTTONS_RPT_ID_INPUT, &input_rpt, sizeof(input_rpt), &out_len);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s HidbusGetReport failed %d", __FUNCTION__, status);
  } else if (!input_reports_are_equal(last_report_, input_rpt)) {
    fbl::AutoLock lock(&client_lock_);
    if (client_.is_valid()) {
      client_.IoQueue(&input_rpt, sizeof(buttons_input_rpt_t), zx_clock_get_monotonic());
      last_report_ = input_rpt;
    }
  }
  if (buttons_[button_index].id == BUTTONS_ID_FDR) {
    zxlogf(INFO, "FDR (up and down buttons) pressed");
  }

  // Notify anyone registered for this ButtonType.
  {
    fbl::AutoLock lock(&channels_lock_);
    uint32_t types = ButtonIdToButtonTypeBitMask(buttons_[button_index].id);
    bool button_value = debounce_states_[button_index].value;
    // Go through each ButtonType and send notifications.
    for (uint8_t raw_type = 0; raw_type < static_cast<uint8_t>(ButtonType::MAX); raw_type++) {
      if ((types & (1 << raw_type)) == 0) {
        continue;
      }

      ButtonType type = static_cast<ButtonType>(raw_type);
      for (ButtonsNotifyInterface* interface : registered_notifiers_[type]) {
        interface->binding()->OnNotify(type, button_value);
      }
    }
  }

  debounce_states_[button_index].enqueued = false;
}

int HidButtonsDevice::Thread() {
  while (1) {
    zx_port_packet_t packet;
    zx_status_t status = port_.wait(zx::time::infinite(), &packet);
    zxlogf(DEBUG, "%s msg received on port key %lu", __FUNCTION__, packet.key);
    if (status != ZX_OK) {
      zxlogf(ERROR, "%s port wait failed %d", __FUNCTION__, status);
      return thrd_error;
    }

    if (packet.key == kPortKeyShutDown) {
      zxlogf(INFO, "%s shutting down", __FUNCTION__);
      return thrd_success;
    }

    if (packet.key >= kPortKeyInterruptStart &&
        packet.key < (kPortKeyInterruptStart + buttons_.size())) {
      uint32_t type = static_cast<uint32_t>(packet.key - kPortKeyInterruptStart);
      if (gpios_[type].config.type == BUTTONS_GPIO_TYPE_INTERRUPT) {
        // We need to reconfigure the GPIO to catch the opposite polarity.
        debounce_states_[type].value = ReconfigurePolarity(type, packet.key);

        // Notify
        debounce_states_[type].timer.set(zx::deadline_after(zx::duration(kDebounceThresholdNs)),
                                         zx::duration(0));
        if (!debounce_states_[type].enqueued) {
          debounce_states_[type].timer.wait_async(port_, kPortKeyTimerStart + type,
                                                  ZX_TIMER_SIGNALED, 0);
        }
        debounce_states_[type].enqueued = true;
      }

      gpios_[type].irq.ack();
    }

    if (packet.key >= kPortKeyTimerStart && packet.key < (kPortKeyTimerStart + buttons_.size())) {
      Notify(static_cast<uint32_t>(packet.key - kPortKeyTimerStart));
    }
  }
  return thrd_success;
}

zx_status_t HidButtonsDevice::HidbusStart(const hidbus_ifc_protocol_t* ifc) {
  fbl::AutoLock lock(&client_lock_);
  if (client_.is_valid()) {
    return ZX_ERR_ALREADY_BOUND;
  }

  client_ = ddk::HidbusIfcProtocolClient(ifc);
  return ZX_OK;
}

zx_status_t HidButtonsDevice::HidbusQuery(uint32_t options, hid_info_t* info) {
  if (!info) {
    return ZX_ERR_INVALID_ARGS;
  }
  info->dev_num = 0;
  info->device_class = HID_DEVICE_CLASS_OTHER;
  info->boot_device = false;

  return ZX_OK;
}

void HidButtonsDevice::HidbusStop() {
  fbl::AutoLock lock(&client_lock_);
  client_.clear();
}

zx_status_t HidButtonsDevice::HidbusGetDescriptor(hid_description_type_t desc_type,
                                                  void* out_data_buffer, size_t data_size,
                                                  size_t* out_data_actual) {
  const uint8_t* desc;
  size_t desc_size = get_buttons_report_desc(&desc);
  if (data_size < desc_size) {
    return ZX_ERR_BUFFER_TOO_SMALL;
  }

  memcpy(out_data_buffer, desc, desc_size);
  *out_data_actual = desc_size;
  return ZX_OK;
}

// Requires interrupts to be disabled for all rows/cols.
bool HidButtonsDevice::MatrixScan(uint32_t row, uint32_t col, zx_duration_t delay) {
  gpio_config_in(&gpios_[col].gpio, GPIO_NO_PULL);  // Float column to find row in use.
  zx::nanosleep(zx::deadline_after(zx::duration(delay)));

  uint8_t val;
  gpio_read(&gpios_[row].gpio, &val);

  gpio_config_out(&gpios_[col].gpio, gpios_[col].config.output_value);
  zxlogf(DEBUG, "%s row %u col %u val %u", __FUNCTION__, row, col, val);
  return static_cast<bool>(val);
}

zx_status_t HidButtonsDevice::HidbusGetReport(uint8_t rpt_type, uint8_t rpt_id, void* data,
                                              size_t len, size_t* out_len) {
  if (!data || !out_len) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (rpt_id != BUTTONS_RPT_ID_INPUT) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  *out_len = sizeof(buttons_input_rpt_t);
  if (*out_len > len) {
    return ZX_ERR_BUFFER_TOO_SMALL;
  }

  buttons_input_rpt_t input_rpt = {};
  input_rpt.rpt_id = BUTTONS_RPT_ID_INPUT;

  for (size_t i = 0; i < buttons_.size(); ++i) {
    bool new_value = false;  // A value true means a button is pressed.
    if (buttons_[i].type == BUTTONS_TYPE_MATRIX) {
      new_value = MatrixScan(buttons_[i].gpioA_idx, buttons_[i].gpioB_idx, buttons_[i].gpio_delay);
    } else if (buttons_[i].type == BUTTONS_TYPE_DIRECT) {
      uint8_t val;
      gpio_read(&gpios_[buttons_[i].gpioA_idx].gpio, &val);
      zxlogf(DEBUG, "%s GPIO direct read %u for button %lu", __FUNCTION__, val, i);
      new_value = val;
    } else {
      zxlogf(ERROR, "%s unknown button type %u", __FUNCTION__, buttons_[i].type);
      return ZX_ERR_INTERNAL;
    }

    if (gpios_[i].config.flags & BUTTONS_GPIO_FLAG_INVERTED) {
      new_value = !new_value;
    }

    zxlogf(DEBUG, "%s GPIO new value %u for button %lu", __FUNCTION__, new_value, i);
    fill_button_in_report(buttons_[i].id, new_value, &input_rpt);
  }
  auto out = static_cast<buttons_input_rpt_t*>(data);
  *out = input_rpt;

  return ZX_OK;
}

zx_status_t HidButtonsDevice::HidbusSetReport(uint8_t rpt_type, uint8_t rpt_id, const void* data,
                                              size_t len) {
  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t HidButtonsDevice::HidbusGetIdle(uint8_t rpt_id, uint8_t* duration) {
  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t HidButtonsDevice::HidbusSetIdle(uint8_t rpt_id, uint8_t duration) {
  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t HidButtonsDevice::HidbusGetProtocol(uint8_t* protocol) { return ZX_ERR_NOT_SUPPORTED; }

zx_status_t HidButtonsDevice::HidbusSetProtocol(uint8_t protocol) { return ZX_OK; }

uint8_t HidButtonsDevice::ReconfigurePolarity(uint32_t idx, uint64_t int_port) {
  zxlogf(DEBUG, "%s gpio %u port %lu", __FUNCTION__, idx, int_port);
  uint8_t current = 0, old;
  gpio_read(&gpios_[idx].gpio, &current);
  do {
    gpio_set_polarity(&gpios_[idx].gpio, current ? GPIO_POLARITY_LOW : GPIO_POLARITY_HIGH);
    old = current;
    gpio_read(&gpios_[idx].gpio, &current);
    zxlogf(TRACE, "%s old gpio %u new gpio %u", __FUNCTION__, old, current);
    // If current switches after setup, we setup a new trigger for it (opposite edge).
  } while (current != old);
  return current;
}

zx_status_t HidButtonsDevice::ConfigureInterrupt(uint32_t idx, uint64_t int_port) {
  zxlogf(DEBUG, "%s gpio %u port %lu", __FUNCTION__, idx, int_port);
  zx_status_t status;
  uint8_t current = 0;
  gpio_read(&gpios_[idx].gpio, &current);
  gpio_release_interrupt(&gpios_[idx].gpio);
  // We setup a trigger for the opposite of the current GPIO value.
  status = gpio_get_interrupt(&gpios_[idx].gpio,
                              current ? ZX_INTERRUPT_MODE_EDGE_LOW : ZX_INTERRUPT_MODE_EDGE_HIGH,
                              gpios_[idx].irq.reset_and_get_address());
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s gpio_get_interrupt failed %d", __FUNCTION__, status);
    return status;
  }
  status = gpios_[idx].irq.bind(port_, int_port, 0);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s zx_interrupt_bind failed %d", __FUNCTION__, status);
    return status;
  }
  // To make sure polarity is correct in case it changed during configuration.
  ReconfigurePolarity(idx, int_port);
  return ZX_OK;
}

zx_status_t HidButtonsDevice::Bind(fbl::Array<Gpio> gpios,
                                   fbl::Array<buttons_button_config_t> buttons) {
  {
    fbl::AutoLock lock(&channels_lock_);
    for (uint8_t raw_type = 0; raw_type < static_cast<uint8_t>(ButtonType::MAX); raw_type++) {
      ButtonType type = static_cast<ButtonType>(raw_type);
      registered_notifiers_[type] = std::set<ButtonsNotifyInterface*>();
    }
  }
  zx_status_t status;

  buttons_ = std::move(buttons);
  gpios_ = std::move(gpios);
  fbl::AllocChecker ac;
  fbl::AutoLock lock(&channels_lock_);

  status = zx::port::create(ZX_PORT_BIND_TO_INTERRUPT, &port_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s port_create failed %d", __FUNCTION__, status);
    return status;
  }

  debounce_states_ = fbl::Array(new (&ac) debounce_state[buttons_.size()], buttons_.size());
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  for (auto& i : debounce_states_) {
    i.enqueued = false;
    zx::timer::create(0, ZX_CLOCK_MONOTONIC, &(i.timer));
    i.value = false;
  }

  // Check the metadata.
  for (uint32_t i = 0; i < buttons_.size(); ++i) {
    if (buttons_[i].gpioA_idx >= gpios_.size()) {
      zxlogf(ERROR, "%s invalid gpioA_idx %u", __FUNCTION__, buttons_[i].gpioA_idx);
      return ZX_ERR_INTERNAL;
    }
    if (buttons_[i].gpioB_idx >= gpios_.size()) {
      zxlogf(ERROR, "%s invalid gpioB_idx %u", __FUNCTION__, buttons_[i].gpioB_idx);
      return ZX_ERR_INTERNAL;
    }
    if (gpios_[buttons_[i].gpioA_idx].config.type != BUTTONS_GPIO_TYPE_INTERRUPT) {
      zxlogf(ERROR, "%s invalid gpioA type %u", __FUNCTION__,
             gpios_[buttons_[i].gpioA_idx].config.type);
      return ZX_ERR_INTERNAL;
    }
    if (buttons_[i].type == BUTTONS_TYPE_MATRIX &&
        gpios_[buttons_[i].gpioB_idx].config.type != BUTTONS_GPIO_TYPE_MATRIX_OUTPUT) {
      zxlogf(ERROR, "%s invalid matrix gpioB type %u", __FUNCTION__,
             gpios_[buttons_[i].gpioB_idx].config.type);
      return ZX_ERR_INTERNAL;
    }
    if (buttons_[i].id == BUTTONS_ID_FDR) {
      zxlogf(INFO, "FDR (up and down buttons) setup to GPIO %u", buttons_[i].gpioA_idx);
    }

    // Update the button_map_ array which maps ButtonTypes to the button.
    uint32_t types = ButtonIdToButtonTypeBitMask(buttons_[i].id);
    for (uint8_t raw_type = 0; raw_type < static_cast<uint8_t>(ButtonType::MAX); raw_type++) {
      if ((types & (1 << raw_type)) == 0) {
        continue;
      }

      ButtonType type = static_cast<ButtonType>(raw_type);
      button_map_[type] = i;
    }
  }

  // Setup.
  for (uint32_t i = 0; i < gpios_.size(); ++i) {
    status = gpio_set_alt_function(&gpios_[i].gpio, 0);  // 0 means function GPIO.
    if (status != ZX_OK) {
      zxlogf(ERROR, "%s gpio_set_alt_function failed %d", __FUNCTION__, status);
      return ZX_ERR_NOT_SUPPORTED;
    }
    if (gpios_[i].config.type == BUTTONS_GPIO_TYPE_MATRIX_OUTPUT) {
      status = gpio_config_out(&gpios_[i].gpio, gpios_[i].config.output_value);
      if (status != ZX_OK) {
        zxlogf(ERROR, "%s gpio_config_out failed %d", __FUNCTION__, status);
        return ZX_ERR_NOT_SUPPORTED;
      }
    } else if (gpios_[i].config.type == BUTTONS_GPIO_TYPE_INTERRUPT) {
      status = gpio_config_in(&gpios_[i].gpio, gpios_[i].config.internal_pull);
      if (status != ZX_OK) {
        zxlogf(ERROR, "%s gpio_config_in failed %d", __FUNCTION__, status);
        return ZX_ERR_NOT_SUPPORTED;
      }
      status = ConfigureInterrupt(i, kPortKeyInterruptStart + i);
      if (status != ZX_OK) {
        return status;
      }
    }
  }

  size_t out_len = 0;
  status = HidbusGetReport(0, BUTTONS_RPT_ID_INPUT, &last_report_, sizeof(last_report_), &out_len);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s HidbusGetReport failed %d", __FUNCTION__, status);
  }

  auto f = [](void* arg) -> int { return reinterpret_cast<HidButtonsDevice*>(arg)->Thread(); };
  int rc = thrd_create_with_name(&thread_, f, this, "hid-buttons-thread");
  if (rc != thrd_success) {
    return ZX_ERR_INTERNAL;
  }

  status = DdkAdd("hid-buttons", DEVICE_ADD_NON_BINDABLE);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s DdkAdd failed %d", __FUNCTION__, status);
    ShutDown();
    return status;
  }

  std::unique_ptr<HidButtonsHidBusFunction> hidbus_function(
      new (&ac) HidButtonsHidBusFunction(zxdev(), this));
  if (!ac.check()) {
    DdkAsyncRemove();
    return ZX_ERR_NO_MEMORY;
  }
  status = hidbus_function->DdkAdd("hidbus_function");
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s DdkAdd for Hidbus Function failed %d", __FUNCTION__, status);
    DdkAsyncRemove();
    return status;
  }
  hidbus_function_ = hidbus_function.release();

  std::unique_ptr<HidButtonsButtonsFunction> buttons_function(
      new (&ac) HidButtonsButtonsFunction(zxdev(), this));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  status = buttons_function->DdkAdd("buttons_function");
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s DdkAdd for Buttons Function failed %d", __FUNCTION__, status);
    DdkAsyncRemove();
    return status;
  }
  buttons_function_ = buttons_function.release();

  return ZX_OK;
}

void HidButtonsDevice::ShutDown() {
  zx_port_packet packet = {kPortKeyShutDown, ZX_PKT_TYPE_USER, ZX_OK, {}};
  zx_status_t status = port_.queue(&packet);
  ZX_ASSERT(status == ZX_OK);
  thrd_join(thread_, NULL);
  for (uint32_t i = 0; i < gpios_.size(); ++i) {
    gpios_[i].irq.destroy();
  }
  fbl::AutoLock lock(&client_lock_);
  client_.clear();

  hidbus_function_ = nullptr;
  buttons_function_ = nullptr;
}

void HidButtonsDevice::DdkUnbind(ddk::UnbindTxn txn) {
  ShutDown();
  txn.Reply();
}

void HidButtonsDevice::DdkRelease() { delete this; }

static zx_status_t hid_buttons_bind(void* ctx, zx_device_t* parent) {
  fbl::AllocChecker ac;
  auto dev = fbl::make_unique_checked<buttons::HidButtonsDevice>(&ac, parent);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  // Get buttons metadata.
  size_t actual = 0;
  auto status = device_get_metadata_size(parent, DEVICE_METADATA_BUTTONS_BUTTONS, &actual);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s device_get_metadata_size failed %d", __FILE__, status);
    return ZX_OK;
  }
  size_t n_buttons = actual / sizeof(buttons_button_config_t);
  auto buttons = fbl::Array(new (&ac) buttons_button_config_t[n_buttons], n_buttons);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  actual = 0;
  status = device_get_metadata(parent, DEVICE_METADATA_BUTTONS_BUTTONS, buttons.data(),
                               buttons.size() * sizeof(buttons_button_config_t), &actual);
  if (status != ZX_OK || actual != buttons.size() * sizeof(buttons_button_config_t)) {
    zxlogf(ERROR, "%s device_get_metadata failed %d", __FILE__, status);
    return status;
  }

  // Get gpios metadata.
  actual = 0;
  status = device_get_metadata_size(parent, DEVICE_METADATA_BUTTONS_GPIOS, &actual);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s device_get_metadata_size failed %d", __FILE__, status);
    return ZX_OK;
  }
  size_t n_gpios = actual / sizeof(buttons_gpio_config_t);
  auto configs = fbl::Array(new (&ac) buttons_gpio_config_t[n_gpios], n_gpios);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  actual = 0;
  status = device_get_metadata(parent, DEVICE_METADATA_BUTTONS_GPIOS, configs.data(),
                               configs.size() * sizeof(buttons_gpio_config_t), &actual);
  if (status != ZX_OK || actual != configs.size() * sizeof(buttons_gpio_config_t)) {
    zxlogf(ERROR, "%s device_get_metadata failed %d", __FILE__, status);
    return status;
  }

  // Get the GPIOs.
  ddk::CompositeProtocolClient composite(parent);
  if (!composite.is_valid()) {
    zxlogf(ERROR, "HidButtonsDevice: Could not get composite protocol");
    return ZX_ERR_NOT_SUPPORTED;
  }

  auto fragment_count = composite.GetFragmentCount();
  if (fragment_count != n_gpios) {
    zxlogf(ERROR, "%s Could not get fragment count", __func__);
    return ZX_ERR_INTERNAL;
  }
  composite_device_fragment_t fragments[fragment_count];
  composite.GetFragments(fragments, fragment_count, &actual);
  if (actual != fragment_count) {
    zxlogf(ERROR, "%s Fragment count did not match", __func__);
    return ZX_ERR_INTERNAL;
  }

  // Prepare gpios array.
  auto gpios = fbl::Array(new (&ac) HidButtonsDevice::Gpio[n_gpios], n_gpios);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  for (uint32_t i = 0; i < n_gpios; ++i) {
    status = device_get_protocol(fragments[i].device, ZX_PROTOCOL_GPIO, &gpios[i].gpio);
    if (status != ZX_OK) {
      zxlogf(ERROR, "%s Could not get protocol", __func__);
      return ZX_ERR_INTERNAL;
    }
    gpios[i].config = configs[i];
  }

  status = dev->Bind(std::move(gpios), std::move(buttons));
  if (status == ZX_OK) {
    // devmgr is now in charge of the memory for dev.
    __UNUSED auto ptr = dev.release();
  }
  return status;
}

zx_status_t HidButtonsDevice::ButtonsGetChannel(zx::channel chan, async_dispatcher_t* dispatcher) {
  fbl::AutoLock lock(&channels_lock_);

  interfaces_.emplace_back(this);
  auto status = interfaces_.back().Init(dispatcher, std::move(chan));
  if (status != ZX_OK)
    interfaces_.pop_back();
  return status;
}

bool HidButtonsDevice::GetState(ButtonType type) {
  uint8_t val;
  gpio_read(&gpios_[buttons_[button_map_[type]].gpioA_idx].gpio, &val);
  return static_cast<bool>(val);
}

zx_status_t HidButtonsDevice::RegisterNotify(uint8_t types, ButtonsNotifyInterface* notify) {
  fbl::AutoLock lock(&channels_lock_);
  // Go through each type in ButtonType and update our registration.
  for (uint8_t raw_type = 0; raw_type < static_cast<uint8_t>(ButtonType::MAX); raw_type++) {
    ButtonType type = static_cast<ButtonType>(raw_type);
    auto& notify_set = registered_notifiers_[type];
    if ((types & (1 << raw_type)) == 0) {
      // Our type does not exist in the bitmask and so we should de-register.
      notify_set.erase(notify);
    } else {
      // Our type exists in the bitmask and so we should register.
      notify_set.insert(notify);
    }
  }
  return ZX_OK;
}

void HidButtonsDevice::ClosingChannel(ButtonsNotifyInterface* notify) {
  fbl::AutoLock lock(&channels_lock_);
  // Remove this notifier from anything it's registered to listen to.
  for (auto& [type, notify_set] : registered_notifiers_) {
    notify_set.erase(notify);
  }

  // release ownership
  for (auto iter = interfaces_.begin(); iter != interfaces_.end(); ++iter) {
    if (&(*iter) == notify) {
      interfaces_.erase(iter);
      return;
    }
  }
  zxlogf(ERROR, "%s interfaces_ could not find channel", __func__);
}

static constexpr zx_driver_ops_t hid_buttons_driver_ops = []() {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = hid_buttons_bind;
  return ops;
}();

}  // namespace buttons

ZIRCON_DRIVER(hid_buttons, buttons::hid_buttons_driver_ops, "zircon", "0.1");
