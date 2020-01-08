/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License found in the LICENSE file in the root
 * directory of this source tree.
 */

#![deny(unused)]
#![type_length_limit = "2097152"]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Error};
use clap::{value_t, Arg};
use cloned::cloned;
use cmdlib::args;
use cmdlib::helpers::{serve_forever, ARG_FORCE_SHUTDOWN_PERIOD, ARG_SHUTDOWN_GRACE_PERIOD};
use fb303::server::make_FacebookService_server;
use fb303_core::server::make_BaseService_server;
use fbinit::FacebookInit;
use futures_ext::FutureExt as Futures01Ext;
use futures_preview::{FutureExt, TryFutureExt};
use metaconfig_parser::RepoConfigs;
use mononoke_api::{CoreContext, Mononoke};
use panichandler::Fate;
use scuba_ext::ScubaSampleBuilder;
use slog::info;
use source_control::server::make_SourceControlService_server;
use srserver::service_framework::{
    BuildModule, Fb303Module, ProfileModule, ServiceFramework, ThriftStatsModule,
};
use srserver::{ThriftServer, ThriftServerBuilder};
use tokio::runtime::Runtime;

mod commit_id;
mod errors;
mod facebook;
mod from_request;
mod into_response;
mod methods;
mod monitoring;
mod source_control_impl;
mod specifiers;

const ARG_PORT: &str = "port";
const ARG_HOST: &str = "host";
const ARG_SCUBA_DATASET: &str = "scuba-dataset";

const SERVICE_NAME: &str = "mononoke_scs_server";

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    panichandler::set_panichandler(Fate::Abort);

    let matches = args::MononokeApp::new("Mononoke Source Control Service Server")
        .with_advanced_args_hidden()
        .with_all_repos()
        .build()
        .arg(
            Arg::with_name(ARG_HOST)
                .short("H")
                .long("host")
                .takes_value(true)
                .default_value("::")
                .value_name("HOST")
                .help("Thrift host"),
        )
        .arg(
            Arg::with_name(ARG_PORT)
                .short("p")
                .long("port")
                .default_value("8367")
                .value_name("PORT")
                .help("Thrift port"),
        )
        .arg(
            Arg::with_name(ARG_SCUBA_DATASET)
                .long("scuba-dataset")
                .takes_value(true)
                .help("The name of the scuba dataset to log to"),
        )
        .arg(
            Arg::with_name(ARG_SHUTDOWN_GRACE_PERIOD)
                .long("shutdown-grace-period")
                .takes_value(true)
                .required(false)
                .default_value("0"),
        )
        .arg(
            Arg::with_name(ARG_FORCE_SHUTDOWN_PERIOD)
                .long("force-shutdown-period")
                .takes_value(true)
                .required(false)
                .default_value("10"),
        )
        .get_matches();

    let logger = args::init_logging(fb, &matches);
    let caching = args::init_cachelib(fb, &matches);
    let port = value_t!(matches.value_of(ARG_PORT), u16)?;
    let host = matches.value_of(ARG_HOST).unwrap_or("::");
    let config_path = matches
        .value_of("mononoke-config-path")
        .expect("must set config path");

    let mut runtime = Runtime::new().expect("failed to create tokio runtime");
    let exec = runtime.executor();

    let repo_configs = RepoConfigs::read_configs(fb, config_path)?;

    let mut scuba_builder = if let Some(scuba_dataset) = matches.value_of(ARG_SCUBA_DATASET) {
        ScubaSampleBuilder::new(fb, scuba_dataset)
    } else {
        ScubaSampleBuilder::with_discard()
    };

    scuba_builder.add_common_server_data();

    let mononoke = Arc::new(
        runtime.block_on(
            Mononoke::new(
                fb,
                logger.clone(),
                repo_configs,
                args::parse_mysql_options(&matches),
                caching,
                args::parse_readonly_storage(&matches),
            )
            .boxed()
            .compat(),
        )?,
    );

    let will_exit = Arc::new(AtomicBool::new(false));

    // Initialize the FB303 Thrift stack.

    let fb303_base = {
        cloned!(will_exit);
        move |proto| {
            make_BaseService_server(proto, facebook::BaseServiceImpl::new(will_exit.clone()))
        }
    };
    let fb303 = move |proto| {
        make_FacebookService_server(proto, facebook::FacebookServiceImpl, fb303_base.clone())
    };
    let source_control_server = source_control_impl::SourceControlServiceImpl::new(
        fb,
        mononoke.clone(),
        logger.clone(),
        scuba_builder.clone(),
    );
    let service = {
        move |proto| {
            make_SourceControlService_server(
                proto,
                source_control_server.thrift_server(),
                fb303.clone(),
            )
        }
    };

    let monitoring_forever = {
        let monitoring_ctx = CoreContext::new_with_logger(fb, logger.clone());
        monitoring::monitoring_stats_submitter(monitoring_ctx, mononoke)
    };

    let thrift: ThriftServer = ThriftServerBuilder::new(fb)
        .with_name(SERVICE_NAME)
        .expect("failed to set name")
        .with_address(&host, port.into(), false)?
        .with_tls()
        .expect("failed to enable TLS")
        .with_factory(exec, move || service)
        .build();

    let mut service_framework = ServiceFramework::from_server(SERVICE_NAME, thrift, port as u32)
        .context("Failed to create service framework server")?;

    service_framework.add_module(BuildModule)?;
    service_framework.add_module(ThriftStatsModule)?;
    service_framework.add_module(Fb303Module)?;
    service_framework.add_module(ProfileModule)?;

    // Start listening.
    info!(logger, "Listening on {}:{}", &host, port);
    service_framework
        .serve_background()
        .expect("failed to start thrift service");
    serve_forever(
        runtime,
        monitoring_forever.discard(),
        &logger,
        will_exit,
        &matches,
    )?;
    drop(service_framework);

    info!(logger, "Exiting...");
    Ok(())
}
