use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;

use epiphany_gui::Gui;
use tokio::net::TcpListener;

const USAGE: &str = "usage:
  epiphany-gui [--listen <address>] [--server <url>] [--web <folder>]

  --listen   where to listen; 0.0.0.0:80 unless given
  --server   the Epiphany server to pass API calls to; http://127.0.0.1:8080 unless given
  --web      serve the application's files from this folder, read afresh on every
             request, instead of the copy built in

Browsers on other machines must sign in before any call is passed on, so a server that
asks no one to sign in is only reachable from this one.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut listen = "0.0.0.0:80".to_owned();
    let mut server = "http://127.0.0.1:8080".to_owned();
    let mut web = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let value = arguments.next();
        match (argument.as_str(), value) {
            ("--listen", Some(value)) => listen = value,
            ("--server", Some(value)) => server = value,
            ("--web", Some(value)) => web = Some(PathBuf::from(value)),
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    if !server.starts_with("http://") {
        return Err(format!("the server is reached as http://host:port, not '{server}'").into());
    }
    let listener = match TcpListener::bind(&listen).await {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(format!(
                "{listen} needs rights this program does not ask for: on Linux a port below \
                 1024 is kept for root. Choose another, as in --listen 0.0.0.0:8000."
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            return Err(format!(
                "{listen} is in use by another program. Choose another, as in --listen 0.0.0.0:8000."
            )
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    let local = listener.local_addr()?;
    println!("listening on http://{local}, passing API calls to {server}");
    let application = Gui::new(&server, web).router();
    axum::serve(
        listener,
        application.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
