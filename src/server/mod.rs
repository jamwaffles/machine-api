pub mod context;
pub mod endpoints;

use std::{env, net::SocketAddr, sync::Arc};

use anyhow::{anyhow, Result};
use dropshot::{ApiDescription, ConfigDropshot, HttpServerStarter};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};

use crate::{config::Config, network_printer::NetworkPrinterManufacturer, server::context::Context};

/// Create an API description for the server.
pub fn create_api_description() -> Result<ApiDescription<Arc<Context>>> {
    fn register_endpoints(api: &mut ApiDescription<Arc<Context>>) -> Result<(), String> {
        api.register(crate::server::endpoints::ping).unwrap();
        api.register(crate::server::endpoints::api_get_schema).unwrap();
        api.register(crate::server::endpoints::print_file).unwrap();
        api.register(crate::server::endpoints::get_machines).unwrap();
        api.register(crate::server::endpoints::get_machine).unwrap();

        // YOUR ENDPOINTS HERE!

        Ok(())
    }

    // Describe the API.
    let tag_config = serde_json::from_str(include_str!("../../openapi/tag-config.json")).unwrap();
    let mut api = ApiDescription::new().tag_config(tag_config);

    if let Err(err) = register_endpoints(&mut api) {
        panic!("failed to register entrypoints: {}", err);
    }

    Ok(api)
}

pub async fn create_server(
    s: &crate::Server,
    opts: &crate::Opts,
    config: &Config,
) -> Result<(dropshot::HttpServer<Arc<Context>>, Arc<Context>)> {
    let mut api = create_api_description()?;
    let schema = get_openapi(&mut api)?;

    let config_dropshot = ConfigDropshot {
        bind_address: s.address.parse()?,
        request_body_max_bytes: 107374182400, // 100 Gigiabytes.
        default_handler_task_mode: dropshot::HandlerTaskMode::CancelOnDisconnect,
    };

    let logger = opts.create_logger("server");
    let dropshot_logger = logger.new(slog::o!("component" => "dropshot"));

    let api_context = Arc::new(Context::new(config, schema, logger).await?);

    let server = HttpServerStarter::new(&config_dropshot, api, api_context.clone(), &dropshot_logger)
        .map_err(|error| anyhow!("failed to create server: {}", error))?
        .start();

    Ok((server, api_context))
}

/// Get the OpenAPI specification for the server.
pub fn get_openapi(api: &mut ApiDescription<Arc<Context>>) -> Result<serde_json::Value> {
    // Create the API schema.
    let mut definition = api.openapi("machine-api", clap::crate_version!());
    definition
        .description("")
        .contact_url("https://zoo.dev")
        .contact_email("machine-api@zoo.dev")
        .json()
        .map_err(|e| e.into())
}

pub async fn server(s: &crate::Server, opts: &crate::Opts, config: &Config) -> Result<()> {
    let (server, api_context) = create_server(s, opts, config).await?;
    let addr: SocketAddr = s.address.parse()?;

    let responder = libmdns::Responder::new().unwrap();
    let _svc = responder.register(
        "_machine-api._tcp".to_owned(),
        "Machine Api Server".to_owned(),
        addr.port(),
        &["path=/"],
    );

    // For Cloud run & ctrl+c, shutdown gracefully.
    // "The main process inside the container will receive SIGTERM, and after a grace period,
    // SIGKILL."
    // Regsitering SIGKILL here will panic at runtime, so let's avoid that.
    let mut signals = Signals::new([SIGINT, SIGTERM])?;

    let cloned_api_context = api_context.clone();
    tokio::spawn(async move {
        if let Some(sig) = signals.forever().next() {
            slog::info!(cloned_api_context.logger, "received signal: {:?}", sig);
            slog::info!(cloned_api_context.logger, "triggering cleanup...");

            // Exit the process.
            slog::info!(cloned_api_context.logger, "all clean, exiting!");
            std::process::exit(0);
        }
    });

    // Start all the discovery tasks.
    tokio::spawn(async move {
        // TODO: Restart tasks if they fail, with some kind of backoff timer
        let form_labs = api_context
            .network_printers
            .get(&NetworkPrinterManufacturer::Formlabs)
            .expect("No formlabs discover task registered");
        let bambu = api_context
            .network_printers
            .get(&NetworkPrinterManufacturer::Bambu)
            .expect("No Bambu discover task registered");

        tokio::join!(form_labs.discover(), bambu.discover())
    });

    server.await.map_err(|error| anyhow!("server failed: {}", error))?;

    Ok(())
}
