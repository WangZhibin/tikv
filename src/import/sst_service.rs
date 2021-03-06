// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use std::convert::TryFrom;
use std::sync::{Arc, Mutex};

use engine::rocks::util::{compact_files_in_range, ingest_maybe_slowdown_writes};
use engine::rocks::DB;
use engine::{name_to_cf, CF_DEFAULT};
use futures::sync::mpsc;
use futures::{future, Future, Stream};
use futures_cpupool::{Builder, CpuPool};
use grpcio::{ClientStreamingSink, RequestStream, RpcContext, UnarySink};
use kvproto::import_sstpb::*;
use kvproto::raft_cmdpb::*;

use crate::raftstore::router::RaftStoreRouter;
use crate::raftstore::store::Callback;
use crate::server::CONFIG_ROCKSDB_GAUGE;
use engine_rocks::{RocksEngine, RocksIOLimiter};
use engine_traits::{IOLimiter, SstExt, SstWriterBuilder};
use sst_importer::send_rpc_response;
use tikv_util::future::paired_future_callback;
use tikv_util::time::Instant;

use sst_importer::import_mode::*;
use sst_importer::metrics::*;
use sst_importer::service::*;
use sst_importer::{Config, Error, SSTImporter};

/// ImportSSTService provides tikv-server with the ability to ingest SST files.
///
/// It saves the SST sent from client to a file and then sends a command to
/// raftstore to trigger the ingest process.
#[derive(Clone)]
pub struct ImportSSTService<Router> {
    cfg: Config,
    router: Router,
    engine: Arc<DB>,
    threads: CpuPool,
    importer: Arc<SSTImporter>,
    switcher: Arc<Mutex<ImportModeSwitcher>>,
    limiter: Option<Arc<RocksIOLimiter>>,
}

impl<Router: RaftStoreRouter> ImportSSTService<Router> {
    pub fn new(
        cfg: Config,
        router: Router,
        engine: Arc<DB>,
        importer: Arc<SSTImporter>,
    ) -> ImportSSTService<Router> {
        let threads = Builder::new()
            .name_prefix("sst-importer")
            .pool_size(cfg.num_threads)
            .create();
        ImportSSTService {
            cfg,
            router,
            engine,
            threads,
            importer,
            switcher: Arc::new(Mutex::new(ImportModeSwitcher::new())),
            limiter: None,
        }
    }
}

impl<Router: RaftStoreRouter> ImportSst for ImportSSTService<Router> {
    fn switch_mode(
        &mut self,
        ctx: RpcContext<'_>,
        req: SwitchModeRequest,
        sink: UnarySink<SwitchModeResponse>,
    ) {
        let label = "switch_mode";
        let timer = Instant::now_coarse();

        let res = {
            let mut switcher = self.switcher.lock().unwrap();
            fn mf(cf: &str, name: &str, v: f64) {
                CONFIG_ROCKSDB_GAUGE.with_label_values(&[cf, name]).set(v);
            }

            match req.get_mode() {
                SwitchMode::Normal => {
                    switcher.enter_normal_mode(RocksEngine::from_ref(&self.engine), mf)
                }
                SwitchMode::Import => {
                    switcher.enter_import_mode(RocksEngine::from_ref(&self.engine), mf)
                }
            }
        };
        match res {
            Ok(_) => info!("switch mode"; "mode" => ?req.get_mode()),
            Err(ref e) => error!("switch mode failed"; "mode" => ?req.get_mode(), "err" => %e),
        }

        ctx.spawn(
            future::result(res)
                .map(|_| SwitchModeResponse::default())
                .then(move |res| send_rpc_response!(res, sink, label, timer)),
        )
    }

    /// Receive SST from client and save the file for later ingesting.
    fn upload(
        &mut self,
        ctx: RpcContext<'_>,
        stream: RequestStream<UploadRequest>,
        sink: ClientStreamingSink<UploadResponse>,
    ) {
        let label = "upload";
        let timer = Instant::now_coarse();
        let import = Arc::clone(&self.importer);
        let bounded_stream = mpsc::spawn(stream, &self.threads, self.cfg.stream_channel_window);

        ctx.spawn(
            self.threads.spawn(
                bounded_stream
                    .into_future()
                    .map_err(|(e, _)| Error::from(e))
                    .and_then(move |(chunk, stream)| {
                        // The first message of the stream contains metadata
                        // of the file.
                        let meta = match chunk {
                            Some(ref chunk) if chunk.has_meta() => chunk.get_meta(),
                            _ => return Err(Error::InvalidChunk),
                        };
                        let file = import.create(meta)?;
                        Ok((file, stream))
                    })
                    .and_then(move |(file, stream)| {
                        stream
                            .map_err(Error::from)
                            .fold(file, |mut file, chunk| {
                                let start = Instant::now_coarse();
                                let data = chunk.get_data();
                                if data.is_empty() {
                                    return future::err(Error::InvalidChunk);
                                }
                                if let Err(e) = file.append(data) {
                                    return future::err(e);
                                }
                                IMPORT_UPLOAD_CHUNK_BYTES.observe(data.len() as f64);
                                IMPORT_UPLOAD_CHUNK_DURATION.observe(start.elapsed_secs());
                                future::ok(file)
                            })
                            .and_then(|mut file| file.finish())
                    })
                    .map(|_| UploadResponse::default())
                    .then(move |res| send_rpc_response!(res, sink, label, timer)),
            ),
        )
    }

    /// Downloads the file and performs key-rewrite for later ingesting.
    fn download(
        &mut self,
        ctx: RpcContext<'_>,
        req: DownloadRequest,
        sink: UnarySink<DownloadResponse>,
    ) {
        let label = "download";
        let timer = Instant::now_coarse();
        let importer = Arc::clone(&self.importer);
        let limiter = self.limiter.clone();
        let engine = Arc::clone(&self.engine);
        let sst_writer = <RocksEngine as SstExt>::SstWriterBuilder::new()
            .set_db(RocksEngine::from_ref(&engine))
            .set_cf(name_to_cf(req.get_sst().get_cf_name()).unwrap())
            .build(self.importer.get_path(req.get_sst()).to_str().unwrap())
            .unwrap();

        ctx.spawn(self.threads.spawn_fn(move || {
            let res = importer.download::<RocksEngine>(
                req.get_sst(),
                req.get_storage_backend(),
                req.get_name(),
                req.get_rewrite_rule(),
                limiter,
                sst_writer,
            );

            future::result(res)
                .map_err(Error::from)
                .map(|range| {
                    let mut resp = DownloadResponse::default();
                    if let Some(r) = range {
                        resp.set_range(r);
                    } else {
                        resp.set_is_empty(true);
                    }
                    resp
                })
                .then(move |res| send_rpc_response!(res, sink, label, timer))
        }));
    }

    /// Ingest the file by sending a raft command to raftstore.
    ///
    /// If the ingestion fails because the region is not found or the epoch does
    /// not match, the remaining files will eventually be cleaned up by
    /// CleanupSSTWorker.
    fn ingest(
        &mut self,
        ctx: RpcContext<'_>,
        mut req: IngestRequest,
        sink: UnarySink<IngestResponse>,
    ) {
        let label = "ingest";
        let timer = Instant::now_coarse();

        if self.switcher.lock().unwrap().get_mode() == SwitchMode::Normal
            && ingest_maybe_slowdown_writes(&self.engine, CF_DEFAULT)
        {
            return send_rpc_error(
                ctx,
                sink,
                Error::Engine(box_err!("too many sst files are ingesting.")),
            );
        }
        // Make ingest command.
        let mut ingest = Request::default();
        ingest.set_cmd_type(CmdType::IngestSst);
        ingest.mut_ingest_sst().set_sst(req.take_sst());
        let mut context = req.take_context();
        let mut header = RaftRequestHeader::default();
        header.set_peer(context.take_peer());
        header.set_region_id(context.get_region_id());
        header.set_region_epoch(context.take_region_epoch());
        let mut cmd = RaftCmdRequest::default();
        cmd.set_header(header);
        cmd.mut_requests().push(ingest);

        let (cb, future) = paired_future_callback();
        if let Err(e) = self.router.send_command(cmd, Callback::Write(cb)) {
            return send_rpc_error(ctx, sink, e);
        }

        ctx.spawn(
            future
                .map_err(Error::from)
                .then(|res| match res {
                    Ok(mut res) => {
                        let mut resp = IngestResponse::default();
                        let mut header = res.response.take_header();
                        if header.has_error() {
                            resp.set_error(header.take_error());
                        }
                        future::ok(resp)
                    }
                    Err(e) => future::err(e),
                })
                .then(move |res| send_rpc_response!(res, sink, label, timer)),
        )
    }

    fn compact(
        &mut self,
        ctx: RpcContext<'_>,
        req: CompactRequest,
        sink: UnarySink<CompactResponse>,
    ) {
        let label = "compact";
        let timer = Instant::now_coarse();
        let engine = Arc::clone(&self.engine);

        ctx.spawn(self.threads.spawn_fn(move || {
            let (start, end) = if !req.has_range() {
                (None, None)
            } else {
                (
                    Some(req.get_range().get_start()),
                    Some(req.get_range().get_end()),
                )
            };
            let output_level = if req.get_output_level() == -1 {
                None
            } else {
                Some(req.get_output_level())
            };

            let res = compact_files_in_range(&engine, start, end, output_level);
            match res {
                Ok(_) => info!(
                    "compact files in range";
                    "start" => start.map(log_wrappers::Key),
                    "end" => end.map(log_wrappers::Key),
                    "output_level" => ?output_level, "takes" => ?timer.elapsed()
                ),
                Err(ref e) => error!(
                    "compact files in range failed";
                    "start" => start.map(log_wrappers::Key),
                    "end" => end.map(log_wrappers::Key),
                    "output_level" => ?output_level, "err" => %e
                ),
            }

            future::result(res)
                .map_err(|e| Error::Engine(box_err!(e)))
                .map(|_| CompactResponse::default())
                .then(move |res| send_rpc_response!(res, sink, label, timer))
        }))
    }

    fn set_download_speed_limit(
        &mut self,
        ctx: RpcContext<'_>,
        req: SetDownloadSpeedLimitRequest,
        sink: UnarySink<SetDownloadSpeedLimitResponse>,
    ) {
        let label = "set_download_speed_limit";
        let timer = Instant::now_coarse();

        let s = i64::try_from(req.get_speed_limit());
        let s = if let Ok(s) = s {
            s
        } else {
            warn!(
                "SetDownloadSpeedLimitRequest out of range: {}. Using i64::max_value",
                req.get_speed_limit()
            );
            i64::max_value()
        };

        match (s, &mut self.limiter) {
            (0, limiter) => *limiter = None,
            (s, Some(l)) => l.set_bytes_per_second(s),
            (s, limiter) => *limiter = Some(Arc::new(RocksIOLimiter::new(s))),
        }

        ctx.spawn(
            future::ok::<_, ()>(SetDownloadSpeedLimitResponse::default())
                .then(move |res| send_rpc_response!(res, sink, label, timer)),
        )
    }
}
