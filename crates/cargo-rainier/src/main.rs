//! `cargo rainier` — the feature set a deployment needs, from anywhere.
//!
//! ```text
//! cargo install cargo-rainier
//!
//! cargo rainier features                     # what .env implies, with reasons
//! cargo rainier features --env .env.production
//! cargo rainier features --check             # CI: fail on an unforwarded selection
//! cargo rainier build --env .env.production --release
//! ```
//!
//! The logic — and the reasoning about why cargo cannot do this by itself —
//! is [`rainier_features`]; this is the standalone door to it. A workspace
//! that prefers no globally-installed tools can put a five-line `xtask` over
//! the same library instead, which is what the sample project does.

use std::path::PathBuf;
use std::process::Command;

use rainier_features::{compute, parse_env, read_sources, Report};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Invoked as `cargo rainier …`, cargo hands us "rainier" first.
    if args.first().map(String::as_str) == Some("rainier") {
        args.remove(0);
    }

    let command = if args.is_empty() { String::new() } else { args.remove(0) };

    let code = match command.as_str() {
        "features" => features_command(&args),
        "build" => build_command(&args),
        _ => {
            eprintln!(
                "usage:\n  cargo rainier features [--env <file>] [--check]\n  cargo rainier \
                 build [--env <file>] [<cargo args>…]"
            );
            2
        }
    };

    std::process::exit(code);
}

fn features_command(args: &[String]) -> i32 {
    let check = args.iter().any(|a| a == "--check");

    let Some(report) = load(args) else { return 1 };

    println!("# from {}", env_path(args).display());
    for line in &report.reasons {
        println!("#   {line}");
    }
    if report.features.is_empty() {
        println!("# nothing beyond the defaults");
    }
    println!("{}", report.build_command());

    if !report.unforwarded.is_empty() {
        for problem in &report.unforwarded {
            eprintln!("error: {problem}");
        }
        if check {
            return 1;
        }
    }

    0
}

fn build_command(args: &[String]) -> i32 {
    let Some(report) = load(args) else { return 1 };

    if !report.unforwarded.is_empty() {
        for problem in &report.unforwarded {
            eprintln!("error: {problem}");
        }
        return 1;
    }

    let mut cargo = Command::new("cargo");
    cargo.arg("build").arg("--no-default-features");

    if !report.features.is_empty() {
        cargo.arg("--features").arg(report.feature_list());
    }

    // Everything except the `--env <file>` pair goes to cargo untouched.
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--env" {
            skip_next = true;
            continue;
        }
        cargo.arg(arg);
    }

    match cargo.status() {
        Ok(status) if status.success() => 0,
        Ok(_) => 1,
        Err(why) => {
            eprintln!("could not run cargo: {why}");
            1
        }
    }
}

fn load(args: &[String]) -> Option<Report> {
    let path = env_path(args);

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(why) => {
            eprintln!("could not read {}: {why}", path.display());
            return None;
        }
    };

    Some(compute(&parse_env(&text), &read_sources(std::path::Path::new("src"))))
}

fn env_path(args: &[String]) -> PathBuf {
    args.iter()
        .position(|a| a == "--env")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let dot_env = PathBuf::from(".env");
            if dot_env.exists() {
                dot_env
            } else {
                PathBuf::from(".env.example")
            }
        })
}
