// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Manages Scan requests for the Client Policy API.
use {
    crate::{
        client::types, mode_management::iface_manager_api::IfaceManagerApi,
        util::sme_conversion::security_from_sme_protection,
    },
    anyhow::{format_err, Error},
    async_trait::async_trait,
    fidl_fuchsia_location_sensor as fidl_location_sensor, fidl_fuchsia_wlan_policy as fidl_policy,
    fidl_fuchsia_wlan_sme as fidl_sme,
    fuchsia_component::client::connect_to_service,
    futures::{lock::Mutex, prelude::*},
    log::{debug, error, info},
    std::{collections::HashMap, sync::Arc},
    stream::FuturesUnordered,
};

// Arbitrary count of networks (ssid/security pairs) to output per request
const OUTPUT_CHUNK_NETWORK_COUNT: usize = 5;

/// Allows for consumption of updated scan results.
#[async_trait]
pub trait ScanResultUpdate: Sync + Send {
    async fn update_scan_results(&mut self, scan_results: &Vec<types::ScanResult>);
}

/// Requests a new SME scan and returns the results.
async fn sme_scan(
    iface_manager: Arc<Mutex<dyn IfaceManagerApi + Send>>,
    scan_request: fidl_sme::ScanRequest,
) -> Result<Vec<fidl_sme::BssInfo>, ()> {
    let txn = {
        let mut iface_manager = iface_manager.lock().await;
        match iface_manager.scan(scan_request).await {
            Ok(txn) => txn,
            Err(error) => {
                error!("Scan initiation error: {:?}", error);
                return Err(());
            }
        }
    };
    debug!("Sent scan request to SME successfully");
    let mut stream = txn.take_event_stream();
    let mut scanned_networks = vec![];
    while let Some(Ok(event)) = stream.next().await {
        match event {
            fidl_sme::ScanTransactionEvent::OnResult { aps: new_aps } => {
                debug!("Received scan results from SME");
                scanned_networks.extend(new_aps);
            }
            fidl_sme::ScanTransactionEvent::OnFinished {} => {
                debug!("Finished getting scan results from SME");
                return Ok(scanned_networks);
            }
            fidl_sme::ScanTransactionEvent::OnError { error } => {
                error!("Scan error from SME: {:?}", error);
                return Err(());
            }
        };
    }
    error!("SME closed scan result channel without sending OnFinished");
    Err(())
}

/// Handles incoming scan requests by creating a new SME scan request.
/// For the output_iterator, returns scan results and/or errors.
/// On successful scan, also provides scan results to:
/// - Emergency Location Provider
/// - Network Selection Module
pub(crate) async fn perform_scan<F>(
    iface_manager: Arc<Mutex<dyn IfaceManagerApi + Send>>,
    mut output_iterator: Option<fidl::endpoints::ServerEnd<fidl_policy::ScanResultIteratorMarker>>,
    mut network_selector: impl ScanResultUpdate,
    mut location_sensor_updater: impl ScanResultUpdate,
    active_scan_decider: F,
) where
    F: FnOnce(&Vec<types::ScanResult>) -> Option<Vec<Vec<u8>>>,
{
    let mut bss_by_network: HashMap<fidl_policy::NetworkIdentifier, Vec<types::Bss>> =
        HashMap::new();

    // Perform an initial passive scan
    let scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
    let sme_result = sme_scan(Arc::clone(&iface_manager), scan_request).await;
    match sme_result {
        Ok(results) => {
            insert_bss_to_network_bss_map(&mut bss_by_network, results, true);
        }
        Err(()) => {
            // The passive scan failed. Send an error to the requester and return early.
            if let Some(output_iterator) = output_iterator {
                send_scan_error(output_iterator, fidl_policy::ScanErrorCode::GeneralError)
                    .await
                    .unwrap_or_else(|e| error!("Failed to send scan error: {}", e));
            }
            return;
        }
    };

    // Determine which active scans to perform by asking the active_scan_decider()
    if let Some(requested_active_scan_ssids) =
        active_scan_decider(&network_bss_map_to_scan_result(&bss_by_network))
    {
        let scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: requested_active_scan_ssids,
            channels: vec![],
        });
        let sme_result = sme_scan(iface_manager, scan_request).await;
        match sme_result {
            Ok(results) => {
                insert_bss_to_network_bss_map(&mut bss_by_network, results, false);
            }
            Err(()) => {
                // There was an error in the active scan. For the FIDL interface, send an error. We
                // `.take()` the output_iterator here, so it won't be used for sending results below.
                if let Some(output_iterator) = output_iterator.take() {
                    send_scan_error(output_iterator, fidl_policy::ScanErrorCode::GeneralError)
                        .await
                        .unwrap_or_else(|e| error!("Failed to send scan error: {}", e));
                };
                info!("Proceeding with passive scan results for non-FIDL scan consumers");
            }
        }
    };

    let scan_results = network_bss_map_to_scan_result(&bss_by_network);
    let mut scan_result_consumers = FuturesUnordered::new();

    // Send scan results to the location sensor
    scan_result_consumers.push(location_sensor_updater.update_scan_results(&scan_results));
    // Send scan results to the network selection module
    scan_result_consumers.push(network_selector.update_scan_results(&scan_results));
    // If the requester provided a channel, send the results to them
    if let Some(output_iterator) = output_iterator {
        let requester_fut = send_scan_results(output_iterator, &scan_results).unwrap_or_else(|e| {
            error!("Failed to send scan results to requester: {:?}", e);
        });
        scan_result_consumers.push(Box::pin(requester_fut));
    }

    while let Some(_) = scan_result_consumers.next().await {}
}

/// Perform a directed active scan for a given network on given channels.
pub(crate) async fn perform_directed_active_scan(
    iface_manager: Arc<Mutex<dyn IfaceManagerApi + Send>>,
    network: &types::NetworkIdentifier,
    channels: Option<Vec<u8>>,
) -> Result<Vec<types::ScanResult>, ()> {
    let scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
        ssids: vec![network.ssid.clone()],
        channels: channels.unwrap_or(vec![]),
    });

    let sme_result = sme_scan(Arc::clone(&iface_manager), scan_request).await;

    sme_result.map(|results| {
        let mut bss_by_network: HashMap<types::NetworkIdentifier, Vec<types::Bss>> = HashMap::new();
        insert_bss_to_network_bss_map(&mut bss_by_network, results, false);

        // The active scan targets a specific SSID, but we want to return only results for the
        // requested NetworkIdentifier (i.e. SSID + Security tuple).
        bss_by_network.retain(|network_id, _| network_id == network);

        network_bss_map_to_scan_result(&bss_by_network)
    })
}

/// The location sensor module uses scan results to help determine the
/// device's location, for use by the Emergency Location Provider.
pub struct LocationSensorUpdater {}
#[async_trait]
impl ScanResultUpdate for LocationSensorUpdater {
    async fn update_scan_results(&mut self, scan_results: &Vec<types::ScanResult>) {
        async fn send_results(scan_results: &Vec<types::ScanResult>) -> Result<(), Error> {
            // Get an output iterator
            let (iter, server) =
                fidl::endpoints::create_endpoints::<fidl_policy::ScanResultIteratorMarker>()
                    .map_err(|err| format_err!("failed to create iterator: {:?}", err))?;
            let location_watcher_proxy =
                connect_to_service::<fidl_location_sensor::WlanBaseStationWatcherMarker>()
                    .map_err(|err| {
                        format_err!("failed to connect to location sensor service: {:?}", err)
                    })?;
            location_watcher_proxy
                .report_current_stations(iter)
                .map_err(|err| format_err!("failed to call location sensor service: {:?}", err))?;

            // Send results to the iterator
            send_scan_results(server, scan_results).await
        }

        // Filter out any errors and just log a message.
        // No error recovery, we'll just try again next time a scan result comes in.
        if let Err(e) = send_results(scan_results).await {
            // TODO(fxbug.dev/52700) Upgrade this to a "warn!" once the location sensor works.
            debug!("Failed to send scan results to location sensor: {:?}", e)
        } else {
            debug!("Updated location sensor")
        };
    }
}

/// Converts sme::BssInfo to our internal BSS type, then adds it to the provided bss_by_network map.
/// Only keeps the first unique instance of a BSSID
fn insert_bss_to_network_bss_map(
    bss_by_network: &mut HashMap<fidl_policy::NetworkIdentifier, Vec<types::Bss>>,
    new_bss: Vec<fidl_sme::BssInfo>,
    observed_in_passive_scan: bool,
) {
    for bss in new_bss {
        if let Some(security) = security_from_sme_protection(bss.protection) {
            let entry = bss_by_network
                .entry(fidl_policy::NetworkIdentifier { ssid: bss.ssid.to_vec(), type_: security })
                .or_insert(vec![]);
            // Check if this BSSID is already in the hashmap
            if !entry.iter().any(|existing_bss| existing_bss.bssid == bss.bssid) {
                entry.push(types::Bss {
                    bssid: bss.bssid,
                    rssi: bss.rssi_dbm,
                    snr_db: bss.snr_db,
                    frequency: 0, // TODO(mnck): convert channel to freq
                    channel: bss.channel,
                    timestamp_nanos: 0, // TODO(mnck): find where this comes from
                    observed_in_passive_scan,
                    compatible: bss.compatible,
                    bss_desc: bss.bss_desc,
                });
            };
        } else {
            // TODO(mnck): log a metric here
            debug!("Unknown security type present in scan results: {:?}", bss.protection);
        }
    }
}

fn network_bss_map_to_scan_result(
    bss_by_network: &HashMap<fidl_policy::NetworkIdentifier, Vec<types::Bss>>,
) -> Vec<types::ScanResult> {
    let mut scan_results: Vec<types::ScanResult> = bss_by_network
        .iter()
        .map(|(network, bss_infos)| types::ScanResult {
            id: network.clone(),
            entries: bss_infos.to_vec(),
            compatibility: if bss_infos.iter().any(|bss| bss.compatible) {
                fidl_policy::Compatibility::Supported
            } else {
                fidl_policy::Compatibility::DisallowedNotSupported
            },
        })
        .collect();

    scan_results.sort_by(|a, b| a.id.ssid.cmp(&b.id.ssid));
    return scan_results;
}

/// Send batches of results to the output iterator when getNext() is called on it.
/// Close the channel when no results are remaining.
async fn send_scan_results(
    output_iterator: fidl::endpoints::ServerEnd<fidl_policy::ScanResultIteratorMarker>,
    scan_results: &Vec<types::ScanResult>,
) -> Result<(), Error> {
    let mut chunks = scan_results.chunks(OUTPUT_CHUNK_NETWORK_COUNT);
    let mut sent_some_results = false;
    // Wait to get a request for a chunk of scan results
    let (mut stream, ctrl) = output_iterator.into_stream_and_control_handle()?;
    loop {
        if let Some(fidl_policy::ScanResultIteratorRequest::GetNext { responder }) =
            stream.try_next().await?
        {
            sent_some_results = true;
            if let Some(chunk) = chunks.next() {
                let mut next_result: fidl_policy::ScanResultIteratorGetNextResult = Ok(chunk
                    .into_iter()
                    .map(|r| fidl_policy::ScanResult::from(r.clone()))
                    .collect());
                responder.send(&mut next_result)?;
            } else {
                // When no results are left, send an empty vec and close the channel.
                let mut next_result: fidl_policy::ScanResultIteratorGetNextResult = Ok(vec![]);
                responder.send(&mut next_result)?;
                ctrl.shutdown();
                break;
            }
        } else {
            // This will happen if the iterator request stream was closed and we expected to send
            // another response.
            if sent_some_results {
                // Some consumers may not care about all scan results, e.g. if they find the
                // particular network they were looking for. This is not an error.
                debug!("Scan result consumer closed channel before consuming all scan results");
                return Ok(());
            } else {
                return Err(format_err!("Peer closed channel before receiving any scan results"));
            }
        }
    }
    Ok(())
}

/// On the next request for results, send an error to the output iterator and
/// shut it down.
async fn send_scan_error(
    output_iterator: fidl::endpoints::ServerEnd<fidl_policy::ScanResultIteratorMarker>,
    error_code: fidl_policy::ScanErrorCode,
) -> Result<(), fidl::Error> {
    // Wait to get a request for a chunk of scan results
    let (mut stream, ctrl) = output_iterator.into_stream_and_control_handle()?;
    if let Some(req) = stream.try_next().await? {
        let fidl_policy::ScanResultIteratorRequest::GetNext { responder } = req;
        let mut err: fidl_policy::ScanResultIteratorGetNextResult = Err(error_code);
        responder.send(&mut err)?;
        ctrl.shutdown();
    } else {
        // This will happen if the iterator request stream was closed and we expected to send
        // another response.
        info!("Peer closed channel for getting scan results unexpectedly");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            access_point::state_machine as ap_fsm,
            client::state_machine as client_fsm,
            util::{clone::clone_bss_info, logger::set_logger_for_test},
        },
        anyhow::Error,
        fidl::endpoints::{create_proxy, Proxy},
        fidl_fuchsia_wlan_common as fidl_common, fuchsia_async as fasync, fuchsia_zircon as zx,
        futures::{channel::oneshot, lock::Mutex, task::Poll},
        pin_utils::pin_mut,
        rand::Rng as _,
        std::{convert::TryInto as _, sync::Arc},
        wlan_common::assert_variant,
    };

    struct FakeIfaceManager {
        pub sme_proxy: fidl_fuchsia_wlan_sme::ClientSmeProxy,
    }

    impl FakeIfaceManager {
        pub fn new(proxy: fidl_fuchsia_wlan_sme::ClientSmeProxy) -> Self {
            FakeIfaceManager { sme_proxy: proxy }
        }
    }

    #[async_trait]
    impl IfaceManagerApi for FakeIfaceManager {
        async fn disconnect(
            &mut self,
            _network_id: fidl_fuchsia_wlan_policy::NetworkIdentifier,
        ) -> Result<(), Error> {
            unimplemented!()
        }

        async fn connect(
            &mut self,
            _connect_req: client_fsm::ConnectRequest,
        ) -> Result<oneshot::Receiver<()>, Error> {
            unimplemented!()
        }

        async fn record_idle_client(&mut self, _iface_id: u16) -> Result<(), Error> {
            unimplemented!()
        }

        async fn has_idle_client(&mut self) -> Result<bool, Error> {
            unimplemented!()
        }

        async fn handle_added_iface(&mut self, _iface_id: u16) -> Result<(), Error> {
            unimplemented!()
        }

        async fn handle_removed_iface(&mut self, _iface_id: u16) -> Result<(), Error> {
            unimplemented!()
        }

        async fn scan(
            &mut self,
            mut scan_request: fidl_sme::ScanRequest,
        ) -> Result<fidl_fuchsia_wlan_sme::ScanTransactionProxy, Error> {
            let (local, remote) = fidl::endpoints::create_proxy()?;
            let _ = self.sme_proxy.scan(&mut scan_request, remote);
            Ok(local)
        }

        async fn stop_client_connections(&mut self) -> Result<(), Error> {
            unimplemented!()
        }

        async fn start_client_connections(&mut self) -> Result<(), Error> {
            unimplemented!()
        }

        async fn start_ap(
            &mut self,
            _config: ap_fsm::ApConfig,
        ) -> Result<oneshot::Receiver<()>, Error> {
            unimplemented!()
        }

        async fn stop_ap(&mut self, _ssid: Vec<u8>, _password: Vec<u8>) -> Result<(), Error> {
            unimplemented!()
        }

        async fn stop_all_aps(&mut self) -> Result<(), Error> {
            unimplemented!()
        }
    }

    /// Creates a Client wrapper.
    async fn create_iface_manager(
    ) -> (Arc<Mutex<FakeIfaceManager>>, fidl_sme::ClientSmeRequestStream) {
        set_logger_for_test();
        let (client_sme, remote) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("error creating proxy");
        let iface_manager = Arc::new(Mutex::new(FakeIfaceManager::new(client_sme)));
        (iface_manager, remote.into_stream().expect("failed to create stream"))
    }

    struct MockScanResultConsumer {
        scan_results: Arc<Mutex<Option<Vec<types::ScanResult>>>>,
    }
    impl MockScanResultConsumer {
        fn new() -> (Self, Arc<Mutex<Option<Vec<types::ScanResult>>>>) {
            let scan_results = Arc::new(Mutex::new(None));
            (Self { scan_results: Arc::clone(&scan_results) }, scan_results)
        }
    }
    #[async_trait]
    impl ScanResultUpdate for MockScanResultConsumer {
        async fn update_scan_results(&mut self, scan_results: &Vec<types::ScanResult>) {
            let mut guard = self.scan_results.lock().await;
            *guard = Some(scan_results.clone());
        }
    }

    fn validate_sme_request_and_send_results(
        exec: &mut fasync::Executor,
        sme_stream: &mut fidl_sme::ClientSmeRequestStream,
        expected_scan_request: &fidl_sme::ScanRequest,
        scan_results: &[fidl_sme::BssInfo],
    ) {
        // Check that a scan request was sent to the sme and send back results
        assert_variant!(
            exec.run_until_stalled(&mut sme_stream.next()),
            Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                txn, req, control_handle: _
            }))) => {
                // Validate the request
                assert_eq!(req, *expected_scan_request);
                // Send all the APs
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                let mut scan_results = scan_results.iter().map(clone_bss_info).collect::<Vec<_>>();
                ctrl.send_on_result(&mut scan_results.iter_mut())
                    .expect("failed to send scan data");

                // Send the end of data
                ctrl.send_on_finished()
                    .expect("failed to send scan data");
            }
        );
    }

    // Creates test data for the scan functions.
    struct MockScanData {
        passive_input_aps: Vec<fidl_sme::BssInfo>,
        passive_internal_aps: Vec<types::ScanResult>,
        passive_fidl_aps: Vec<fidl_policy::ScanResult>,
        active_input_aps: Vec<fidl_sme::BssInfo>,
        combined_internal_aps: Vec<types::ScanResult>,
        combined_fidl_aps: Vec<fidl_policy::ScanResult>,
    }
    fn create_scan_ap_data() -> MockScanData {
        let passive_input_aps = vec![
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 0,
                snr_db: 1,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: None,
            },
            fidl_sme::BssInfo {
                bssid: [1, 2, 3, 4, 5, 6],
                ssid: "unique ssid".as_bytes().to_vec(),
                rssi_dbm: 7,
                snr_db: 2,
                channel: fidl_common::WlanChan {
                    primary: 8,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa2Personal,
                compatible: true,
                bss_desc: None,
            },
            fidl_sme::BssInfo {
                bssid: [7, 8, 9, 10, 11, 12],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 13,
                snr_db: 3,
                channel: fidl_common::WlanChan {
                    primary: 11,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: false,
                bss_desc: None,
            },
        ];
        // input_aps contains some duplicate SSIDs, which should be
        // grouped in the output.
        let passive_internal_aps = vec![
            types::ScanResult {
                id: types::NetworkIdentifier {
                    ssid: "duplicated ssid".as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa3,
                },
                entries: vec![
                    types::Bss {
                        bssid: [0, 0, 0, 0, 0, 0],
                        rssi: 0,
                        frequency: 0,
                        timestamp_nanos: 0,
                        snr_db: 1,
                        channel: fidl_common::WlanChan {
                            primary: 1,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        observed_in_passive_scan: true,
                        compatible: true,
                        bss_desc: None,
                    },
                    types::Bss {
                        bssid: [7, 8, 9, 10, 11, 12],
                        rssi: 13,
                        frequency: 0,
                        timestamp_nanos: 0,
                        snr_db: 3,
                        channel: fidl_common::WlanChan {
                            primary: 11,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        observed_in_passive_scan: true,
                        compatible: false,
                        bss_desc: None,
                    },
                ],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                id: types::NetworkIdentifier {
                    ssid: "unique ssid".as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                entries: vec![types::Bss {
                    bssid: [1, 2, 3, 4, 5, 6],
                    rssi: 7,
                    frequency: 0,
                    timestamp_nanos: 0,
                    snr_db: 2,
                    channel: fidl_common::WlanChan {
                        primary: 8,
                        cbw: fidl_common::Cbw::Cbw20,
                        secondary80: 0,
                    },
                    observed_in_passive_scan: true,
                    compatible: true,
                    bss_desc: None,
                }],
                compatibility: types::Compatibility::Supported,
            },
        ];
        let passive_fidl_aps = vec![
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "duplicated ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa3,
                }),
                entries: Some(vec![
                    fidl_policy::Bss {
                        bssid: Some([0, 0, 0, 0, 0, 0]),
                        rssi: Some(0),
                        frequency: Some(0),
                        timestamp_nanos: Some(0),
                        ..fidl_policy::Bss::empty()
                    },
                    fidl_policy::Bss {
                        bssid: Some([7, 8, 9, 10, 11, 12]),
                        rssi: Some(13),
                        frequency: Some(0),
                        timestamp_nanos: Some(0),
                        ..fidl_policy::Bss::empty()
                    },
                ]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::empty()
            },
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "unique ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa2,
                }),
                entries: Some(vec![fidl_policy::Bss {
                    bssid: Some([1, 2, 3, 4, 5, 6]),
                    rssi: Some(7),
                    frequency: Some(0),
                    timestamp_nanos: Some(0),
                    ..fidl_policy::Bss::empty()
                }]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::empty()
            },
        ];

        let active_input_aps = vec![
            fidl_sme::BssInfo {
                bssid: [9, 9, 9, 9, 9, 9],
                ssid: "foo active ssid".as_bytes().to_vec(),
                rssi_dbm: 0,
                snr_db: 8,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: None,
            },
            fidl_sme::BssInfo {
                bssid: [8, 8, 8, 8, 8, 8],
                ssid: "misc ssid".as_bytes().to_vec(),
                rssi_dbm: 7,
                snr_db: 9,
                channel: fidl_common::WlanChan {
                    primary: 8,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa2Personal,
                compatible: true,
                bss_desc: None,
            },
        ];
        let combined_internal_aps = vec![
            types::ScanResult {
                id: types::NetworkIdentifier {
                    ssid: "duplicated ssid".as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa3,
                },
                entries: vec![
                    types::Bss {
                        bssid: [0, 0, 0, 0, 0, 0],
                        rssi: 0,
                        frequency: 0,
                        timestamp_nanos: 0,
                        snr_db: 1,
                        channel: fidl_common::WlanChan {
                            primary: 1,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        observed_in_passive_scan: true,
                        compatible: true,
                        bss_desc: None,
                    },
                    types::Bss {
                        bssid: [7, 8, 9, 10, 11, 12],
                        rssi: 13,
                        frequency: 0,
                        timestamp_nanos: 0,
                        snr_db: 3,
                        channel: fidl_common::WlanChan {
                            primary: 11,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        observed_in_passive_scan: true,
                        compatible: false,
                        bss_desc: None,
                    },
                ],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                id: types::NetworkIdentifier {
                    ssid: "foo active ssid".as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa3,
                },
                entries: vec![types::Bss {
                    bssid: [9, 9, 9, 9, 9, 9],
                    rssi: 0,
                    frequency: 0,
                    timestamp_nanos: 0,
                    snr_db: 8,
                    channel: fidl_common::WlanChan {
                        primary: 1,
                        cbw: fidl_common::Cbw::Cbw20,
                        secondary80: 0,
                    },
                    observed_in_passive_scan: false,
                    compatible: true,
                    bss_desc: None,
                }],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                id: types::NetworkIdentifier {
                    ssid: "misc ssid".as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                entries: vec![types::Bss {
                    bssid: [8, 8, 8, 8, 8, 8],
                    rssi: 7,
                    frequency: 0,
                    timestamp_nanos: 0,
                    snr_db: 9,
                    channel: fidl_common::WlanChan {
                        primary: 8,
                        cbw: fidl_common::Cbw::Cbw20,
                        secondary80: 0,
                    },
                    observed_in_passive_scan: false,
                    compatible: true,
                    bss_desc: None,
                }],
                compatibility: types::Compatibility::Supported,
            },
            types::ScanResult {
                id: types::NetworkIdentifier {
                    ssid: "unique ssid".as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                entries: vec![types::Bss {
                    bssid: [1, 2, 3, 4, 5, 6],
                    rssi: 7,
                    frequency: 0,
                    timestamp_nanos: 0,
                    snr_db: 2,
                    channel: fidl_common::WlanChan {
                        primary: 8,
                        cbw: fidl_common::Cbw::Cbw20,
                        secondary80: 0,
                    },
                    observed_in_passive_scan: true,
                    compatible: true,
                    bss_desc: None,
                }],
                compatibility: types::Compatibility::Supported,
            },
        ];
        let combined_fidl_aps = vec![
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "duplicated ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa3,
                }),
                entries: Some(vec![
                    fidl_policy::Bss {
                        bssid: Some([0, 0, 0, 0, 0, 0]),
                        rssi: Some(0),
                        frequency: Some(0),
                        timestamp_nanos: Some(0),
                        ..fidl_policy::Bss::empty()
                    },
                    fidl_policy::Bss {
                        bssid: Some([7, 8, 9, 10, 11, 12]),
                        rssi: Some(13),
                        frequency: Some(0),
                        timestamp_nanos: Some(0),
                        ..fidl_policy::Bss::empty()
                    },
                ]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::empty()
            },
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "foo active ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa3,
                }),
                entries: Some(vec![fidl_policy::Bss {
                    bssid: Some([9, 9, 9, 9, 9, 9]),
                    rssi: Some(0),
                    frequency: Some(0),
                    timestamp_nanos: Some(0),
                    ..fidl_policy::Bss::empty()
                }]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::empty()
            },
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "misc ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa2,
                }),
                entries: Some(vec![fidl_policy::Bss {
                    bssid: Some([8, 8, 8, 8, 8, 8]),
                    rssi: Some(7),
                    frequency: Some(0),
                    timestamp_nanos: Some(0),
                    ..fidl_policy::Bss::empty()
                }]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::empty()
            },
            fidl_policy::ScanResult {
                id: Some(fidl_policy::NetworkIdentifier {
                    ssid: "unique ssid".as_bytes().to_vec(),
                    type_: fidl_policy::SecurityType::Wpa2,
                }),
                entries: Some(vec![fidl_policy::Bss {
                    bssid: Some([1, 2, 3, 4, 5, 6]),
                    rssi: Some(7),
                    frequency: Some(0),
                    timestamp_nanos: Some(0),
                    ..fidl_policy::Bss::empty()
                }]),
                compatibility: Some(fidl_policy::Compatibility::Supported),
                ..fidl_policy::ScanResult::empty()
            },
        ];

        MockScanData {
            passive_input_aps,
            passive_internal_aps,
            passive_fidl_aps,
            active_input_aps,
            combined_internal_aps,
            combined_fidl_aps,
        }
    }

    #[test]
    fn sme_scan_with_passive_request() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());

        // Issue request to scan.
        let scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let scan_fut = sme_scan(client, scan_request.clone());
        pin_mut!(scan_fut);

        // Request scan data from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: _,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        // Validate the SME received the scan_request and send back mock data
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &scan_request,
            &input_aps,
        );

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            assert_eq!(result, Ok(input_aps));
        });
    }

    #[test]
    fn sme_scan_with_active_request() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());

        // Issue request to scan.
        let scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec!["foo_ssid".as_bytes().to_vec(), "bar_ssid".as_bytes().to_vec()],
            channels: vec![1, 20],
        });
        let scan_fut = sme_scan(client, scan_request.clone());
        pin_mut!(scan_fut);

        // Request scan data from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: _,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        // Validate the SME received the scan_request and send back mock data
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &scan_request,
            &input_aps,
        );

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            assert_eq!(result, Ok(input_aps));
        });
    }

    #[test]
    fn sme_scan_error() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());

        // Issue request to scan.
        let scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let scan_fut = sme_scan(client, scan_request);
        pin_mut!(scan_fut);

        // Request scan data from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        assert_variant!(
                exec.run_until_stalled(&mut sme_stream.next()),
                Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                    txn, ..
                }))) => {
                    // Send failed scan response.
                    let (_stream, ctrl) = txn
                        .into_stream_and_control_handle().expect("error accessing control handle");
                    ctrl.send_on_error(&mut fidl_sme::ScanError {
                        code: fidl_sme::ScanErrorCode::InternalError,
                        message: "Failed to scan".to_string()
                    })
                        .expect("failed to send scan error");
                }
            );

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            assert_eq!(result, Err(()));
        });
    }

    #[test]
    fn sme_scan_channel_closed() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());

        // Issue request to scan.
        let scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let scan_fut = sme_scan(client, scan_request);
        pin_mut!(scan_fut);

        // Request scan data from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        assert_variant!(
                exec.run_until_stalled(&mut sme_stream.next()),
                Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                    txn, ..
                }))) => {
                    // Send failed scan response.
                    txn.close_with_epitaph(zx::Status::OK).expect("Failed to close channel");
                }
            );

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            assert_eq!(result, Err(()));
        });
    }

    #[test]
    fn basic_scan() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut =
            perform_scan(client, Some(iter_server), network_selector, location_sensor, |_| None);
        pin_mut!(scan_fut);

        // Request a chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Pending);
        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data and send it via the SME
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: internal_aps,
            passive_fidl_aps: fidl_aps,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            &input_aps,
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, fidl_aps);
        });

        // Request the next chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();

        // Process scan handler
        // Note: this will be Poll::Ready because the scan handler will exit after sending the final
        // scan results.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, vec![]);
        });

        // Check both successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results.lock()),
            Some(internal_aps.clone())
        );
    }

    #[test]
    fn scan_with_active_scan_decider() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();

        // Create the passive and active scan info
        let MockScanData {
            passive_input_aps,
            passive_internal_aps,
            passive_fidl_aps: _,
            active_input_aps,
            combined_internal_aps,
            combined_fidl_aps,
        } = create_scan_ap_data();

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let expected_passive_results = passive_internal_aps.clone();
        let scan_fut = perform_scan(
            client,
            Some(iter_server),
            network_selector,
            location_sensor,
            |passive_results| {
                assert_eq!(*passive_results, expected_passive_results);
                Some(vec!["foo active ssid".as_bytes().to_vec()])
            },
        );
        pin_mut!(scan_fut);

        // Request a chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Pending);
        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Respond to the first (passive) scan request
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            &passive_input_aps,
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Respond to the second (active) scan request
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec!["foo active ssid".as_bytes().to_vec()],
            channels: vec![],
        });
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            &active_input_aps,
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, combined_fidl_aps);
        });

        // Request the next chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();

        // Process scan handler
        // Note: this will be Poll::Ready because the scan handler will exit after sending the final
        // scan results.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, vec![]);
        });

        // Check both successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results.lock()),
            Some(combined_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results.lock()),
            Some(combined_internal_aps.clone())
        );
    }

    #[test]
    fn insert_bss_to_network_bss_map_duplicated_bss() {
        let mut bss_by_network = HashMap::new();

        // Create some input data with duplicated BSSID and Network Identifiers
        let passive_input_aps = vec![
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 0,
                snr_db: 1,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: None,
            },
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 13,
                snr_db: 3,
                channel: fidl_common::WlanChan {
                    primary: 14,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: None,
            },
        ];

        let expected_id = types::NetworkIdentifier {
            ssid: "duplicated ssid".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa3,
        };

        // We should only see one entry for the duplicated BSSs in the passive scan results
        let expected_bss = vec![types::Bss {
            bssid: [0, 0, 0, 0, 0, 0],
            rssi: 0,
            frequency: 0,
            timestamp_nanos: 0,
            snr_db: 1,
            channel: fidl_common::WlanChan {
                primary: 1,
                cbw: fidl_common::Cbw::Cbw20,
                secondary80: 0,
            },
            observed_in_passive_scan: true,
            compatible: true,
            bss_desc: None,
        }];

        insert_bss_to_network_bss_map(&mut bss_by_network, passive_input_aps, true);
        assert_eq!(bss_by_network.len(), 1);
        assert_eq!(bss_by_network[&expected_id], expected_bss);

        // Create some input data with one duplicate BSSID and one new BSSID
        let active_input_aps = vec![
            fidl_sme::BssInfo {
                bssid: [0, 0, 0, 0, 0, 0],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 100,
                snr_db: 100,
                channel: fidl_common::WlanChan {
                    primary: 100,
                    cbw: fidl_common::Cbw::Cbw40,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: None,
            },
            fidl_sme::BssInfo {
                bssid: [1, 2, 3, 4, 5, 6],
                ssid: "duplicated ssid".as_bytes().to_vec(),
                rssi_dbm: 101,
                snr_db: 101,
                channel: fidl_common::WlanChan {
                    primary: 101,
                    cbw: fidl_common::Cbw::Cbw40,
                    secondary80: 0,
                },
                protection: fidl_sme::Protection::Wpa3Enterprise,
                compatible: true,
                bss_desc: None,
            },
        ];

        // After the active scan, there should be a second bss included in the results
        let expected_bss = vec![
            types::Bss {
                bssid: [0, 0, 0, 0, 0, 0],
                rssi: 0,
                frequency: 0,
                timestamp_nanos: 0,
                snr_db: 1,
                channel: fidl_common::WlanChan {
                    primary: 1,
                    cbw: fidl_common::Cbw::Cbw20,
                    secondary80: 0,
                },
                observed_in_passive_scan: true,
                compatible: true,
                bss_desc: None,
            },
            types::Bss {
                bssid: [1, 2, 3, 4, 5, 6],
                rssi: 101,
                frequency: 0,
                timestamp_nanos: 0,
                snr_db: 101,
                channel: fidl_common::WlanChan {
                    primary: 101,
                    cbw: fidl_common::Cbw::Cbw40,
                    secondary80: 0,
                },
                observed_in_passive_scan: false,
                compatible: true,
                bss_desc: None,
            },
        ];

        insert_bss_to_network_bss_map(&mut bss_by_network, active_input_aps, false);
        assert_eq!(bss_by_network.len(), 1);
        assert_eq!(bss_by_network[&expected_id], expected_bss);
    }

    #[test]
    fn scan_with_active_scan_decider_and_active_scan_failure() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();

        // Create the passive and active scan info
        let MockScanData {
            passive_input_aps,
            passive_internal_aps,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let expected_passive_results = passive_internal_aps.clone();
        let scan_fut = perform_scan(
            client,
            Some(iter_server),
            network_selector,
            location_sensor,
            |passive_results| {
                assert_eq!(*passive_results, expected_passive_results);
                Some(vec!["foo active ssid".as_bytes().to_vec()])
            },
        );
        pin_mut!(scan_fut);

        // Request a chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Pending);
        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Respond to the first (passive) scan request
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            &passive_input_aps,
        );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec!["foo active ssid".as_bytes().to_vec()],
            channels: vec![],
        });
        assert_variant!(
            exec.run_until_stalled(&mut sme_stream.next()),
            Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                txn, req, ..
            }))) => {
                assert_eq!(req, expected_scan_request);
                // Send failed scan response.
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_error(&mut fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::InternalError,
                    message: "Failed to scan".to_string()
                })
                    .expect("failed to send scan error");
            }
        );

        // Process scan handler
        // Note: this will be Poll::Ready because the scan handler will exit after sending the final
        // scan results.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // Check the FIDL result -- this should be an error, since the active scan failed
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let result = result.expect("Failed to get next scan results").unwrap_err();
            assert_eq!(result, fidl_policy::ScanErrorCode::GeneralError);
        });

        // Check both scan consumers got just the passive scan results, since the active scan failed
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results.lock()),
            Some(passive_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results.lock()),
            Some(passive_internal_aps.clone())
        );
    }

    #[test]
    fn scan_iterator_never_polled() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector1, network_selector_results1) = MockScanResultConsumer::new();
        let (location_sensor1, location_sensor_results1) = MockScanResultConsumer::new();
        let (network_selector2, network_selector_results2) = MockScanResultConsumer::new();
        let (location_sensor2, location_sensor_results2) = MockScanResultConsumer::new();

        // Issue request to scan.
        let (_iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut = perform_scan(
            client.clone(),
            Some(iter_server),
            network_selector1,
            location_sensor1,
            |_| None,
        );
        pin_mut!(scan_fut);

        // Progress scan side forward without ever calling getNext() on the scan result iterator
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data and send it via the SME
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: internal_aps,
            passive_fidl_aps: fidl_aps,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            &input_aps,
        );

        // Progress scan side forward without progressing the scan result iterator
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Issue a second request to scan, to make sure that everything is still
        // moving along even though the first scan result iterator was never progressed.
        let (iter2, iter_server2) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut2 =
            perform_scan(client, Some(iter_server2), network_selector2, location_sensor2, |_| None);
        pin_mut!(scan_fut2);

        // Progress scan side forward
        assert_variant!(exec.run_until_stalled(&mut scan_fut2), Poll::Pending);

        // Create mock scan data and send it via the SME
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            &input_aps,
        );

        // Request the results on the second iterator
        let mut output_iter_fut2 = iter2.get_next();

        // Progress scan side forward
        assert_variant!(exec.run_until_stalled(&mut scan_fut2), Poll::Pending);

        // Ensure results are present on the iterator
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut2), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, fidl_aps);
        });

        // Check all successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results1.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results1.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results2.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results2.lock()),
            Some(internal_aps.clone())
        );
    }

    #[test]
    fn scan_iterator_shut_down() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut =
            perform_scan(client, Some(iter_server), network_selector, location_sensor, |_| None);
        pin_mut!(scan_fut);

        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Create mock scan data and send it via the SME
        let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
        let MockScanData {
            passive_input_aps: input_aps,
            passive_internal_aps: internal_aps,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            &input_aps,
        );

        // Close the channel
        drop(iter.into_channel());

        // Process scan handler
        // Note: this will be Poll::Ready because the scan handler will exit since all the consumers are done
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // Check both successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results.lock()),
            Some(internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results.lock()),
            Some(internal_aps.clone())
        );
    }

    #[test]
    fn scan_error() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector, network_selector_results) = MockScanResultConsumer::new();
        let (location_sensor, location_sensor_results) = MockScanResultConsumer::new();

        // Issue request to scan.
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let scan_fut =
            perform_scan(client, Some(iter_server), network_selector, location_sensor, |_| None);
        pin_mut!(scan_fut);

        // Request a chunk of scan results. Progress until waiting on response from server side of
        // the iterator.
        let mut output_iter_fut = iter.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Pending);
        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Check that a scan request was sent to the sme and send back an error
        assert_variant!(
                exec.run_until_stalled(&mut sme_stream.next()),
                Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                    txn, ..
                }))) => {
                    // Send failed scan response.
                    let (_stream, ctrl) = txn
                        .into_stream_and_control_handle().expect("error accessing control handle");
                    ctrl.send_on_error(&mut fidl_sme::ScanError {
                        code: fidl_sme::ScanErrorCode::InternalError,
                        message: "Failed to scan".to_string()
                    })
                        .expect("failed to send scan error");
                }
            );

        // Process SME result.
        // Note: this will be Poll::Ready, since the scan handler will quit after sending the error
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(()));

        // the iterator should have an error on it
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results");
            assert_eq!(results, Err(fidl_policy::ScanErrorCode::GeneralError));
        });

        // Check both successful scan consumers have no results
        assert_eq!(*exec.run_singlethreaded(network_selector_results.lock()), None);
        assert_eq!(*exec.run_singlethreaded(location_sensor_results.lock()), None);
    }

    #[test]
    fn overlapping_scans() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());
        let (network_selector1, network_selector_results1) = MockScanResultConsumer::new();
        let (location_sensor1, location_sensor_results1) = MockScanResultConsumer::new();
        let (network_selector2, network_selector_results2) = MockScanResultConsumer::new();
        let (location_sensor2, location_sensor_results2) = MockScanResultConsumer::new();

        let MockScanData {
            passive_input_aps,
            passive_internal_aps,
            passive_fidl_aps,
            active_input_aps,
            combined_internal_aps,
            combined_fidl_aps,
        } = create_scan_ap_data();

        // Create two sets of endpoints
        let (iter0, iter_server0) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let (iter1, iter_server1) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");

        // Issue request to scan on both iterator.
        let scan_fut0 = perform_scan(
            client.clone(),
            Some(iter_server0),
            network_selector1,
            location_sensor1,
            |_| None,
        );
        pin_mut!(scan_fut0);
        let scan_fut1 = perform_scan(
            client.clone(),
            Some(iter_server1),
            network_selector2,
            location_sensor2,
            |passive_results| {
                assert_eq!(*passive_results, passive_internal_aps);
                Some(vec!["foo active ssid".as_bytes().to_vec()])
            },
        );
        pin_mut!(scan_fut1);

        // Request a chunk of scan results on both iterators. Progress until waiting on
        // response from server side of the iterator.
        let mut output_iter_fut0 = iter0.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut0), Poll::Pending);
        let mut output_iter_fut1 = iter1.get_next();
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut1), Poll::Pending);

        // Progress first scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut0), Poll::Pending);

        // Check that a scan request was sent to the sme and send back results
        assert_variant!(
                exec.run_until_stalled(&mut sme_stream.next()),
                Poll::Ready(Some(Ok(fidl_sme::ClientSmeRequest::Scan {
                    txn, ..
                }))) => {
                    // Send the first AP
                    let (_stream, ctrl) = txn
                        .into_stream_and_control_handle().expect("error accessing control handle");
                    let mut aps = [clone_bss_info(&passive_input_aps[0])];
                    ctrl.send_on_result(&mut aps.iter_mut())
                        .expect("failed to send scan data");
                    // Process SME result.
                    assert_variant!(exec.run_until_stalled(&mut scan_fut0), Poll::Pending);
                    // The iterator should not have any data yet, until the sme is done
                    assert_variant!(exec.run_until_stalled(&mut output_iter_fut0), Poll::Pending);

                    // Progress second scan handler forward so that it will respond to the iterator get next request.
                    assert_variant!(exec.run_until_stalled(&mut scan_fut1), Poll::Pending);
                    // Check that the second scan request was sent to the sme and send back results
                    let expected_scan_request = fidl_sme::ScanRequest::Passive(fidl_sme::PassiveScanRequest {});
                    validate_sme_request_and_send_results(&mut exec, &mut sme_stream, &expected_scan_request, &passive_input_aps); // for output_iter_fut1
                    // Process SME result.
                    assert_variant!(exec.run_until_stalled(&mut scan_fut1), Poll::Pending);
                    // The second request should now result in an active scan
                    let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
                        channels: vec![],
                        ssids: vec!["foo active ssid".as_bytes().to_vec()],
                    });
                    validate_sme_request_and_send_results(&mut exec, &mut sme_stream, &expected_scan_request, &active_input_aps); // for output_iter_fut1
                    // Process SME result.
                    assert_variant!(exec.run_until_stalled(&mut scan_fut1), Poll::Pending);// The second iterator should have all its data

                    assert_variant!(exec.run_until_stalled(&mut output_iter_fut1), Poll::Ready(result) => {
                        let results = result.expect("Failed to get next scan results").unwrap();
                        assert_eq!(results.len(), combined_fidl_aps.len());
                        assert_eq!(results, combined_fidl_aps);
                    });

                    // Send the remaining APs for the first iterator
                    let mut aps = passive_input_aps[1..].iter().map(clone_bss_info).collect::<Vec<_>>();
                    ctrl.send_on_result(&mut aps.iter_mut())
                        .expect("failed to send scan data");
                    // Process SME result.
                    assert_variant!(exec.run_until_stalled(&mut scan_fut0), Poll::Pending);
                    // Send the end of data
                    ctrl.send_on_finished()
                        .expect("failed to send scan data");
                }
            );

        // Process response from SME
        assert_variant!(exec.run_until_stalled(&mut scan_fut0), Poll::Pending);

        // The first iterator should have all its data
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut0), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results.len(), passive_fidl_aps.len());
            assert_eq!(results, passive_fidl_aps);
        });

        // Check both successful scan consumers got results
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results1.lock()),
            Some(passive_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results1.lock()),
            Some(passive_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(network_selector_results2.lock()),
            Some(combined_internal_aps.clone())
        );
        assert_eq!(
            *exec.run_singlethreaded(location_sensor_results2.lock()),
            Some(combined_internal_aps.clone())
        );
    }

    // TODO(fxbug.dev/54255): Separate test case for "empty final vector not consumed" vs "partial ap list"
    // consumed.
    #[test]
    fn partial_scan_result_consumption_has_no_error() {
        set_logger_for_test();
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let MockScanData {
            passive_input_aps,
            passive_internal_aps: _,
            passive_fidl_aps: _,
            active_input_aps,
            combined_internal_aps: _,
            combined_fidl_aps: fidl_aps,
        } = create_scan_ap_data();

        let mut bss_by_network: HashMap<fidl_policy::NetworkIdentifier, Vec<types::Bss>> =
            HashMap::new();
        insert_bss_to_network_bss_map(&mut bss_by_network, passive_input_aps, true);
        insert_bss_to_network_bss_map(&mut bss_by_network, active_input_aps, false);
        let scan_results = network_bss_map_to_scan_result(&bss_by_network);

        // Create an iterator and send scan results
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let send_fut = send_scan_results(iter_server, &scan_results);
        pin_mut!(send_fut);

        // Request a chunk of scan results.
        let mut output_iter_fut = iter.get_next();

        // Send first chunk of scan results
        assert_variant!(exec.run_until_stalled(&mut send_fut), Poll::Pending);

        // Make sure the first chunk of results were delivered
        assert_variant!(exec.run_until_stalled(&mut output_iter_fut), Poll::Ready(result) => {
            let results = result.expect("Failed to get next scan results").unwrap();
            assert_eq!(results, fidl_aps);
        });

        // Close the channel without getting remaining results
        // Note: as of the writing of this test, the "remaining results" are just the final message
        // with an empty vector of networks that signify the end of results. That final empty vector
        // is still considered part of the results, so this test successfully exercises the
        // "partial results read" path.
        drop(output_iter_fut);
        drop(iter);

        // This should not result in error, since some results were consumed
        assert_variant!(exec.run_until_stalled(&mut send_fut), Poll::Ready(Ok(())));
    }

    #[test]
    fn no_scan_result_consumption_has_error() {
        set_logger_for_test();
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let MockScanData {
            passive_input_aps,
            passive_internal_aps: _,
            passive_fidl_aps: _,
            active_input_aps,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();

        let mut bss_by_network: HashMap<types::NetworkIdentifier, Vec<types::Bss>> = HashMap::new();
        insert_bss_to_network_bss_map(&mut bss_by_network, passive_input_aps, true);
        insert_bss_to_network_bss_map(&mut bss_by_network, active_input_aps, false);
        let scan_results = network_bss_map_to_scan_result(&bss_by_network);

        // Create an iterator and send scan results
        let (iter, iter_server) =
            fidl::endpoints::create_proxy().expect("failed to create iterator");
        let send_fut = send_scan_results(iter_server, &scan_results);
        pin_mut!(send_fut);

        // Close the channel without getting results
        drop(iter);

        // This should result in error, since no results were consumed
        assert_variant!(exec.run_until_stalled(&mut send_fut), Poll::Ready(Err(_)));
    }

    fn generate_random_bss_info() -> fidl_sme::BssInfo {
        let mut rng = rand::thread_rng();
        let bssid = (0..6).map(|_| rng.gen::<u8>()).collect::<Vec<u8>>();
        fidl_sme::BssInfo {
            bssid: bssid.as_slice().try_into().unwrap(),
            ssid: format!("scan result rand {}", rng.gen::<i32>()).as_bytes().to_vec(),
            rssi_dbm: rng.gen_range(-100, 20),
            channel: types::WlanChan {
                primary: rng.gen_range(1, 255),
                cbw: fidl_common::Cbw::Cbw20,
                secondary80: 0,
            },
            snr_db: rng.gen_range(-20, 50),
            compatible: rng.gen::<bool>(),
            protection: match rng.gen_range(0, 5) {
                0 => fidl_sme::Protection::Open,
                1 => fidl_sme::Protection::Wep,
                2 => fidl_sme::Protection::Wpa1,
                3 => fidl_sme::Protection::Wpa1Wpa2Personal,
                4 => fidl_sme::Protection::Wpa2Personal,
                5 => fidl_sme::Protection::Wpa2Enterprise,
                6 => fidl_sme::Protection::Wpa3Enterprise,
                _ => panic!(),
            },
            bss_desc: None,
        }
    }

    #[test]
    fn directed_active_scan_filters_desired_network() {
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let (client, mut sme_stream) = exec.run_singlethreaded(create_iface_manager());

        // Issue request to scan.
        let desired_network = types::NetworkIdentifier {
            ssid: "test_ssid".as_bytes().to_vec(),
            type_: types::SecurityType::Wpa2,
        };
        let desired_channels = vec![1, 36];
        let scan_fut =
            perform_directed_active_scan(client, &desired_network, Some(desired_channels.clone()));
        pin_mut!(scan_fut);

        // Generate scan results
        let scan_result_aps = vec![
            fidl_sme::BssInfo {
                ssid: desired_network.ssid.clone(),
                protection: fidl_sme::Protection::Wpa3Enterprise, // wrong security type
                ..generate_random_bss_info()
            },
            fidl_sme::BssInfo {
                ssid: desired_network.ssid.clone(),
                protection: fidl_sme::Protection::Wpa2Personal,
                ..generate_random_bss_info()
            },
            fidl_sme::BssInfo {
                ssid: desired_network.ssid.clone(),
                protection: fidl_sme::Protection::Wpa2Personal,
                ..generate_random_bss_info()
            },
            fidl_sme::BssInfo {
                ssid: "other ssid".as_bytes().to_vec(),
                protection: fidl_sme::Protection::Wpa2Personal,
                ..generate_random_bss_info()
            },
        ];

        // Progress scan handler forward so that it will respond to the iterator get next request.
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Pending);

        // Respond to the scan request
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![desired_network.ssid.clone()],
            channels: desired_channels,
        });
        validate_sme_request_and_send_results(
            &mut exec,
            &mut sme_stream,
            &expected_scan_request,
            &scan_result_aps,
        );

        // Check for results
        assert_variant!(exec.run_until_stalled(&mut scan_fut), Poll::Ready(result) => {
            let result = result.unwrap();
            // Only the desired network is present in results
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].id, desired_network);
            // Two BSSs for this network
            assert_eq!(result[0].entries.len(), 2);
        });
    }

    // TODO(fxbug.dev/52700) Ignore this test until the location sensor module exists.
    #[ignore]
    #[test]
    fn scan_observer_sends_to_location_sensor() {
        set_logger_for_test();
        let mut exec = fasync::Executor::new().expect("failed to create an executor");
        let mut location_sensor_updater = LocationSensorUpdater {};
        let MockScanData {
            passive_input_aps: _,
            passive_internal_aps: internal_aps,
            passive_fidl_aps: _,
            active_input_aps: _,
            combined_internal_aps: _,
            combined_fidl_aps: _,
        } = create_scan_ap_data();
        let fut = location_sensor_updater.update_scan_results(&internal_aps);
        exec.run_singlethreaded(fut);
        panic!("Need to reach into location sensor and check it got data")
    }
}
