//! Material 3 Calendar: the app, the background daemon and the command
//! line in one binary. See `m3-calendar --help`.

mod cli;
mod config;
mod daemon;
mod dbus;
mod ical;
mod model;
mod nlp;
mod oauth;
mod ops;
mod providers;
mod recur;
mod secrets;
mod store;
mod sync;
mod ui;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::run(&args) {
        Ok(true) => {}
        Ok(false) => {
            let opts = match cli::app_args(&args) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("m3-calendar: {e:#}");
                    std::process::exit(2);
                }
            };
            if let Err(e) = ui::run(opts) {
                eprintln!("m3-calendar: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("m3-calendar: {e:#}");
            std::process::exit(1);
        }
    }
}
