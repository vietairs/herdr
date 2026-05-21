use crate::api::schema::{EmptyParams, Method, Request};

pub(super) fn run_server_command(args: &[String]) -> std::io::Result<Option<i32>> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        return Ok(None);
    };

    match subcommand {
        "stop" => server_stop(&args[1..]).map(Some),
        "reload-config" => server_reload_config(&args[1..]).map(Some),
        "help" | "--help" | "-h" => {
            print_server_help();
            Ok(Some(0))
        }
        _ => {
            print_server_help();
            Ok(Some(2))
        }
    }
}

fn server_stop(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr server stop");
        return Ok(2);
    }

    super::send_ok_request(Method::ServerStop(EmptyParams::default()))
}

fn server_reload_config(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr server reload-config");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:server:reload-config".into(),
        method: Method::ServerReloadConfig(EmptyParams::default()),
    })?)
}

fn print_server_help() {
    eprintln!("herdr server commands:");
    eprintln!("  herdr server                run as headless server");
    eprintln!("  herdr server stop           stop the running server via the API socket");
    eprintln!("  herdr server reload-config  reload config.toml in the running server");
}
