mod args;
mod file_util;
mod manifest;
use args::{
    Args,
    Subcommands,
};
use clap::Parser as _;
use log::info;
use manifest::Manifest;
use simplelog::{
    ColorChoice,
    Config,
    LevelFilter,
    TermLogger,
    TerminalMode,
};

pub const VERSION: u16 = 1;

fn main() {
    color_eyre::install().expect("Failed to setup color_eyre");

    let args = Args::parse();

    let level = if args.verbose {
        LevelFilter::Info
    } else {
        LevelFilter::Warn
    };

    TermLogger::init(
        level,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .expect("Failed to initialize logger");

    info!("Program version: '{VERSION}'");
    match args.sub_command {
        Subcommands::Deactivate { manifest } => Manifest::read(&manifest).deactivate(),
        Subcommands::Activate { manifest, prefix } => Manifest::read(&manifest).activate(&prefix),
        Subcommands::Diff {
            prefix,
            manifest,
            old_manifest,
        } => Manifest::read(&manifest).diff(Manifest::read(&old_manifest), &prefix),
    }
}
