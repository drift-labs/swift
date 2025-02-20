use clap::{Arg, Command};

mod confirmation_server;
mod connection;
mod fastlane_server;
mod super_slot_subscriber;
mod types;
mod util;
mod ws_server;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    env_logger::init();

    let matches = Command::new("Fastlane Server")
        .version("1.0")
        .arg(
            Arg::new("server")
                .long("server")
                .value_name("SERVER_TYPE")
                .help("Sets the type of server")
                .default_value("fastlane")
                .value_parser(["fastlane", "ws", "confirmation"]),
        )
        .get_matches();

    let server_type = matches
        .get_one::<String>("server")
        .expect("default is provided");

    match server_type.as_str() {
        "confirmation" => {
            // Run the confirmation server
            confirmation_server::start_server().await;
        }
        "fastlane" => {
            // Run the fastlane http server
            fastlane_server::start_server().await;
        }
        "ws" => {
            // Run the WebSocket server
            ws_server::start_server().await;
        }
        _ => {
            log::error!("Invalid server type");
        }
    }
}
