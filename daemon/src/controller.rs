use channel::tcp::TcpSender;
use channel::poll::{PollEvent, PollingLoop};
use distributary::{Blender, CoordinationMessage, CoordinationPayload};
use slog::Logger;
use std::io;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api;

pub struct WorkerStatus {
    healthy: bool,
    last_heartbeat: Instant,
    sender: Option<Arc<Mutex<TcpSender<CoordinationMessage>>>>,
}

impl WorkerStatus {
    pub fn new(sender: Arc<Mutex<TcpSender<CoordinationMessage>>>) -> Self {
        WorkerStatus {
            healthy: true,
            last_heartbeat: Instant::now(),
            sender: Some(sender),
        }
    }
}

pub struct Controller {
    listen_addr: String,
    listen_port: u16,

    log: Logger,

    blender: Arc<Mutex<Blender>>,
    workers: HashMap<SocketAddr, WorkerStatus>,

    heartbeat_every: Duration,
    healthcheck_every: Duration,
    last_checked_workers: Instant,
}

impl Controller {
    pub fn new(
        listen_addr: &str,
        port: u16,
        heartbeat_every: Duration,
        healthcheck_every: Duration,
        log: Logger,
    ) -> Controller {
        let mut blender = Blender::new();
        blender.log_with(log.clone());

        Controller {
            listen_addr: String::from(listen_addr),
            listen_port: port,
            log: log,
            blender: Arc::new(Mutex::new(blender)),
            workers: HashMap::new(),
            heartbeat_every: heartbeat_every,
            healthcheck_every: healthcheck_every,
            last_checked_workers: Instant::now(),
        }
    }

    /// Listen for workers to connect
    pub fn listen(&mut self) {
        use channel::poll::ProcessResult;
        use mio::net::TcpListener;
        use std::str::FromStr;
        use std::thread;

        let listener = TcpListener::bind(&SocketAddr::from_str(
            &format!("{}:{}", self.listen_addr, self.listen_port),
        ).unwrap()).unwrap();

        // run the API server (to receive recipes)
        let tb = thread::Builder::new().name("api-srv".into());
        let blender_arc = self.blender.clone();
        let api_logger = self.log.clone();
        let api_jh = match tb.spawn(|| api::run(blender_arc, api_logger).unwrap()) {
            Ok(jh) => jh,
            Err(e) => panic!("failed to spawn API server: {:?}", e),
        };

        let mut pl: PollingLoop<CoordinationMessage> = PollingLoop::from_listener(listener);
        pl.run_polling_loop(|e| {
            match e {
                PollEvent::Process(ref msg) => {
                    debug!(self.log, "Received {:?}", msg);
                    match self.handle(msg) {
                        Ok(_) => (),
                        Err(e) => error!(self.log, "failed to handle message {:?}: {:?}", msg, e),
                    }
                }
                PollEvent::ResumePolling(timeout) => *timeout = Some(self.healthcheck_every),
                PollEvent::Timeout => (),
            }

            self.check_worker_liveness();

            ProcessResult::KeepPolling
        });

        api_jh.join().unwrap();
    }

    fn check_worker_liveness(&mut self) {
        if self.last_checked_workers.elapsed() > self.healthcheck_every {
            for (addr, ws) in self.workers.iter_mut() {
                if ws.healthy && ws.last_heartbeat.elapsed() > self.heartbeat_every * 3 {
                    warn!(self.log, "worker at {:?} has failed!", addr);
                    ws.healthy = false;
                }
            }
            self.last_checked_workers = Instant::now();
        }
    }

    fn handle(&mut self, msg: &CoordinationMessage) -> Result<(), io::Error> {
        match msg.payload {
            CoordinationPayload::Register(ref remote) => self.handle_register(msg, remote),
            CoordinationPayload::Heartbeat => self.handle_heartbeat(msg),
            _ => unimplemented!(),
        }
    }

    fn handle_register(
        &mut self,
        msg: &CoordinationMessage,
        remote: &SocketAddr,
    ) -> Result<(), io::Error> {
        info!(
            self.log,
            "new worker registered from {:?}, which listens on {:?}",
            msg.source,
            remote
        );

        let sender = Arc::new(Mutex::new(TcpSender::connect(remote, None)?));
        let ws = WorkerStatus::new(sender.clone());
        self.workers.insert(msg.source.clone(), ws);

        let mut b = self.blender.lock().unwrap();
        b.add_worker(msg.source, sender);

        Ok(())
    }

    fn handle_heartbeat(&mut self, msg: &CoordinationMessage) -> Result<(), io::Error> {
        match self.workers.get_mut(&msg.source) {
            None => {
                crit!(
                    self.log,
                    "got heartbeat for unknown worker {:?}",
                    msg.source
                )
            }
            Some(ref mut ws) => {
                ws.last_heartbeat = Instant::now();
            }
        }

        Ok(())
    }
}