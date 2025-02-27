//! `version` subcommand

#![allow(clippy::never_loop)]

use super::files_cmd;
use crate::entrypoint;
use crate::prelude::app_config;
use abscissa_core::{status_info, status_ok, Command, Options, Runnable};
use diem_genesis_tool::ol_node_files;
use diem_types::{transaction::SignedTransaction, waypoint::Waypoint};
use diem_wallet::WalletLibrary;
use ol::{commands::init_cmd, config::AppCfg};
use ol_fixtures::get_test_genesis_blob;
use ol_keys::{scheme::KeyScheme, wallet};
use ol_types::block::Block;
use ol_types::config::IS_TEST;
use ol_types::{account::ValConfigs, config::TxType, pay_instruction::PayInstruction};
use reqwest::Url;
use std::fs;
use std::process::exit;
use std::{fs::File, io::Write, path::PathBuf};
use txs::{commands::autopay_batch_cmd, submit_tx};

/// `validator wizard` subcommand
#[derive(Command, Debug, Default, Options)]
pub struct ValWizardCmd {
    #[options(
        short = "a",
        help = "where to output the account.json file, defaults to node home"
    )]
    output_path: Option<PathBuf>,
    #[options(help = "explicitly set home path instead of answer in wizard, for CI usually")]
    home_path: Option<PathBuf>,
    #[options(help = "id of the chain")]
    chain_id: Option<u8>,
    #[options(help = "github org of genesis repo")]
    github_org: Option<String>,
    #[options(help = "repo with with genesis transactions")]
    repo: Option<String>,
    #[options(help = "use a genesis file instead of building")]
    prebuilt_genesis: Option<PathBuf>,
    #[options(help = "fetching genesis blob from github")]
    fetch_git_genesis: bool,
    #[options(help = "skip mining a block zero")]
    skip_mining: bool,
    #[options(short = "u", help = "template account.json to configure from")]
    template_url: Option<Url>,
    #[options(help = "autopay file if instructions are to be sent")]
    autopay_file: Option<PathBuf>,
    #[options(help = "An upstream peer to use in 0L.toml")]
    upstream_peer: Option<Url>,
    #[options(help = "If validator is building from source")]
    source_path: Option<PathBuf>,
    #[options(short = "w", help = "If validator is building from source")]
    waypoint: Option<Waypoint>,
    #[options(short = "e", help = "If validator is building from source")]
    epoch: Option<u64>,
    #[options(help = "For testing in ci, use genesis.blob fixtures")]
    ci: bool,
    #[options(help = "Used only on genesis ceremony")]
    genesis_ceremony: bool,
}

impl Runnable for ValWizardCmd {
    fn run(&self) {
        // Note. `onboard` command DOES NOT READ CONFIGS FROM 0L.toml

        status_info!(
            "\nValidator Config Wizard.", "Next you'll enter your mnemonic and some other info to configure your validator node and on-chain account. If you haven't yet generated keys, run the standalone keygen tool with 'ol keygen'.\n\nYour first 0L proof-of-work will be mined now. Expect this to take up to 15 minutes on modern CPUs.\n"
        );

        let entry_args = entrypoint::get_args();

        // Get credentials from prompt
        let (authkey, account, wallet) = wallet::get_account_from_prompt();

        let upstream_peer = if *&self.genesis_ceremony {
            None
        } else {
            let mut upstream = self.upstream_peer.clone().unwrap_or_else(|| {
              self.template_url.clone().unwrap_or_else(|| {
                  print!("ERROR: Must set a URL to query chain. Use --upstream-peer of --template-url, exiting.");
                  exit(1)  
              })
            });
            upstream.set_port(Some(8080)).unwrap();
            println!("Setting upstream peer URL to: {:?}", &upstream.as_str());
            Some(upstream)
        };

        let app_config = AppCfg::init_app_configs(
            authkey,
            account,
            &upstream_peer,
            &self.home_path,
            &self.epoch,
            &self.waypoint,
            &self.source_path,
            None,
            None,
        );
        let home_path = &app_config.workspace.node_home;
        let base_waypoint = app_config.chain_info.base_waypoint.clone();

        status_ok!("\nApp configs written", "\n...........................\n");

        if let Some(url) = &self.template_url {
            let mut url = url.to_owned();
            url.set_port(Some(3030)).unwrap(); //web port
            save_template(&url.join("account.json").unwrap(), home_path);
            // get autopay
            status_ok!("\nTemplate saved", "\n...........................\n");
        }

        // Use any autopay instructions
        // TODO: simplify signature
        let (autopay_batch, autopay_signed) = get_autopay_batch(
            &self.template_url,
            &self.autopay_file,
            home_path,
            &app_config,
            &wallet,
            entry_args.swarm_path.as_ref().is_some(),
            *&self.genesis_ceremony,
        );
        status_ok!(
            "\nAutopay transactions signed",
            "\n...........................\n"
        );

        // Initialize Validator Keys
        init_cmd::initialize_validator(&wallet, &app_config, base_waypoint, *&self.genesis_ceremony).expect("could not initialize validator key_store.json");
        status_ok!("\nKey file written", "\n...........................\n");

        if !self.genesis_ceremony {
            // fetching the genesis files from genesis-archive, will override the path for prebuilt genesis.
            let mut prebuilt_genesis_path = self.prebuilt_genesis.clone();
            if self.fetch_git_genesis {
                files_cmd::get_files(home_path.clone(), &self.github_org, &self.repo);
                status_ok!(
                    "\nDownloaded genesis files",
                    "\n...........................\n"
                );

                prebuilt_genesis_path = Some(home_path.join("genesis.blob"));
            } else if self.ci {
                fs::copy(
                    get_test_genesis_blob().as_os_str(),
                    home_path.join("genesis.blob"),
                )
                .unwrap();
                prebuilt_genesis_path = Some(home_path.join("genesis.blob"));
                status_ok!(
                    "\nUsing test genesis.blob",
                    "\n...........................\n"
                );
            }

            let home_dir = app_config.workspace.node_home.to_owned();
            // 0L convention is for the namespace of the operator to be appended by '-oper'
            let namespace = app_config.profile.auth_key.clone() + "-oper";

            // TODO: use node_config to get the seed peers and then write upstream_node vec in 0L.toml from that.
            ol_node_files::write_node_config_files(
                home_dir.clone(),
                self.chain_id.unwrap_or(1),
                &self.github_org.clone().unwrap_or("OLSF".to_string()),
                &self
                    .repo
                    .clone()
                    .unwrap_or("experimental-genesis".to_string()),
                &namespace,
                &prebuilt_genesis_path,
                &false,
                base_waypoint,
                &None,
            )
            .unwrap();

            status_ok!("\nNode config written", "\n...........................\n");
        }

        if !self.skip_mining {
            // Mine Block
            miner::block::write_genesis(&app_config);
            status_ok!(
                "\nGenesis proof complete",
                "\n...........................\n"
            );
        }

        // Write account manifest
        write_account_json(
            &self.output_path,
            wallet,
            Some(app_config.clone()),
            autopay_batch,
            autopay_signed,
        );
        status_ok!(
            "\nAccount manifest written",
            "\n...........................\n"
        );

        status_info!(
            "Your validator node and miner app are now configured.", 
            &format!(
                "\nStart your node with `ol start`, and then ask someone with GAS to do this transaction `txs create-validator -u http://{}`",
                &app_config.profile.ip
            )
        );
    }
}

/// get autopay instructions from file
pub fn get_autopay_batch(
    template: &Option<Url>,
    file_path: &Option<PathBuf>,
    home_path: &PathBuf,
    cfg: &AppCfg,
    wallet: &WalletLibrary,
    is_swarm: bool,
    is_genesis: bool,
) -> (Option<Vec<PayInstruction>>, Option<Vec<SignedTransaction>>) {
    let file_name = if template.is_some() {
        // assumes the template was downloaded from URL
        "template.json"
    } else {
        "autopay_batch.json"
    };

    let starting_epoch = cfg.chain_info.base_epoch.unwrap_or(0);
    let instr_vec = PayInstruction::parse_autopay_instructions(
        &file_path.clone().unwrap_or(home_path.join(file_name)),
        Some(starting_epoch.clone()),
        None,
    )
    .unwrap();

    if is_genesis { return (Some(instr_vec), None ) }


    let script_vec = autopay_batch_cmd::process_instructions(instr_vec.clone());
    let url = cfg.what_url(false);
    let mut tx_params = submit_tx::get_tx_params_from_toml(
        cfg.to_owned(),
        TxType::Miner,
        Some(wallet),
        url,
        None,
        is_swarm,
    )
    .unwrap();
    let tx_expiration_sec = if *IS_TEST {
        // creating fixtures here, so give it near infinite expiry
        100 * 360 * 24 * 60 * 60
    } else {
        // give the tx a very long expiration, 7 days.
        7 * 24 * 60 * 60
    };
    tx_params.tx_cost.user_tx_timeout = tx_expiration_sec;
    let txn_vec = autopay_batch_cmd::sign_instructions(script_vec, 0, &tx_params);
    (Some(instr_vec), Some(txn_vec))
}

/// save template file
pub fn save_template(url: &Url, home_path: &PathBuf) -> PathBuf {
    let g_res = reqwest::blocking::get(&url.to_string());
    let g_path = home_path.join("template.json");
    let mut g_file = File::create(&g_path).expect("couldn't create file");
    let g_content = g_res
        .unwrap()
        .bytes()
        .expect("cannot connect to upstream node")
        .to_vec(); //.text().unwrap();
    g_file.write_all(g_content.as_slice()).unwrap();
    g_path
}

/// Creates an account.json file for the validator
pub fn write_account_json(
    json_path: &Option<PathBuf>,
    wallet: WalletLibrary,
    wizard_config: Option<AppCfg>,
    autopay_batch: Option<Vec<PayInstruction>>,
    autopay_signed: Option<Vec<SignedTransaction>>,
) {
    let cfg = wizard_config.unwrap_or(app_config().clone());
    let json_path = json_path.clone().unwrap_or(cfg.workspace.node_home.clone());
    let keys = KeyScheme::new(&wallet);
    let block = Block::parse_block_file(cfg.get_block_dir().join("block_0.json").to_owned());

    ValConfigs::new(
        block,
        keys,
        cfg.profile.ip.to_string(),
        autopay_batch,
        autopay_signed,
    )
    .create_manifest(json_path);
}
