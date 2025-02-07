// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        accessor::ArchiveAccessor,
        archive, configs, constants, diagnostics,
        events::{stream::EventStream, types::EventSource},
        logs::redact::Redactor,
        pipeline::Pipeline,
        repository::DataRepo,
    },
    anyhow::Error,
    fidl::{endpoints::RequestStream, AsyncChannel},
    fidl_fuchsia_diagnostics::Selector,
    fidl_fuchsia_diagnostics_test::{ControllerRequest, ControllerRequestStream},
    fidl_fuchsia_process_lifecycle::{LifecycleRequest, LifecycleRequestStream},
    fidl_fuchsia_sys_internal::SourceIdentity,
    fuchsia_async::{self as fasync, Task},
    fuchsia_component::server::{ServiceFs, ServiceObj, ServiceObjTrait},
    fuchsia_inspect::{component, health::Reporter},
    fuchsia_runtime::{take_startup_handle, HandleInfo, HandleType},
    fuchsia_zircon as zx,
    futures::{
        channel::mpsc,
        future::{self, abortable},
        prelude::*,
    },
    io_util,
    parking_lot::RwLock,
    std::{
        path::{Path, PathBuf},
        sync::Arc,
    },
    tracing::{debug, error, info, warn},
};

/// Spawns controller sends stop signal.
fn spawn_controller(mut stream: ControllerRequestStream, mut stop_sender: mpsc::Sender<()>) {
    fasync::Task::spawn(
        async move {
            while let Some(ControllerRequest::Stop { .. }) = stream.try_next().await? {
                debug!("Stop request received.");
                stop_sender.send(()).await.ok();
                break;
            }
            Ok(())
        }
        .map(|o: Result<(), fidl::Error>| {
            if let Err(e) = o {
                error!(%e, "error serving controller");
            }
        }),
    )
    .detach();
}

fn maybe_create_archive<ServiceObjTy: ServiceObjTrait>(
    fs: &mut ServiceFs<ServiceObjTy>,
    archive_path: &PathBuf,
) -> Result<archive::ArchiveWriter, Error> {
    let writer = archive::ArchiveWriter::open(archive_path.clone())?;
    fs.add_remote(
        "archive",
        io_util::open_directory_in_namespace(
            &archive_path.to_string_lossy(),
            io_util::OPEN_RIGHT_READABLE | io_util::OPEN_RIGHT_WRITABLE,
        )?,
    );
    Ok(writer)
}

/// The `Archivist` is responsible for publishing all the services and monitoring component's health.
/// # All resposibilities:
///  * Run and process Log Sink connections on main future.
///  * Run and Process Log Listener connections by spawning them.
///  * Optionally collect component events.
pub struct Archivist {
    /// Archive state, including the diagnostics repo which currently stores all logs.
    state: archive::ArchivistState,

    /// True if pipeline exists.
    pipeline_exists: bool,

    /// Store for safe keeping,
    _pipeline_nodes: Vec<fuchsia_inspect::Node>,

    // Store for safe keeping.
    _pipeline_configs: Vec<configs::PipelineConfig>,

    /// ServiceFs object to server outgoing directory.
    fs: ServiceFs<ServiceObj<'static, ()>>,

    /// Receiver for stream which will process LogSink connections.
    log_receiver: mpsc::UnboundedReceiver<Task<()>>,

    /// Sender which is used to close the stream of LogSink connections.
    ///
    /// Clones of the sender keep the receiver end of the channel open. As soon
    /// as all clones are dropped or disconnected, the receiver will close. The
    /// receiver must close for `Archivist::run` to return gracefully.
    log_sender: mpsc::UnboundedSender<Task<()>>,

    /// Receiver for stream which will process Log connections.
    listen_receiver: mpsc::UnboundedReceiver<Task<()>>,

    /// Sender which is used to close the stream of Log connections after log_sender
    /// completes.
    ///
    /// Clones of the sender keep the receiver end of the channel open. As soon
    /// as all clones are dropped or disconnected, the receiver will close. The
    /// receiver must close for `Archivist::run` to return gracefully.
    listen_sender: mpsc::UnboundedSender<Task<()>>,

    /// Listes for events coming from v1 and v2.
    event_stream: EventStream,

    /// Recieve stop signal to kill this archivist.
    stop_recv: Option<mpsc::Receiver<()>>,
}

impl Archivist {
    async fn collect_component_events(
        event_stream: EventStream,
        state: archive::ArchivistState,
        pipeline_exists: bool,
    ) {
        let events = event_stream.listen().await;
        if !pipeline_exists {
            component::health().set_unhealthy("Pipeline config has an error");
        } else {
            component::health().set_ok();
        }
        archive::run_archivist(state, events).await
    }

    /// Install controller service.
    pub fn install_controller_service(&mut self) -> &mut Self {
        let (stop_sender, stop_recv) = mpsc::channel(0);
        self.fs
            .dir("svc")
            .add_fidl_service(move |stream| spawn_controller(stream, stop_sender.clone()));
        self.stop_recv = Some(stop_recv);
        debug!("Controller services initialized.");
        self
    }

    fn take_lifecycle_channel() -> LifecycleRequestStream {
        let lifecycle_handle_info = HandleInfo::new(HandleType::Lifecycle, 0);
        let lifecycle_handle = take_startup_handle(lifecycle_handle_info)
            .expect("must have been provided a lifecycle channel in procargs");
        let x: zx::Channel = lifecycle_handle.into();
        let async_x = AsyncChannel::from(
            fasync::Channel::from_channel(x).expect("Async channel conversion failed."),
        );
        LifecycleRequestStream::from_channel(async_x)
    }

    pub fn install_lifecycle_listener(&mut self) -> &mut Self {
        let (mut stop_sender, stop_recv) = mpsc::channel(0);
        let mut req_stream = Self::take_lifecycle_channel();

        Task::spawn(async move {
            debug!("Awaiting request to close");
            while let Some(LifecycleRequest::Stop { .. }) =
                req_stream.try_next().await.expect("Failure receiving lifecycle FIDL message")
            {
                info!("Initiating shutdown.");
                stop_sender.send(()).await.unwrap();
            }
        })
        .detach();

        self.stop_recv = Some(stop_recv);
        debug!("Lifecycle listener initialized.");
        self
    }

    /// Installs `LogSink` and `Log` services. Panics if called twice.
    /// # Arguments:
    /// * `log_connector` - If provided, install log connector.
    pub fn install_logger_services(&mut self) -> &mut Self {
        let data_repo_1 = self.data_repo().clone();
        let data_repo_2 = self.data_repo().clone();
        let data_repo_3 = self.data_repo().clone();
        let log_sender = self.log_sender.clone();
        let log_sender2 = self.log_sender.clone();
        let listen_sender = self.listen_sender.clone();

        self.fs
            .dir("svc")
            .add_fidl_service(move |stream| {
                debug!("fuchsia.logger.Log connection");
                data_repo_1.clone().handle_log(stream, listen_sender.clone());
            })
            .add_fidl_service(move |stream| {
                debug!("fuchsia.logger.LogSink connection");
                let source = Arc::new(SourceIdentity::EMPTY);
                fasync::Task::spawn(data_repo_2.clone().handle_log_sink(
                    stream,
                    source,
                    log_sender.clone(),
                ))
                .detach();
            })
            .add_fidl_service(move |stream| {
                debug!("fuchsia.sys.EventStream connection");
                fasync::Task::spawn(
                    data_repo_3.clone().handle_event_stream(stream, log_sender2.clone()),
                )
                .detach()
            });
        debug!("Log services initialized.");
        self
    }

    // Sets event provider which is used to collect component events, Panics if called twice.
    pub fn add_event_source(
        &mut self,
        name: impl Into<String>,
        source: Box<dyn EventSource>,
    ) -> &mut Self {
        let name = name.into();
        debug!("{} event source initialized", &name);
        self.event_stream.add_source(name, source);
        self
    }

    /// Creates new instance, sets up inspect and adds 'archive' directory to output folder.
    /// Also installs `fuchsia.diagnostics.Archive` service.
    /// Call `install_logger_services`, `add_event_source`.
    pub fn new(archivist_configuration: configs::Config) -> Result<Self, Error> {
        let (log_sender, log_receiver) = mpsc::unbounded();
        let (listen_sender, listen_receiver) = mpsc::unbounded();

        let mut fs = ServiceFs::new();
        diagnostics::serve(&mut fs)?;

        let writer = archivist_configuration.archive_path.as_ref().and_then(|archive_path| {
            maybe_create_archive(&mut fs, archive_path)
                .or_else(|e| {
                    // TODO(fxbug.dev/57271): this is not expected in regular builds of the archivist. It's
                    // happening when starting the zircon_guest (fx shell guest launch zircon_guest)
                    // We'd normally fail if we couldn't create the archive, but for now we include
                    // a warning.
                    warn!(
                        path = %archive_path.display(), ?e,
                        "Failed to create archive"
                    );
                    Err(e)
                })
                .ok()
        });

        let pipelines_node = diagnostics::root().create_child("pipelines");
        let feedback_pipeline_node = pipelines_node.create_child("feedback");
        let legacy_pipeline_node = pipelines_node.create_child("legacy_metrics");
        let mut feedback_config = configs::PipelineConfig::from_directory(
            "/config/data/feedback",
            configs::EmptyBehavior::DoNotFilter,
        );
        feedback_config.record_to_inspect(&feedback_pipeline_node);
        let mut legacy_config = configs::PipelineConfig::from_directory(
            "/config/data/legacy_metrics",
            configs::EmptyBehavior::Disable,
        );
        legacy_config.record_to_inspect(&legacy_pipeline_node);
        // Do not set the state to error if the pipelines simply do not exist.
        let pipeline_exists = !((Path::new("/config/data/feedback").is_dir()
            && feedback_config.has_error())
            || (Path::new("/config/data/legacy_metrics").is_dir() && legacy_config.has_error()));

        let diagnostics_repo = DataRepo::with_logs_inspect(diagnostics::root(), "log_stats");

        // The Inspect Repository offered to the ALL_ACCESS pipeline. This
        // repository is unique in that it has no statically configured
        // selectors, meaning all diagnostics data is visible.
        // This should not be used for production services.
        // TODO(fxbug.dev/55735): Lock down this protocol using allowlists.
        let all_access_pipeline =
            Arc::new(RwLock::new(Pipeline::new(None, Redactor::noop(), diagnostics_repo.clone())));

        // The Inspect Repository offered to the Feedback pipeline. This repository applies
        // static selectors configured under config/data/feedback to inspect exfiltration.
        let (feedback_static_selectors, feedback_redactor) = if !feedback_config.disable_filtering {
            (
                feedback_config.take_inspect_selectors().map(|selectors| {
                    selectors
                        .into_iter()
                        .map(|selector| Arc::new(selector))
                        .collect::<Vec<Arc<Selector>>>()
                }),
                Redactor::with_static_patterns(),
            )
        } else {
            (None, Redactor::noop())
        };

        let feedback_pipeline = Arc::new(RwLock::new(Pipeline::new(
            feedback_static_selectors,
            feedback_redactor,
            diagnostics_repo.clone(),
        )));

        // The Inspect Repository offered to the LegacyMetrics
        // pipeline. This repository applies static selectors configured
        // under config/data/legacy_metrics to inspect exfiltration.
        let legacy_metrics_pipeline = Arc::new(RwLock::new(Pipeline::new(
            match legacy_config.disable_filtering {
                false => legacy_config.take_inspect_selectors().map(|selectors| {
                    selectors
                        .into_iter()
                        .map(|selector| Arc::new(selector))
                        .collect::<Vec<Arc<Selector>>>()
                }),
                true => None,
            },
            Redactor::noop(),
            diagnostics_repo.clone(),
        )));

        // TODO(fxbug.dev/55736): Refactor this code so that we don't store
        // diagnostics data N times if we have N pipelines. We should be
        // storing a single copy regardless of the number of pipelines.
        let archivist_state = archive::ArchivistState::new(
            archivist_configuration,
            vec![
                all_access_pipeline.clone(),
                feedback_pipeline.clone(),
                legacy_metrics_pipeline.clone(),
            ],
            diagnostics_repo,
            writer,
        )?;

        let all_accessor_stats = Arc::new(diagnostics::AccessorStats::new(
            component::inspector().root().create_child("all_archive_accessor"),
        ));

        let feedback_accessor_stats = Arc::new(diagnostics::AccessorStats::new(
            component::inspector().root().create_child("feedback_archive_accessor"),
        ));

        let legacy_accessor_stats = Arc::new(diagnostics::AccessorStats::new(
            component::inspector().root().create_child("legacy_metrics_archive_accessor"),
        ));

        fs.dir("svc")
            .add_fidl_service(move |stream| {
                debug!("fuchsia.diagnostics.ArchiveAccessor connection");
                let all_archive_accessor =
                    ArchiveAccessor::new(all_access_pipeline.clone(), all_accessor_stats.clone());
                all_archive_accessor.spawn_archive_accessor_server(stream)
            })
            .add_fidl_service_at(constants::FEEDBACK_ARCHIVE_ACCESSOR_NAME, move |chan| {
                debug!("fuchsia.diagnostics.FeedbackArchiveAccessor connection");
                let feedback_archive_accessor = ArchiveAccessor::new(
                    feedback_pipeline.clone(),
                    feedback_accessor_stats.clone(),
                );
                feedback_archive_accessor.spawn_archive_accessor_server(chan)
            })
            .add_fidl_service_at(constants::LEGACY_METRICS_ARCHIVE_ACCESSOR_NAME, move |chan| {
                debug!("fuchsia.diagnostics.LegacyMetricsAccessor connection");
                let legacy_archive_accessor = ArchiveAccessor::new(
                    legacy_metrics_pipeline.clone(),
                    legacy_accessor_stats.clone(),
                );
                legacy_archive_accessor.spawn_archive_accessor_server(chan)
            });

        let events_node = diagnostics::root().create_child("event_stats");
        Ok(Self {
            fs,
            state: archivist_state,
            log_receiver,
            log_sender,
            listen_receiver,
            listen_sender,
            pipeline_exists,
            _pipeline_nodes: vec![pipelines_node, feedback_pipeline_node, legacy_pipeline_node],
            _pipeline_configs: vec![feedback_config, legacy_config],
            event_stream: EventStream::new(events_node),
            stop_recv: None,
        })
    }

    pub fn data_repo(&self) -> &DataRepo {
        &self.state.diagnostics_repo
    }

    pub fn log_sender(&self) -> &mpsc::UnboundedSender<Task<()>> {
        &self.log_sender
    }

    /// Run archivist to completion.
    /// # Arguments:
    /// * `outgoing_channel`- channel to serve outgoing directory on.
    pub async fn run(mut self, outgoing_channel: zx::Channel) -> Result<(), Error> {
        debug!("Running Archivist.");

        let data_repo = { self.data_repo().clone() };
        self.fs.serve_connection(outgoing_channel)?;
        // Start servcing all outgoing services.
        let run_outgoing = self.fs.collect::<()>();
        // collect events.
        let run_event_collection =
            Self::collect_component_events(self.event_stream, self.state, self.pipeline_exists);

        // Process messages from log sink.
        let log_receiver = self.log_receiver;
        let listen_receiver = self.listen_receiver;
        let all_msg = async {
            log_receiver.for_each_concurrent(None, |rx| async move { rx.await }).await;
            debug!("Log ingestion stopped.");
            data_repo.terminate_logs();
            debug!("Flushing to listeners.");
            listen_receiver.for_each_concurrent(None, |rx| async move { rx.await }).await;
            debug!("Log listeners stopped.");
        };

        let (abortable_fut, abort_handle) =
            abortable(future::join(run_outgoing, run_event_collection));

        let mut listen_sender = self.listen_sender;
        let mut log_sender = self.log_sender;
        let stop_fut = match self.stop_recv {
            Some(stop_recv) => async move {
                stop_recv.into_future().await;
                listen_sender.disconnect();
                log_sender.disconnect();
                abort_handle.abort()
            }
            .left_future(),
            None => future::ready(()).right_future(),
        };

        debug!("Entering core loop.");
        // Combine all three futures into a main future.
        future::join3(abortable_fut, stop_fut, all_msg).map(|_| Ok(())).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testing::*;
    use fidl::endpoints::create_proxy;
    use fidl_fuchsia_diagnostics_test::ControllerMarker;
    use fidl_fuchsia_io as fio;
    use fio::DirectoryProxy;
    use fuchsia_async as fasync;
    use fuchsia_component::client::connect_to_protocol_at_dir_svc;
    use futures::channel::oneshot;

    fn init_archivist() -> Archivist {
        let config = configs::Config {
            archive_path: None,
            max_archive_size_bytes: 10,
            max_event_group_size_bytes: 10,
            num_threads: 1,
        };

        Archivist::new(config).unwrap()
    }

    // run archivist and send signal when it dies.
    fn run_archivist_and_signal_on_exit() -> (DirectoryProxy, oneshot::Receiver<()>) {
        let (directory, server_end) = create_proxy::<fio::DirectoryMarker>().unwrap();
        let mut archivist = init_archivist();
        archivist.install_logger_services().install_controller_service();
        let (signal_send, signal_recv) = oneshot::channel();
        fasync::Task::spawn(async move {
            archivist.run(server_end.into_channel()).await.expect("Cannot run archivist");
            signal_send.send(()).unwrap();
        })
        .detach();
        (directory, signal_recv)
    }

    // runs archivist and returns its directory.
    fn run_archivist() -> DirectoryProxy {
        let (directory, server_end) = create_proxy::<fio::DirectoryMarker>().unwrap();
        let mut archivist = init_archivist();
        archivist.install_logger_services();
        fasync::Task::spawn(async move {
            archivist.run(server_end.into_channel()).await.expect("Cannot run archivist");
        })
        .detach();
        directory
    }

    #[fasync::run_singlethreaded(test)]
    async fn can_log_and_retrive_log() {
        let directory = run_archivist();
        let mut recv_logs = start_listener(&directory);

        let mut log_helper = LogSinkHelper::new(&directory);
        log_helper.write_log("my msg1");
        log_helper.write_log("my msg2");

        assert_eq!(
            vec! {Some("my msg1".to_owned()),Some("my msg2".to_owned())},
            vec! {recv_logs.next().await,recv_logs.next().await}
        );

        // new client can log
        let mut log_helper2 = LogSinkHelper::new(&directory);
        log_helper2.write_log("my msg1");
        log_helper.write_log("my msg2");

        let mut expected = vec!["my msg1".to_owned(), "my msg2".to_owned()];
        expected.sort();

        let mut actual = vec![recv_logs.next().await.unwrap(), recv_logs.next().await.unwrap()];
        actual.sort();

        assert_eq!(expected, actual);

        // can log after killing log sink proxy
        log_helper.kill_log_sink();
        log_helper.write_log("my msg1");
        log_helper.write_log("my msg2");

        assert_eq!(
            expected,
            vec! {recv_logs.next().await.unwrap(),recv_logs.next().await.unwrap()}
        );

        // can log from new socket cnonnection
        log_helper2.add_new_connection();
        log_helper2.write_log("my msg1");
        log_helper2.write_log("my msg2");

        assert_eq!(
            expected,
            vec! {recv_logs.next().await.unwrap(),recv_logs.next().await.unwrap()}
        );
    }

    /// Makes sure that implementaion can handle multiple sockets from same
    /// log sink.
    #[fasync::run_singlethreaded(test)]
    async fn log_from_multiple_sock() {
        let directory = run_archivist();
        let mut recv_logs = start_listener(&directory);

        let log_helper = LogSinkHelper::new(&directory);
        let sock1 = log_helper.connect();
        let sock2 = log_helper.connect();
        let sock3 = log_helper.connect();

        LogSinkHelper::write_log_at(&sock1, "msg sock1-1");
        LogSinkHelper::write_log_at(&sock2, "msg sock2-1");
        LogSinkHelper::write_log_at(&sock1, "msg sock1-2");
        LogSinkHelper::write_log_at(&sock3, "msg sock3-1");
        LogSinkHelper::write_log_at(&sock2, "msg sock2-2");

        let mut expected = vec![
            "msg sock1-1".to_owned(),
            "msg sock1-2".to_owned(),
            "msg sock2-1".to_owned(),
            "msg sock2-2".to_owned(),
            "msg sock3-1".to_owned(),
        ];
        expected.sort();

        let mut actual = vec![
            recv_logs.next().await.unwrap(),
            recv_logs.next().await.unwrap(),
            recv_logs.next().await.unwrap(),
            recv_logs.next().await.unwrap(),
            recv_logs.next().await.unwrap(),
        ];
        actual.sort();

        assert_eq!(expected, actual);
    }

    /// Stop API works
    #[fasync::run_singlethreaded(test)]
    async fn stop_works() {
        let (directory, signal_recv) = run_archivist_and_signal_on_exit();
        let mut recv_logs = start_listener(&directory);

        {
            // make sure we can write logs
            let log_sink_helper = LogSinkHelper::new(&directory);
            let sock1 = log_sink_helper.connect();
            LogSinkHelper::write_log_at(&sock1, "msg sock1-1");
            log_sink_helper.write_log("msg sock1-2");
            let mut expected = vec!["msg sock1-1".to_owned(), "msg sock1-2".to_owned()];
            expected.sort();
            let mut actual = vec![recv_logs.next().await.unwrap(), recv_logs.next().await.unwrap()];
            actual.sort();
            assert_eq!(expected, actual);

            //  Start new connections and sockets
            let log_sink_helper1 = LogSinkHelper::new(&directory);
            let sock2 = log_sink_helper.connect();
            // Write logs before calling stop
            log_sink_helper1.write_log("msg 1");
            log_sink_helper1.write_log("msg 2");
            let log_sink_helper2 = LogSinkHelper::new(&directory);

            let controller = connect_to_protocol_at_dir_svc::<ControllerMarker>(&directory)
                .expect("cannot connect to log proxy");
            controller.stop().unwrap();

            // make more socket connections and write to them and old ones.
            let sock3 = log_sink_helper2.connect();
            log_sink_helper2.write_log("msg 3");
            log_sink_helper2.write_log("msg 4");

            LogSinkHelper::write_log_at(&sock3, "msg 5");
            LogSinkHelper::write_log_at(&sock2, "msg 6");
            log_sink_helper.write_log("msg 7");
            LogSinkHelper::write_log_at(&sock1, "msg 8");

            LogSinkHelper::write_log_at(&sock2, "msg 9");
        } // kills all sockets and log_sink connections
        let mut expected = vec![];
        let mut actual = vec![];
        for i in 1..=9 {
            expected.push(format!("msg {}", i));
            actual.push(recv_logs.next().await.unwrap());
        }
        expected.sort();
        actual.sort();

        // make sure archivist is dead.
        signal_recv.await.unwrap();

        assert_eq!(expected, actual);
    }
}
