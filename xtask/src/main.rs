use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use xtask::{
    compose::{self, Options},
    fixture,
    native::{assert_native, SystemNativeGate},
    release::{package_with_gate, verify_archives, write_checksums},
};

#[derive(Parser)]
#[command(version, about = "Rust-only rproxy development and release tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Package {
        #[arg(long)]
        target: String,
        #[arg(long)]
        tag: String,
        #[arg(long, default_value = "dist")]
        directory: PathBuf,
    },
    Verify {
        #[arg(long)]
        tag: String,
        #[arg(long, default_value = "dist")]
        directory: PathBuf,
    },
    Checksums {
        #[arg(long, default_value = "dist")]
        directory: PathBuf,
    },
    AssertNative {
        #[arg(long)]
        target: String,
    },
    ComposeSmoke {
        #[arg(long, value_parser = ["docker", "podman"], default_value = "docker")]
        engine: String,
        #[arg(long)]
        skip_build: bool,
        #[arg(long, requires = "skip_build")]
        image_prefix: Option<String>,
    },
    Fixture,
}
fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    match Cli::parse().command {
        Command::Package {
            target,
            tag,
            directory,
        } => {
            assert_native(&target)?;
            println!(
                "{}",
                package_with_gate(root, &target, &tag, &directory, &mut SystemNativeGate)?
                    .display()
            );
        }
        Command::Verify { tag, directory } => {
            for path in verify_archives(&directory, &tag)? {
                println!("{}", path.display());
            }
        }
        Command::Checksums { directory } => println!("{}", write_checksums(&directory)?.display()),
        Command::AssertNative { target } => assert_native(&target)?,
        Command::ComposeSmoke {
            engine,
            skip_build,
            image_prefix,
        } => compose::run(
            root,
            Options {
                engine,
                skip_build,
                image_prefix,
            },
        )?,
        Command::Fixture => fixture::run()?,
    }
    Ok(())
}
