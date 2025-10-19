use clap::{Parser, Subcommand};
use std::{env, time::Instant};

mod compiled;
mod decompiler;
mod rbxlx;

use decompiler::Decompiler;
use rbxlx::process_rbxlx_file;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Oracle key
    /// You can also set it with the ORACLE_KEY env variable
    /// If both are provided, one from the argument is used
    #[arg(short, long, verbatim_doc_comment)]
    key: Option<String>,

    /// Oracle decompiler url
    #[arg(long, default_value = "wss://oracle.mshq.dev/v1/ws")]
    base_url: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Process a .rbxlx file
    Rbxlx {
        /// Input file path
        input: String,

        /// Output file path
        /// Defaults to out.rbxlx
        #[arg(short, long, verbatim_doc_comment, default_value = "processed.rbxlx")]
        output: String,
    },
    /// Process a single bytecode file
    Single {
        /// Input file path
        input: String,

        /// Output file path
        /// Defaults to out.rbxlx
        #[arg(short, long, verbatim_doc_comment, default_value = "decompiled.lua")]
        output: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let key = {
        let env = env::var("ORACLE_KEY").ok();
        let arg = args.key;
        match arg.or(env) {
            Some(key) => key,
            None => {
                return Err(format!(
                    "oracle key not provided. try `{} help`",
                    env::args().next().unwrap()
                )
                .into());
            }
        }
    };

    let decompiler = Decompiler::new(&args.base_url, &key).await?;

    let processing_start = Instant::now();

    match &args.command {
        Some(Commands::Rbxlx { input, output }) => {
            process_rbxlx_file(&decompiler, input, output).await?;
        }
        Some(Commands::Single { input, output }) => {
            let (bytecode, header) = compiled::get_bytecode_from_file(input)?;
            let mut result = decompiler.decompile_single(&bytecode).await??;

            if let Some(header) = header {
                result = format!("{}{}\n\n-- decompilation:\n{}", header, bytecode, result);
            }

            std::fs::write(output, result)?;
        }
        None => {
            println!("Try passing in --help")
        }
    }

    drop(decompiler);

    println!("time: {:?}", processing_start.elapsed());
    Ok(())
}
