use std::io;
use std::process;

use env_logger::Env;
use mdbook::renderer::RenderContext;
use renderer::ConfluenceRenderer;
use renderer::{ConfluenceConfig, Error};

#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate lazy_static;

mod client;
mod renderer;

#[tokio::main]
async fn main() {
    env_logger::init_from_env(Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"));

    if let Err(e) = render().await {
        error!("{:?}", e);
        process::exit(1);
    }
}

async fn render() -> Result<(), Error> {
    let context = RenderContext::from_json(io::stdin())?;
    let config: ConfluenceConfig = context
        .config
        .get_deserialized_opt("output.confluence")
        .map(|c| c.unwrap_or_default())
        .unwrap_or_default(); // TODO throw an error here

    if config.enabled {
        let confluence_renderer = ConfluenceRenderer::new(config).await?;

        if context.version != mdbook::MDBOOK_VERSION {
            // We should probably use the `semver` crate to check compatibility
            // here...
            warn!(
                "Warning: The {} plugin was built against version {} of mdbook, \
             but we're being called from version {}",
                renderer::RENDERER_NAME,
                mdbook::MDBOOK_VERSION,
                context.version
            );
        }

        confluence_renderer.render(context).await?;
    } else {
        info!("Confluence renderer is disabled")
    }

    Ok(())
}
