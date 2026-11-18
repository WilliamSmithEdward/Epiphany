use std::error::Error;
use std::path::Path;

use epiphany_core::database::Database;
use epiphany_server::{app, load_model, openapi};
use tokio::net::TcpListener;

const USAGE: &str = "usage:
  epiphany-server serve <model-directory> [address]   serve the model (default 127.0.0.1:8080)
  epiphany-server openapi                             print the OpenAPI document";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match arguments.as_slice() {
        ["openapi"] => println!("{}", openapi().to_pretty_json()?),
        ["serve", model] => serve(model, "127.0.0.1:8080").await?,
        ["serve", model, address] => serve(model, address).await?,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
    Ok(())
}

async fn serve(model: &str, address: &str) -> Result<(), Box<dyn Error>> {
    let database = Database::new(load_model(Path::new(model))?)?;
    let listener = TcpListener::bind(address).await?;
    println!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app(database)).await?;
    Ok(())
}
