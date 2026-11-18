use std::error::Error;
use std::io::{self, BufRead, Write};
use std::path::Path;

use epiphany_server::{Auth, app_with, open, openapi, set_user, with_lab};
use tokio::net::TcpListener;

const ADDRESS: &str = "127.0.0.1:8080";
const USAGE: &str = "usage:
  epiphany-server serve <directory> [address]   serve the database kept in a directory
  epiphany-server lab <directory> [address]     the same, plus the test page
                                                <directory>/index.html at /lab
  epiphany-server user <directory> <name>       add a user, or change a user's password;
                                                the password is read from standard input
  epiphany-server openapi                       print the OpenAPI document

The directory holds the model in <directory>/model, keeps its data in <directory>/data and
its users in <directory>/users.json. The address defaults to 127.0.0.1:8080. Once the
directory has a user, every request must come from a signed-in session; until then the
server asks for no sign-in and only listens on this machine.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let (command, directory, address) = match arguments.as_slice() {
        ["openapi"] => {
            println!("{}", openapi().to_pretty_json()?);
            return Ok(());
        }
        ["user", directory, name] => return user(Path::new(directory), name),
        [command @ ("serve" | "lab"), directory] => (*command, Path::new(directory), ADDRESS),
        [command @ ("serve" | "lab"), directory, address] => {
            (*command, Path::new(directory), *address)
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    let auth = Auth::load(directory)?;
    let listener = TcpListener::bind(address).await?;
    let local = listener.local_addr()?;
    if auth.is_none() && !local.ip().is_loopback() {
        return Err(format!(
            "{local} can be reached from other machines, and this directory has no users. \
             Add one with 'epiphany-server user', or listen on 127.0.0.1."
        )
        .into());
    }
    let signing = match auth {
        Some(_) => "users must sign in",
        None => "no sign-in",
    };
    let application = app_with(open(directory)?, auth);
    let (application, page) = match command {
        "lab" => (with_lab(application, directory.join("index.html")), "/lab"),
        _ => (application, ""),
    };
    println!("listening on http://{local}{page} ({signing})");
    axum::serve(listener, application).await?;
    Ok(())
}

/// Adds a user, reading the password from standard input so that it is in no command line.
fn user(directory: &Path, name: &str) -> Result<(), Box<dyn Error>> {
    eprint!("Password for {name} (it will be shown as you type): ");
    io::stderr().flush()?;
    let mut password = String::new();
    io::stdin().lock().read_line(&mut password)?;
    // Some shells put a byte-order mark before text they pipe in; it is no part of a password.
    let password = password
        .trim_start_matches('\u{feff}')
        .trim_end_matches(['\r', '\n']);
    set_user(directory, name, password)?;
    eprintln!("Saved. Restart the server for it to take effect.");
    Ok(())
}
