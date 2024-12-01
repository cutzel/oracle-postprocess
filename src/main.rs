use clap::Parser;
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde_json::json;
use std::fs::File;
use std::io::Write;
use std::time::SystemTime;
use std::{env, fs, io, process};
use xmltree::{Element, XMLNode};

/// A rbxlx postprocessor that decompiles everything inside 😋
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Input file path
    input_file: String,

    /// Output file path
    /// Defaults to out.rbxlx
    #[arg(short, long, verbatim_doc_comment, default_value = "out.rbxlx")]
    output: String,

    /// Oracle key
    /// You can also set it with the ORACLE_KEY env variable
    /// If both are provided, one from the argument is used
    #[arg(short, long, verbatim_doc_comment)]
    key: Option<String>,

    /// Oracle decompiler url
    #[arg(long, default_value = "https://oracle.mshq.dev/decompile")]
    base_url: String,
}

fn walk_count(el: &Element, counter: &mut u64) {
    if el
        .attributes
        .get("class")
        .is_some_and(|class| class == "ModuleScript" || class == "LocalScript" || class == "Script")
    {
        *counter += 1;
    }

    for child in el.children.iter() {
        let Some(el) = child.as_element() else {
            continue;
        };
        walk_count(el, counter);
    }
}

fn walk(el: &mut Element, total: &u64, counter: &mut u64, oracle_url: &String, key: &String) {
    if el.attributes.get("class").is_some_and(|class| {
        class == "ModuleScript" || class == "LocalScript"
    } || class == "Script")
    {
        *counter += 1;
        let props = el
            .children
            .iter_mut()
            .find(|it| it.as_element().is_some_and(|it| it.name == "Properties"))
            .and_then(|it| it.as_mut_element());

        let script_name = props.as_ref()
            .and_then(|it| {
                    it.children.iter().find(|it| {
                        it.as_element().is_some_and(|it| {
                            it.attributes.get("name").is_some_and(|it| it == "Name")
                        })
                    })
            })
            .and_then(|it| { it.as_element().and_then(|it| it.children.first().and_then(|it| it.as_cdata())) });

        print!(
            "[{}/{}] Decompiling {}... ",
            counter,
            total,
            script_name.unwrap_or("unknown")
        );
        let _ = io::stdout().flush();

        let src_node = props
            .and_then(|it| {
                    it.children.iter_mut().find(|it| {
                        it.as_element().is_some_and(|it| {
                            it.attributes.get("name").is_some_and(|it| it == "Source")
                        })
                    })
            })
            .and_then(|it| it.as_mut_element());

        if let Some(n) = src_node {
            if let Some(source) = n.children.first().and_then(|it| it.as_cdata()) {
                let re = Regex::new(r"-- Bytecode \(Base64\):\n-- (.*)\n\n").unwrap();
                let b64_bytecode = re
                    .captures(source)
                    .and_then(|it| it.get(1).map(|it| it.as_str()));

                let watermark = source.lines().take(6).collect::<Vec<_>>().join("\n");

                if let Some(bytecode) = b64_bytecode {
                    let start = SystemTime::now();
                    
                    match Client::new()
                        .post(oracle_url)
                        .header("Authorization", format!("Bearer {}", key))
                        .body(
                            serde_json::to_string(&json!({
                                "script": bytecode
                            }))
                            .unwrap(),
                        )
                        .send()
                    {
                        Ok(dec) => {
                            match dec.status() {
                                StatusCode::OK => {
                                    if let Ok(deserialized) = dec.text() {
                                        n.children[0] = XMLNode::CData(vec![watermark, deserialized].join("\n"));
                                    }

                                    let elapsed = start.elapsed()
                                        .expect("Time went backwards");
                                    println!("decompiled in {}ms!", elapsed.as_millis());
                                }
                                StatusCode::PAYMENT_REQUIRED
                                | StatusCode::TOO_MANY_REQUESTS
                                | StatusCode::UNAUTHORIZED => {
                                    println!("{}", dec.text().ok().unwrap_or("unlucky".into()))
                                }
                                StatusCode::INTERNAL_SERVER_ERROR => {
                                    println!("Internal server error")
                                }
                                StatusCode::BAD_REQUEST => {
                                    println!("Update the app please please please please")
                                }
                                code => println!("something went wrong: {code}"),
                            }
                        }
                        Err(e) => {
                            println!("error: {e:?}");
                        }
                    }
                } else {
                    println!("no bytecode!");
                }
            } else {
                println!("malformed rbxlx");
            }
        } else {
            println!("malformed rbxlx");
        }
    }

    for child in el.children.iter_mut() {
        let Some(el) = child.as_mut_element() else {
            continue;
        };
        walk(el, total, counter, oracle_url, key);
    }
}

fn main() {
    let args = Args::parse();

    let env_key = env::var("ORACLE_KEY").ok();
    let arg_key = args.key;

    let key = arg_key.or(env_key).unwrap_or_else(|| {
        eprintln!("Oracle key not provided");
        process::exit(1);
    });

    let Ok(contents) = fs::read(args.input_file) else {
        eprintln!("Can't read the file");
        process::exit(1);
    };

    let Ok(mut rbx) = Element::parse(contents.as_slice()) else {
        eprintln!("Can't parse the file");
        process::exit(1);
    };

    print!("Counting scripts... ");
    let _ = io::stdout().flush();

    let mut total = 0u64;
    walk_count(&rbx, &mut total);
    println!("{}", total);

    let start = SystemTime::now();

    let mut decompiled = 0u64;
    walk(&mut rbx, &total, &mut decompiled, &args.base_url, &key);
    
    let elapsed = start.elapsed()
        .expect("Time went backwards");
    println!("Processed in {}s!", elapsed.as_secs());

    print!("Writing output to {}... ", args.output);
    let _ = io::stdout().flush();

    let file = File::create(args.output).unwrap();

    match rbx.write(file) {
        Ok(_) => {println!("Done!");},
        Err(e) => {println!("Can't write the file: {:?}", e);}
    };
}
