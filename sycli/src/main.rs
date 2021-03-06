#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate prettytable;
extern crate clap;
extern crate rpc;
extern crate serde;
extern crate serde_json;
extern crate reqwest;
extern crate url;
extern crate websocket;

use std::process;

use url::Url;
use clap::{App, AppSettings, Arg, SubCommand};
use websocket::ClientBuilder;

mod cmd;

fn main() {
    let matches = App::new("sycli")
        .about("cli interface for synapse")
        .author(env!("CARGO_PKG_AUTHORS"))
        .version(env!("CARGO_PKG_VERSION"))
        .setting(AppSettings::SubcommandRequired)
        .arg(Arg::with_name("server")
             .help("URI of the synapse client to connect to.")
             .short("s")
             .long("server")
             .default_value("ws://localhost:8412/"))
        .arg(Arg::with_name("password")
             .help("Password to use when connecting to synapse.")
             .short("p")
             .long("password")
             .takes_value(true))
        .subcommand(SubCommand::with_name("add")
                    .about("Adds torrents to synapse.")
                    .arg(Arg::with_name("directory")
                         .help("Custom directory to download the torrent to.")
                         .short("d")
                         .long("directory")
                         .takes_value(true))
                    .arg(Arg::with_name("files")
                         .help("Torrent files to add")
                         .multiple(true)
                         .short("f")
                         .long("files")
                         .required(true)
                         .index(1))
                   )
        .subcommand(SubCommand::with_name("del")
                    .about("Deletes torrents from synapse.")
                    .arg(Arg::with_name("torrents")
                         .help("Names of torrents to delete. A fuzzy match will be attempted and ambiguities displayed.")
                         .multiple(true)
                         .short("t")
                         .long("torrents")
                         .required(true)
                         .index(1))
                   )
        .subcommand(SubCommand::with_name("list")
                    .about("Lists resources of a given type in synapse.")
                    .arg(Arg::with_name("filter")
                         .help("Apply an array of json formatted criterion to the resources.")
                         .short("f")
                         .long("filter")
                         .takes_value(true))
                    .arg(Arg::with_name("kind")
                        .help("The kind of resource to list.")
                        .possible_values(&["torrent", "peer", "file", "server", "tracker", "piece"])
                        .default_value("torrent")
                        .short("k")
                        .long("kind"))
                    .arg(Arg::with_name("output")
                         .help("Output the results in the specified format.")
                         .short("o")
                         .long("output")
                         .possible_values(&["json", "text"])
                         .default_value("text")
                        )
                   )
        .subcommand(SubCommand::with_name("pause")
                    .about("Toggles the pause state of the given torrents.")
                    .arg(Arg::with_name("torrents")
                         .help("Names of torrents to pause/unpause. A fuzzy match will be attempted and ambiguities displayed.")
                         .multiple(true)
                         .short("t")
                         .long("torrents")
                         .index(1))
                   )
        .subcommand(SubCommand::with_name("dl")
                    .about("Downloads a torrent.")
                    .arg(Arg::with_name("torrent")
                         .help("Name of torrent to download. A fuzzy match will be attempted and ambiguities displayed.")
                         .short("t")
                         .long("torrent")
                         .index(1)
                         .required(true))
                   )
        .get_matches();

    let mut url = match Url::parse(matches.value_of("server").unwrap()) {
        Ok(url) => url,
        Err(_) => {
            eprintln!("Couldn't parse server URI!");
            process::exit(1);
        }
    };
    if let Some(password) = matches.value_of("password") {
        url.query_pairs_mut().append_pair("password", password);
    }
    let client = match ClientBuilder::new(url.as_str()).unwrap().connect(None) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("Couldn't connect to synapse!");
            process::exit(1);
        }
    };
    if url.scheme() == "wss" {
        url.set_scheme("https").unwrap();
    } else {
        url.set_scheme("http").unwrap();
    }

    match matches.subcommand_name().unwrap() {
        "add" => {
            let args = matches.subcommand_matches("add").unwrap();
            let mut files = Vec::new();
            for file in args.values_of("files").unwrap() {
                files.push(file)
            }
            let res = cmd::add(client, url.as_str(), files, args.value_of("directory"));
            if let Err(e) = res {
                eprintln!("Failed to add torrents: {:?}", e);
                process::exit(1);
            }
        }
        "del" => {
            let args = matches.subcommand_matches("del").unwrap();
            let res = cmd::del(client, args.values_of("torrents").unwrap().collect());
            if let Err(e) = res {
                eprintln!("Failed to delete torrents: {:?}", e);
                process::exit(1);
            }
        }
        "dl" => {
            let args = matches.subcommand_matches("dl").unwrap();
            let res = cmd::dl(client, url.as_str(), args.value_of("torrent").unwrap());
            if let Err(e) = res {
                eprintln!("Failed to download torrent: {:?}", e);
                process::exit(1);
            }
        }
        "list" => {
            let args = matches.subcommand_matches("list").unwrap();
            let crit = args.value_of("filter").and_then(|f| {
                let single_crit = serde_json::from_str(f).map(|c| vec![c]).ok();
                single_crit.or_else(|| serde_json::from_str(f).ok())
            }).unwrap_or(vec![]);
            let kind = args.value_of("kind").unwrap();
            let output = args.value_of("output").unwrap();
            let res = cmd::list(client, kind, crit, output);
            if let Err(e) = res {
                eprintln!("Failed to list torrents: {:?}", e);
                process::exit(1);
            }
        }
        "pause" => {
            let args = matches.subcommand_matches("pause").unwrap();
            let res = cmd::pause(client, args.values_of("torrents").unwrap().collect());
            if let Err(e) = res {
                eprintln!("Failed to pause torrents: {:?}", e);
                process::exit(1);
            }
        }
        _ => { },
    }
}
