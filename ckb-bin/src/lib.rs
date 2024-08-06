//! CKB executable.
//!
//! This crate is created to reduce the link time to build CKB.
mod helper;
mod setup_guard;
mod subcommand;
use ckb_app_config::{cli, ExitCode, Setup};
use ckb_async_runtime::new_global_runtime;
use ckb_build_info::Version;
use ckb_logger::{debug, info};
use ckb_network::tokio;
use clap::ArgMatches;
use helper::raise_fd_limit;
use setup_guard::SetupGuard;

#[cfg(not(target_os = "windows"))]
use colored::Colorize;
#[cfg(not(target_os = "windows"))]
use daemonize::Daemonize;
#[cfg(not(target_os = "windows"))]
use subcommand::check_process;
#[cfg(feature = "with_sentry")]
pub(crate) const LOG_TARGET_SENTRY: &str = "sentry";

/// The executable main entry.
///
/// It returns `Ok` when the process exist normally, otherwise the `ExitCode` is converted to the
/// process exit status code.
///
/// ## Parameters
///
/// * `version` - The version is passed in so the bin crate can collect the version without trigger
/// re-linking.
pub fn run_app(version: Version) -> Result<(), ExitCode> {
    // Always print backtrace on panic.
    ::std::env::set_var("RUST_BACKTRACE", "full");

    let (bin_name, app_matches) = cli::get_bin_name_and_matches(&version);
    if let Some((cli, matches)) = app_matches.subcommand() {
        match cli {
            cli::CMD_INIT => {
                return subcommand::init(Setup::init(matches)?);
            }
            cli::CMD_LIST_HASHES => {
                return subcommand::list_hashes(Setup::root_dir_from_matches(matches)?, matches);
            }
            cli::CMD_PEERID => {
                if let Some((cli, matches)) = matches.subcommand() {
                    match cli {
                        cli::CMD_GEN_SECRET => return Setup::gen(matches),
                        cli::CMD_FROM_SECRET => {
                            return subcommand::peer_id(Setup::peer_id(matches)?)
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let (cmd, matches) = app_matches
        .subcommand()
        .expect("SubcommandRequiredElseHelp");

    #[cfg(not(target_os = "windows"))]
    if run_daemon(cmd, matches) {
        return run_app_in_daemon(version, bin_name, cmd, matches);
    }

    debug!("ckb version: {}", version);
    run_app_inner(version, bin_name, cmd, matches)
}

#[cfg(not(target_os = "windows"))]
fn run_app_in_daemon(
    version: Version,
    bin_name: String,
    cmd: &str,
    matches: &ArgMatches,
) -> Result<(), ExitCode> {
    eprintln!("starting CKB in daemon mode ...");
    eprintln!("check status : `{}`", "ckb daemon --check".green());
    eprintln!("stop daemon  : `{}`", "ckb daemon --stop".yellow());

    assert!(matches!(cmd, cli::CMD_RUN));
    let root_dir = Setup::root_dir_from_matches(matches)?;
    let daemon_dir = root_dir.join("data/daemon");
    // make sure daemon dir exists
    std::fs::create_dir_all(daemon_dir)?;
    let pid_file = Setup::daemon_pid_file_path(matches)?;

    if check_process(&pid_file).is_ok() {
        eprintln!("{}", "ckb is already running".red());
        return Ok(());
    }
    eprintln!("no ckb process, starting ...");

    let pwd = std::env::current_dir()?;
    let daemon = Daemonize::new()
        .pid_file(pid_file)
        .chown_pid_file(true)
        .working_directory(pwd);

    match daemon.start() {
        Ok(_) => {
            info!("Success, daemonized ...");
            run_app_inner(version, bin_name, cmd, matches)
        }
        Err(e) => {
            info!("daemonize error: {}", e);
            Err(ExitCode::Failure)
        }
    }
}

fn run_app_inner(
    version: Version,
    bin_name: String,
    cmd: &str,
    matches: &ArgMatches,
) -> Result<(), ExitCode> {
    let is_silent_logging = is_silent_logging(cmd);
    let (mut handle, mut handle_stop_rx, _runtime) = new_global_runtime(None);
    let setup = Setup::from_matches(bin_name, cmd, matches)?;
    let _guard = SetupGuard::from_setup(&setup, &version, handle.clone(), is_silent_logging)?;

    raise_fd_limit();

    let ret = match cmd {
        cli::CMD_RUN => subcommand::run(setup.run(matches)?, version, handle.clone()),
        cli::CMD_MINER => subcommand::miner(setup.miner(matches)?, handle.clone()),
        cli::CMD_REPLAY => subcommand::replay(setup.replay(matches)?, handle.clone()),
        cli::CMD_EXPORT => subcommand::export(setup.export(matches)?, handle.clone()),
        cli::CMD_IMPORT => subcommand::import(setup.import(matches)?, handle.clone()),
        cli::CMD_STATS => subcommand::stats(setup.stats(matches)?, handle.clone()),
        cli::CMD_RESET_DATA => subcommand::reset_data(setup.reset_data(matches)?),
        cli::CMD_MIGRATE => subcommand::migrate(setup.migrate(matches)?),
        #[cfg(not(target_os = "windows"))]
        cli::CMD_DAEMON => subcommand::daemon(setup.daemon(matches)?),
        "test" => test_rpc(
            setup.run(matches)?,
            version,
            handle.clone(),
            matches
                .get_one::<String>("number")
                .unwrap()
                .parse()
                .unwrap(),
        ),
        _ => unreachable!(),
    };

    if matches!(cmd, cli::CMD_RUN) {
        handle.drop_guard();

        tokio::task::block_in_place(|| {
            info!("Waiting for all tokio tasks to exit...");
            handle_stop_rx.blocking_recv();
            info!("All tokio tasks and threads have exited. CKB shutdown");
        });
    }

    ret
}

fn test_rpc(
    args: ckb_app_config::RunArgs,
    version: Version,
    async_handle: ckb_async_runtime::Handle,
    number: usize,
) -> Result<(), ExitCode> {
    use ckb_indexer::IndexerService;
    use ckb_indexer_sync::{new_secondary_db, PoolService};

    println!("number {}", number);
    let rpc_threads_num = {
        let system_parallelism: usize = std::thread::available_parallelism().unwrap().into();
        let default_num = usize::max(system_parallelism, 1);
        args.config.rpc.threads.unwrap_or(default_num)
    };
    info!("ckb version: {}", version);
    let (mut rpc_handle, _rpc_stop_rx, _runtime) = new_global_runtime(Some(rpc_threads_num));
    let launcher = ckb_launcher::Launcher::new(args, version, async_handle, rpc_handle.clone());

    let block_assembler_config = launcher.sanitize_block_assembler_config()?;
    let miner_enable = block_assembler_config.is_some();

    launcher.check_indexer_config()?;

    let (shared, mut pack) = launcher.build_shared(block_assembler_config)?;

    let ckb_secondary_db = new_secondary_db(
        &launcher.args.config.db,
        &((&launcher.args.config.indexer).into()),
    );
    let pool_service = PoolService::new(
        launcher.args.config.indexer.index_tx_pool,
        shared.async_handle().clone(),
    );

    let mut indexer = IndexerService::new(
        ckb_secondary_db.clone(),
        pool_service.clone(),
        &launcher.args.config.indexer,
        shared.async_handle().clone(),
    );
    indexer.spawn_poll(shared.notify_controller().clone());

    let indexer_handle = indexer.handle();

    for _ in 0..number {
        let search_key: ckb_jsonrpc_types::IndexerSearchKey = serde_json::from_str(r#"{"script": {"code_hash": "0x709f3fda12f561cfacf92273c57a98fede188a3f1a59b1f888d113f9cce08649","hash_type": "data",  "args": "0x30e87dd4b3d46bbd1521c3efb3405e0693669831"},"script_type": "lock","script_search_mode": "exact"}"#).unwrap();

        let cells = indexer_handle.get_cells_with_snapshot(
            search_key,
            ckb_jsonrpc_types::IndexerOrder::Asc,
            u32::from_str_radix("ffffff", 16).unwrap().into(),
            None,
        );
        drop(cells)
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_daemon(cmd: &str, matches: &ArgMatches) -> bool {
    match cmd {
        cli::CMD_RUN => matches.get_flag(cli::ARG_DAEMON),
        _ => false,
    }
}

type Silent = bool;

fn is_silent_logging(cmd: &str) -> Silent {
    matches!(
        cmd,
        cli::CMD_EXPORT
            | cli::CMD_IMPORT
            | cli::CMD_STATS
            | cli::CMD_MIGRATE
            | cli::CMD_RESET_DATA
            | cli::CMD_DAEMON
    )
}
