//! pacz: pacman wrapper with batched, mirror-aware parallel downloads.
//!
//! `pacz plan --rows FILE --conf FILE [--arch A]` is kept as a dry-run debugging aid
//! (no network, no root): it plans downloads from saved `pacman --print` output.
use pacz::{
    app,
    conf::parse_conf,
    config::Config,
    pacman::parse_cli,
    plan::{plan, PlanCfg},
    rows::parse_rows,
};
use std::{env, fs, process::ExitCode};

fn plan_cmd(args: &[String]) -> ExitCode {
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(rows), Some(conf)) = (get("--rows"), get("--conf")) else {
        eprintln!("usage: pacz plan --rows FILE --conf FILE [--arch ARCH]");
        return ExitCode::from(2);
    };
    let arch = get("--arch").unwrap_or_else(|| env::consts::ARCH.to_string());
    let (Ok(rows), Ok(conf)) = (fs::read_to_string(&rows), fs::read_to_string(&conf)) else {
        eprintln!("pacz: cannot read input files");
        return ExitCode::from(1);
    };
    let repos = parse_conf(&conf, &arch, &app::read_include, Config::default().mirrors);
    let p = plan(parse_rows(&rows), &PlanCfg::default());
    for l in app::describe_plan(&p, &repos) {
        println!("{l}");
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("plan") {
        return plan_cmd(&args);
    }
    match parse_cli(args) {
        Ok(cli) => ExitCode::from(app::main(cli)),
        Err(e) => {
            eprintln!("pacz: {e}");
            ExitCode::from(2)
        }
    }
}
