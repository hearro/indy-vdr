extern crate env_logger;
extern crate indy_vdr;
extern crate log;
#[macro_use]
extern crate serde_json;

mod app;
mod handlers;

use std::cell::RefCell;
use std::fs;
use std::net::IpAddr;
use std::process::exit;
use std::rc::Rc;
use std::time::SystemTime;

#[cfg(feature = "fetch")]
use hyper::body::Buf;
use hyper::service::{make_service_fn, service_fn};
#[cfg(feature = "fetch")]
use hyper::Client;
use hyper::Server;
#[cfg(feature = "fetch")]
use hyper_tls::HttpsConnector;
use hyper_unix_connector::UnixConnector;

use indy_vdr::common::error::prelude::*;
use indy_vdr::pool::{helpers::perform_refresh, LocalPool, PoolBuilder, PoolTransactions};

fn main() {
    let config = app::load_config().unwrap_or_else(|err| {
        eprintln!("{}", err);
        exit(1);
    });

    env_logger::init();

    let mut rt = tokio::runtime::Builder::new()
        .enable_all()
        .basic_scheduler()
        .build()
        .expect("build runtime");

    let local = tokio::task::LocalSet::new();
    if let Err(err) = local.block_on(&mut rt, init_server(config)) {
        eprintln!("{}", err);
        exit(1);
    }
}

pub struct AppState {
    pool: Option<LocalPool>,
    last_refresh: Option<SystemTime>,
    transactions: PoolTransactions,
}

#[cfg(feature = "fetch")]
async fn fetch_transactions(genesis: String) -> VdrResult<PoolTransactions> {
    let https = HttpsConnector::new();
    let client = Client::builder().build::<_, hyper::Body>(https);
    let mut res = client
        .get(genesis.parse().with_err_msg(
            VdrErrorKind::Config,
            format!("Error parsing genesis URL: {}", genesis),
        )?)
        .await
        .with_err_msg(VdrErrorKind::Config, "Error fetching genesis transactions")?;
    if res.status() != 200 {
        return Err(err_msg(
            VdrErrorKind::Config,
            format!(
                "Unexpected HTTP status for genesis transactions: {}",
                res.status()
            ),
        ));
    };
    let body = hyper::body::aggregate(res.body_mut())
        .await
        .with_err_msg(VdrErrorKind::Config, "Error receiving genesis transactions")?
        .to_bytes();
    let txns = String::from_utf8_lossy(&body);
    PoolTransactions::from_json(&txns)
}

#[cfg(not(feature = "fetch"))]
async fn fetch_transactions(_genesis: String) -> VdrResult<PoolTransactions> {
    Err(err_msg(
        VdrErrorKind::Config,
        "This application is not compiled with HTTP(S) request support",
    ))
}

async fn init_app_state(genesis: String) -> VdrResult<AppState> {
    let transactions = if genesis.starts_with("http:") || genesis.starts_with("https:") {
        fetch_transactions(genesis).await?
    } else {
        PoolTransactions::from_file(genesis.as_str())?
    };
    let state = AppState {
        pool: None,
        last_refresh: None,
        transactions,
    };
    Ok(state)
}

async fn init_pool(state: Rc<RefCell<AppState>>, refresh: bool) {
    match create_pool(state.clone(), refresh).await {
        Ok(pool) => {
            state.borrow_mut().pool.replace(pool);
        }
        Err(err) => {
            eprintln!("Error initializing pool: {}", err);
            // FIXME send shutdown signal
        }
    }
    // FIXME wait for refresh timeout, then perform refresh
    // use return from this async fn to signal shutdown
}

async fn create_pool(state: Rc<RefCell<AppState>>, refresh: bool) -> VdrResult<LocalPool> {
    let builder = PoolBuilder::default().transactions(state.borrow().transactions.clone())?;
    let pool = builder.into_local()?;
    let refresh_pool = if refresh {
        refresh_pool(state, &pool).await?
    } else {
        None
    };
    Ok(refresh_pool.unwrap_or(pool))
}

async fn refresh_pool(
    state: Rc<RefCell<AppState>>,
    pool: &LocalPool,
) -> VdrResult<Option<LocalPool>> {
    let (txns, _timing) = perform_refresh(pool).await?;
    state.borrow_mut().last_refresh.replace(SystemTime::now());
    if let Some(txns) = txns {
        let builder = {
            let pool_txns = &mut state.borrow_mut().transactions;
            pool_txns.extend_from_json(&txns)?;
            PoolBuilder::default().transactions(pool_txns.clone())?
        };
        Ok(Some(builder.into_local()?))
    } else {
        Ok(None)
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}

async fn init_server(config: app::Config) -> Result<(), String> {
    let state = Rc::new(RefCell::new(
        init_app_state(config.genesis)
            .await
            .map_err(|err| format!("Error loading config: {}", err))?,
    ));

    tokio::task::spawn_local(init_pool(state.clone(), config.init_refresh));

    if let Some(socket) = config.socket {
        fs::remove_file(&socket)
            .map_err(|err| format!("Error removing socket: {}", err.to_string()))?;
        let uc: UnixConnector = tokio::net::UnixListener::bind(&socket)
            .map_err(|err| format!("Error binding UNIX socket: {}", err.to_string()))?
            .into();
        run_server(Server::builder(uc), state, format!("socket {}", socket)).await
    } else {
        let ip = config
            .host
            .unwrap()
            .parse::<IpAddr>()
            .map_err(|_| "Error parsing host IP")?;
        let addr = (ip, config.port.unwrap()).into();
        let builder = Server::try_bind(&addr)
            .map_err(|err| format!("Error binding TCP socket: {}", err.to_string()))?;
        run_server(builder, state, format!("http://{}", addr)).await
    }
}

async fn run_server<I>(
    builder: hyper::server::Builder<I>,
    state: Rc<RefCell<AppState>>,
    address: String,
) -> Result<(), String>
where
    I: hyper::server::accept::Accept + 'static,
    I::Conn: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin,
    I::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let svc = make_service_fn(move |_| {
        let state = state.clone();
        async move {
            let state = state.clone();
            Ok::<_, hyper::Error>(service_fn(move |req| {
                handlers::handle_request::<LocalPool>(req, state.to_owned())
            }))
        }
    });
    let server = builder.executor(LocalExec).serve(svc);
    println!("Listening on {} ...", address);
    server
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| format!("Server terminated: {}", err))
}

#[derive(Clone, Copy, Debug)]
struct LocalExec;

impl<F> hyper::rt::Executor<F> for LocalExec
where
    F: std::future::Future + 'static,
{
    fn execute(&self, fut: F) {
        tokio::task::spawn_local(fut);
    }
}
