use std::error::Error;
use std::path::Path;

use axum::Router;
use epiphany_core::database::Database;
use epiphany_server::{app, lab, load_model, openapi};
use tokio::net::TcpListener;

const ADDRESS: &str = "127.0.0.1:8080";
const USAGE: &str = "usage:
  epiphany-server serve <model-directory> [address]   serve the model (default 127.0.0.1:8080)
  epiphany-server lab <lab-directory> [address]       serve <lab-directory>/model and the test
                                                      page <lab-directory>/index.html at /lab
  epiphany-server openapi                             print the OpenAPI document";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let (command, directory, address) = match arguments.as_slice() {
        ["openapi"] => {
            println!("{}", openapi().to_pretty_json()?);
            return Ok(());
        }
        [command @ ("serve" | "lab"), directory] => (*command, Path::new(directory), ADDRESS),
        [command @ ("serve" | "lab"), directory, address] => {
            (*command, Path::new(directory), *address)
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    let (application, page): (Router, &str) = match command {
        "lab" => {
            let database = Database::new(load_model(&directory.join("model"))?)?;
            (lab(database, directory.join("index.html")), "/lab")
        }
        _ => (app(Database::new(load_model(directory)?)?), ""),
    };
    let listener = TcpListener::bind(address).await?;
    println!("listening on http://{}{page}", listener.local_addr()?);
    axum::serve(listener, application).await?;
    Ok(())
}
