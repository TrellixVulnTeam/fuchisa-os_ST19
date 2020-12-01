// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_wlan_common as fidl_common;
use fidl_fuchsia_wlan_internal as fidl_internal;
use fidl_fuchsia_wlan_mlme::{self as fidl_mlme, BandCapabilities};

pub fn clone_device_info(d: &fidl_mlme::DeviceInfo) -> fidl_mlme::DeviceInfo {
    fidl_mlme::DeviceInfo {
        mac_addr: d.mac_addr,
        role: d.role,
        bands: clone_bands(&d.bands),
        driver_features: d.driver_features.clone(),
        qos_capable: d.qos_capable,
    }
}

pub fn clone_ht_capabilities(c: &fidl_internal::HtCapabilities) -> fidl_internal::HtCapabilities {
    fidl_internal::HtCapabilities { bytes: c.bytes.clone() }
}

pub fn clone_ht_operation(o: &fidl_internal::HtOperation) -> fidl_internal::HtOperation {
    fidl_internal::HtOperation { bytes: o.bytes.clone() }
}

pub fn clone_vht_capabilities(
    c: &fidl_internal::VhtCapabilities,
) -> fidl_internal::VhtCapabilities {
    fidl_internal::VhtCapabilities { bytes: c.bytes.clone() }
}

pub fn clone_vht_operation(o: &fidl_internal::VhtOperation) -> fidl_internal::VhtOperation {
    fidl_internal::VhtOperation { bytes: o.bytes.clone() }
}

pub fn clone_bss_desc(d: &fidl_internal::BssDescription) -> fidl_internal::BssDescription {
    fidl_internal::BssDescription {
        bssid: d.bssid.clone(),
        ssid: d.ssid.clone(),
        bss_type: d.bss_type,
        beacon_period: d.beacon_period,
        dtim_period: d.dtim_period,
        timestamp: d.timestamp,
        local_time: d.local_time,

        cap: d.cap,
        rates: d.rates.clone(),
        country: d.country.clone(),

        rsne: d.rsne.clone(),
        vendor_ies: d.vendor_ies.clone(),

        ht_cap: d.ht_cap.as_ref().map(|v| Box::new(clone_ht_capabilities(v))),
        ht_op: d.ht_op.as_ref().map(|v| Box::new(clone_ht_operation(v))),

        vht_cap: d.vht_cap.as_ref().map(|v| Box::new(clone_vht_capabilities(v))),
        vht_op: d.vht_op.as_ref().map(|v| Box::new(clone_vht_operation(v))),

        chan: fidl_common::WlanChan {
            primary: d.chan.primary,
            cbw: d.chan.cbw,
            secondary80: d.chan.secondary80,
        },
        rssi_dbm: d.rssi_dbm,
        snr_db: d.snr_db,
    }
}

pub fn clone_band_cap(b: &BandCapabilities) -> BandCapabilities {
    BandCapabilities {
        band_id: b.band_id,
        rates: b.rates.clone(),
        base_frequency: b.base_frequency,
        channels: b.channels.clone(),
        ht_cap: b.ht_cap.as_ref().map(|v| Box::new(clone_ht_capabilities(v))),
        vht_cap: b.vht_cap.as_ref().map(|v| Box::new(clone_vht_capabilities(v))),
        cap: b.cap,
    }
}

pub fn clone_bands(bv: &Vec<BandCapabilities>) -> Vec<BandCapabilities> {
    bv.iter().map(clone_band_cap).collect()
}

pub fn clone_mesh_configuration(c: &fidl_mlme::MeshConfiguration) -> fidl_mlme::MeshConfiguration {
    fidl_mlme::MeshConfiguration { ..*c }
}

pub fn clone_mesh_peering_common(c: &fidl_mlme::MeshPeeringCommon) -> fidl_mlme::MeshPeeringCommon {
    fidl_mlme::MeshPeeringCommon {
        mesh_id: c.mesh_id.clone(),
        mesh_config: clone_mesh_configuration(&c.mesh_config),
        rates: c.rates.clone(),
        ht_cap: c.ht_cap.as_ref().map(|x| Box::new(clone_ht_capabilities(x))),
        ht_op: c.ht_op.as_ref().map(|x| Box::new(clone_ht_operation(x))),
        vht_cap: c.vht_cap.as_ref().map(|x| Box::new(clone_vht_capabilities(x))),
        vht_op: c.vht_op.as_ref().map(|x| Box::new(clone_vht_operation(x))),
        ..*c
    }
}

pub fn clone_scan_request(sr: &fidl_mlme::ScanRequest) -> fidl_mlme::ScanRequest {
    fidl_mlme::ScanRequest {
        ssid: sr.ssid.clone(),
        channel_list: sr.channel_list.clone(),
        ssid_list: sr.ssid_list.clone(),
        ..*sr
    }
}
