// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use crate::prelude::*;
use crate::spinel::*;
use futures::prelude::*;

use anyhow::Error;
use fasync::Time;
use fuchsia_async::TimeoutExt;
use futures::TryFutureExt;
use lowpan_driver_common::{FutureExt as _, ZxResult};
use spinel_pack::TryOwnedUnpack;

/// Miscellaneous private methods
impl<DS: SpinelDeviceClient, NI: NetworkInterface> SpinelDriver<DS, NI> {
    /// This method is called whenever it is observed that the
    /// NCP is acting in a weird or spurious manner. This could
    /// be due to timeouts or bad byte packing, for example.
    pub(super) fn ncp_is_misbehaving(&self) {
        fx_log_err!("NCP is misbehaving.");

        // TODO: Add a counter?

        self.driver_state.lock().prepare_for_init();
        self.driver_state_change.trigger();
    }

    /// Waits for all of the returned values from
    /// previous calls to this method to go out of scope.
    ///
    /// This is used to ensure that certain API tasks
    /// do not execute simultaneously.
    ///
    /// The `description` field is used to describe who is holding
    /// the lock. It is used only for debugging purposes.
    pub(super) async fn wait_for_api_task_lock(
        &self,
        _description: &'static str,
    ) -> ZxResult<futures::lock::MutexGuard<'_, ()>> {
        // In the future, `description` will be used for debugging purposes.
        Ok(self.exclusive_task_lock.lock().await)
    }

    /// Decorates the given future with error mapping,
    /// reset handling, and a standard timeout.
    pub(super) fn apply_standard_combinators<'a, F>(
        &'a self,
        future: F,
    ) -> impl Future<Output = ZxResult<F::Ok>> + 'a
    where
        F: TryFuture<Error = Error> + Unpin + Send + 'a,
        <F as TryFuture>::Ok: Send,
    {
        future
            .map_err(|e| ZxStatus::from(ErrorAdapter(e)))
            .cancel_upon(self.ncp_did_reset.wait(), Err(ZxStatus::CANCELED))
            .on_timeout(Time::after(DEFAULT_TIMEOUT), ncp_cmd_timeout!(self))
    }

    /// Returns a future that gets a property and returns the value.
    pub(super) fn get_property_simple<T: TryOwnedUnpack + 'static, P: Into<Prop>>(
        &self,
        prop: P,
    ) -> impl Future<Output = ZxResult<T::Unpacked>> + '_ {
        self.apply_standard_combinators(
            self.frame_handler.send_request(CmdPropValueGet(prop.into()).returning::<T>()).boxed(),
        )
    }
}

/// State synchronization
impl<DS: SpinelDeviceClient, NI: NetworkInterface> SpinelDriver<DS, NI> {
    fn print_ncp_debug_line(&self, line: &[u8]) {
        match std::str::from_utf8(line) {
            Ok(line) => fx_log_warn!("NCP-DEBUG: {:?}", line),
            Err(_) => fx_log_warn!("NCP-DEBUG: Non-UTF8: {:x?}", line),
        }
    }

    /// Handler for keeping track of property value changes
    /// so that local state stays in sync with the device.
    pub(super) fn on_prop_value_is(&self, prop: Prop, mut value: &[u8]) -> Result<(), Error> {
        fx_log_info!("on_prop_value_is: {:?} {:02x?}", prop, value);
        match prop {
            Prop::Stream(PropStream::Debug) => {
                let mut ncp_debug_buffer = self.ncp_debug_buffer.lock();

                if value.ends_with(&[0]) {
                    value = &value[..value.len() - 1];
                }

                // Append the data to the end of the existing debug buffer.
                std::io::Write::write_all(&mut *ncp_debug_buffer, value)
                    .expect("Unable to write to NCP debug buffer");

                // Look for newlines and print out all of the complete lines.
                while ncp_debug_buffer.contains(&b'\n') {
                    let line = ncp_debug_buffer.split(|c| *c == b'\n').next().unwrap();
                    self.print_ncp_debug_line(line);
                    let len = line.len() + 1; // +1 to remove the `\n`
                    ncp_debug_buffer.drain(..len);
                }

                // If our buffer is still larger than our max buffer size, go ahead
                // and break up the line.
                while ncp_debug_buffer.len() >= MAX_NCP_DEBUG_LINE_LEN {
                    self.print_ncp_debug_line(&ncp_debug_buffer[..MAX_NCP_DEBUG_LINE_LEN]);
                    ncp_debug_buffer.drain(..MAX_NCP_DEBUG_LINE_LEN);
                }
            }

            Prop::Net(PropNet::Saved) => {
                let saved = bool::try_unpack_from_slice(value)?;
                let mut driver_state = self.driver_state.lock();
                let new_connectivity_state = if saved {
                    driver_state.connectivity_state.provisioned()
                } else {
                    driver_state.connectivity_state.unprovisioned()
                };
                if new_connectivity_state != driver_state.connectivity_state {
                    let old_connectivity_state = driver_state.connectivity_state;
                    driver_state.connectivity_state = new_connectivity_state;
                    std::mem::drop(driver_state);
                    self.driver_state_change.trigger();
                    self.on_connectivity_state_change(
                        new_connectivity_state,
                        old_connectivity_state,
                    );
                }
            }

            Prop::Net(PropNet::Role) => {
                let new_role = match NetRole::try_unpack_from_slice(value)? {
                    NetRole::Detached => Role::Detached,
                    NetRole::Child => Role::EndDevice,
                    NetRole::Router => Role::Router,
                    NetRole::Leader => Role::Leader,
                    NetRole::Unknown(_) => Role::EndDevice,
                };

                let mut driver_state = self.driver_state.lock();

                if new_role != driver_state.role {
                    fx_log_info!("Role changed from {:?} to {:?}", driver_state.role, new_role);

                    driver_state.role = new_role;

                    let new_connectivity_state =
                        driver_state.connectivity_state.role_updated(new_role);
                    if new_connectivity_state != driver_state.connectivity_state {
                        let old_connectivity_state = driver_state.connectivity_state;
                        driver_state.connectivity_state = new_connectivity_state;
                        std::mem::drop(driver_state);
                        self.driver_state_change.trigger();
                        self.on_connectivity_state_change(
                            new_connectivity_state,
                            old_connectivity_state,
                        );
                    } else {
                        std::mem::drop(driver_state);
                        self.driver_state_change.trigger();
                    }
                }
            }

            Prop::Net(PropNet::NetworkName) => {
                // Get a mutable version of our value so we can
                // remove any trailing zeros.
                let mut value = value;

                // Skip trailing zeros.
                while value.last() == Some(&0) {
                    value = &value[..value.len() - 1];
                }

                let mut driver_state = self.driver_state.lock();

                if Some(true)
                    != driver_state.identity.raw_name.as_ref().map(|x| x.as_slice() == value)
                {
                    fx_log_info!(
                        "Network name changed from {:?} to {:?}",
                        driver_state.identity.raw_name,
                        value
                    );
                    driver_state.identity.raw_name = Some(value.to_vec());
                    std::mem::drop(driver_state);
                    self.driver_state_change.trigger();
                }
            }

            Prop::Net(PropNet::Xpanid) => {
                let value = if value.is_empty() {
                    None
                } else if value.len() == 8 {
                    Some(value)
                } else {
                    return Err(format_err!(
                        "Invalid XPANID from NCP: {:?} (Must by 8 bytes)",
                        value
                    ));
                };

                let mut driver_state = self.driver_state.lock();

                if Some(true)
                    != driver_state.identity.xpanid.as_ref().map(|x| Some(x.as_slice()) == value)
                {
                    fx_log_info!(
                        "XPANID changed from {:?} to {:?}",
                        driver_state.identity.xpanid,
                        value
                    );
                    driver_state.identity.xpanid = value.map(Vec::<u8>::from);
                    std::mem::drop(driver_state);
                    self.driver_state_change.trigger();
                }
            }

            Prop::Phy(PropPhy::Chan) => {
                let value = u8::try_unpack_from_slice(value)? as u16;

                let mut driver_state = self.driver_state.lock();

                if Some(value) != driver_state.identity.channel {
                    fx_log_info!(
                        "Channel changed from {:?} to {:?}",
                        driver_state.identity.channel,
                        value
                    );
                    driver_state.identity.channel = Some(value);
                    std::mem::drop(driver_state);
                    self.driver_state_change.trigger();
                }
            }

            Prop::Mac(PropMac::Panid) => {
                let value = u16::try_unpack_from_slice(value)?;

                let mut driver_state = self.driver_state.lock();

                if Some(value) != driver_state.identity.panid {
                    fx_log_info!(
                        "PANID changed from {:?} to {:?}",
                        driver_state.identity.channel,
                        value
                    );
                    driver_state.identity.panid = Some(value);
                    std::mem::drop(driver_state);
                    self.driver_state_change.trigger();
                }
            }

            Prop::Ipv6(PropIpv6::LlAddr) => {
                let value = std::net::Ipv6Addr::try_unpack_from_slice(value)?;

                let mut driver_state = self.driver_state.lock();

                if value == driver_state.link_local_addr {
                    return Ok(());
                }

                if driver_state.connectivity_state.is_online() {
                    if !driver_state.link_local_addr.is_unspecified() {
                        let subnet = Subnet {
                            addr: driver_state.link_local_addr,
                            prefix_len: STD_IPV6_NET_PREFIX_LEN,
                        };
                        if let Err(err) = self.net_if.remove_address(&subnet) {
                            fx_log_err!(
                                "Unable to remove address `{}` from interface: {:?}",
                                subnet.addr,
                                err
                            );
                        }
                    }

                    if !value.is_unspecified() {
                        let subnet = Subnet { addr: value, prefix_len: STD_IPV6_NET_PREFIX_LEN };
                        if let Err(err) = self.net_if.add_address(&subnet) {
                            fx_log_err!(
                                "Unable to add address `{}` to interface: {:?}",
                                subnet.addr,
                                err
                            );
                        } else {
                            driver_state
                                .address_table
                                .insert(AddressTableEntry { subnet, ..Default::default() });
                        }
                    }
                }

                driver_state.link_local_addr = value;
                std::mem::drop(driver_state);
                self.driver_state_change.trigger();
            }

            Prop::Ipv6(PropIpv6::MlAddr) => {
                let value = std::net::Ipv6Addr::try_unpack_from_slice(value)?;

                let mut driver_state = self.driver_state.lock();

                if value == driver_state.mesh_local_addr {
                    return Ok(());
                }

                if driver_state.connectivity_state.is_online() {
                    if !driver_state.mesh_local_addr.is_unspecified() {
                        let subnet = Subnet {
                            addr: driver_state.mesh_local_addr,
                            prefix_len: STD_IPV6_NET_PREFIX_LEN,
                        };
                        if let Err(err) = self.net_if.remove_address(&subnet) {
                            fx_log_err!(
                                "Unable to remove address `{}` from interface: {:?}",
                                subnet.addr,
                                err
                            );
                        }
                    }

                    if !value.is_unspecified() {
                        let subnet = Subnet { addr: value, prefix_len: STD_IPV6_NET_PREFIX_LEN };
                        if let Err(err) = self.net_if.add_address(&subnet) {
                            fx_log_err!(
                                "Unable to add address `{}` to interface: {:?}",
                                subnet.addr,
                                err
                            );
                        } else {
                            driver_state
                                .address_table
                                .insert(AddressTableEntry { subnet, ..Default::default() });
                        }
                    }
                }

                driver_state.mesh_local_addr = value;
                std::mem::drop(driver_state);
                self.driver_state_change.trigger();
            }

            Prop::Ipv6(PropIpv6::AddressTable) => {
                let value = AddressTable::try_unpack_from_slice(value)?;

                let mut driver_state = self.driver_state.lock();

                fx_log_info!("New address table: {:#?}", value);

                if value != driver_state.address_table {
                    if driver_state.connectivity_state.is_online() {
                        for changed_address in
                            driver_state.address_table.symmetric_difference(&value)
                        {
                            if value.contains(changed_address)
                                && !driver_state.addr_is_mesh_local(&changed_address.subnet.addr)
                            {
                                if let Err(err) = self.net_if.add_address(&changed_address.subnet) {
                                    fx_log_err!(
                                        "Unable to add address `{:?}` to interface: {:?}",
                                        changed_address.subnet,
                                        err
                                    );
                                }
                            } else {
                                if let Err(err) =
                                    self.net_if.remove_address(&changed_address.subnet)
                                {
                                    fx_log_err!(
                                        "Unable to remove address `{:?}` from interface: {:?}",
                                        changed_address.subnet,
                                        err
                                    );
                                }
                            }
                        }
                    }
                    driver_state.address_table = value;
                    std::mem::drop(driver_state);
                    self.driver_state_change.trigger();
                }
            }

            _ => {}
        }
        Ok(())
    }

    /// Handler for keeping track of property value insertions
    /// so that local state stays in sync with the device.
    pub(super) fn on_prop_value_inserted(&self, prop: Prop, value: &[u8]) -> Result<(), Error> {
        fx_log_info!("on_prop_value_inserted: {:?} {:02X?}", prop, value);
        Ok(())
    }

    /// Handler for keeping track of property value removals
    /// so that local state stays in sync with the device.
    pub(super) fn on_prop_value_removed(&self, prop: Prop, value: &[u8]) -> Result<(), Error> {
        fx_log_info!("on_prop_value_removed: {:?} {:02X?}", prop, value);
        Ok(())
    }
}
