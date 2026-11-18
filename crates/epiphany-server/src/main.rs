use std::error::Error;
use std::path::Path;

use epiphany_server::{app, lab, open, openapi};
use tokio::net::TcpListener;

const ADDRESS: &str = "127.0.0.1:8080";
const USAGE: &str = "usage:
  epiphany-server serve <directory> [address]   serve the database kept in a directory
  epiphany-server lab <directory> [address]     the same, plus the test page
                                                <directory>/index.html at /lab
  epiphany-server openapi                       print the OpenAPI document

The directory holds the model in <directory>/model and keeps its data in <directory>/data.
The address defaults to 127.0.0.1:8080.";

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
    let database = open(directory)?;
    let (application, page) = match command {
        "lab" => (lab(database, directory.join("index.html")), "/lab"),
        _ => (app(database), ""),
    };
    let listener = TcpListener::bind(address).await?;
    println!("listening on http://{}{page}", listener.local_addr()?);
    axum::serve(listener, application).await?;
    Ok(())
}
