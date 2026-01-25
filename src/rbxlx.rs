use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use tokio::sync::{mpsc, oneshot};
use xml::reader::{EventReader, XmlEvent};
use xml::writer::{EmitterConfig, XmlEvent as WriteXmlEvent};

use crate::decompiler::{DecompilationRequest, Decompiler};

enum ToWrite {
    XmlEvent(XmlEvent),
    DecompilationResult {
        header: String,
        bytecode: Arc<str>,
        rx: oneshot::Receiver<Result<String, String>>,
    },
}

pub async fn process_rbxlx_file(
    decompiler: &Decompiler,
    input_file: &str,
    output_file: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let total_scripts = Arc::new(AtomicU32::new(0));
    let decompiled_count = Arc::new(AtomicU32::new(0));
    let total_events = Arc::new(AtomicU32::new(0));
    let written_events = Arc::new(AtomicU32::new(0));
    let reader_done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<ToWrite>();
    let decompiled_count_clone = decompiled_count.clone();
    let written_events_clone = written_events.clone();
    let output_file = output_file.to_string();
    let writer_handle = tokio::spawn(async move {
        let file = File::create(&output_file).expect("failed to create output file");
        let mut buf_writer = BufWriter::with_capacity(8 * 1024 * 1024, file);
        let mut writer = EmitterConfig::new()
            .create_writer(&mut buf_writer);

        while let Some(task) = write_rx.recv().await {
            match task {
                ToWrite::XmlEvent(e) => {
                    match e {
                        XmlEvent::StartElement { name, attributes, .. } => {
                            use xml::name::Name;
                            let local_name_str = name.local_name.as_str();
                            let elem_name = Name::local(local_name_str);
                            let builder = WriteXmlEvent::start_element(elem_name);
                            if attributes.is_empty() {
                                if let Err(e) = writer.write(builder) {
                                    panic!("Write error: {e}");
                                }
                            } else {
                                let final_builder = attributes.iter().fold(builder, |b, attr| {
                                    let attr_name = Name::local(attr.name.local_name.as_str());
                                    b.attr(attr_name, &attr.value)
                                });
                                if let Err(e) = writer.write(final_builder) {
                                    panic!("Write error: {e}");
                                }
                            }
                        }
                        XmlEvent::EndElement { name: _ } => {
                            if let Err(e) = writer.write(WriteXmlEvent::end_element()) {
                                panic!("Write error: {e}");
                            }
                        }
                        XmlEvent::CData(text) => {
                            let text_owned = text.clone();
                            if let Err(e) = writer.write(WriteXmlEvent::cdata(&text_owned)) {
                                panic!("Write error: {e}");
                            }
                        }
                        XmlEvent::Characters(text) => {
                            let text_owned = text.clone();
                            if let Err(e) = writer.write(WriteXmlEvent::characters(&text_owned)) {
                                panic!("Write error: {e}");
                            }
                        }
                        XmlEvent::Comment(text) => {
                            let text_owned = text.clone();
                            if let Err(e) = writer.write(WriteXmlEvent::comment(&text_owned)) {
                                panic!("Write error: {e}");
                            }
                        }
                        XmlEvent::ProcessingInstruction { .. } | XmlEvent::StartDocument { .. } => {
                            written_events_clone.fetch_add(1, Ordering::Relaxed);
                            continue
                        }
                        _ => {
                            written_events_clone.fetch_add(1, Ordering::Relaxed);
                            continue
                        }
                    }
                }
                ToWrite::DecompilationResult {
                    header,
                    bytecode,
                    rx,
                } => {
                    let result = match rx.await {
                        Ok(it) => it,
                        Err(_) => {
                            eprintln!("error: decompilation response never received (sender dropped)");
                            Err("oracle-postprocess error: sender dropped".to_string())
                        }
                    };
                    let result = match result {
                        Ok(it) => format!("-- decompilation:\n{}", it),
                        Err(it) => format!("-- decompilation failed:\n-- {}", it),
                    };
                    let formatted_result = format!("{}{}\n\n{}\n", header, bytecode, result);
                    let escaped_result = formatted_result.replace("]]>", "]]]]><![CDATA[>");
                    let event = WriteXmlEvent::cdata(&escaped_result);

                    decompiled_count_clone.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = writer.write(event) {
                        panic!("Write error: {e}");
                    }
                }
            }
            written_events_clone.fetch_add(1, Ordering::Relaxed);
        }

        if let Err(e) = buf_writer.flush() {
            println!("couldnt flush buffer: {:?}", e);
        }

        if let Ok(metadata) = std::fs::metadata(&output_file) {
            println!("wrote {} KiB to {}", metadata.len() / 1024, output_file);
        } else {
            println!("wrote output file to {}", output_file);
        }
    });

    let decompiled_count_clone = decompiled_count.clone();
    let total_scripts_clone_progress = total_scripts.clone();
    let total_events_clone = total_events.clone();
    let written_events_clone = written_events.clone();
    let reader_done_clone = reader_done.clone();
    let progress_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            let decompiled = decompiled_count_clone.load(Ordering::Relaxed);
            let total = total_scripts_clone_progress.load(Ordering::Relaxed);
            let total_ev = total_events_clone.load(Ordering::Relaxed);
            let written_ev = written_events_clone.load(Ordering::Relaxed);
            let is_reader_done = reader_done_clone.load(Ordering::Relaxed);

            if total_ev > 0 {
                let write_pct = (written_ev as f64 / total_ev as f64) * 100.0;
                if total > 0 {
                    let decompile_pct = (decompiled as f64 / total as f64) * 100.0;
                    println!(
                        "xml: {}/{} ({:.1}%) | decompiled: {}/{} ({:.1}%)",
                        written_ev, total_ev, write_pct, decompiled, total, decompile_pct
                    );
                } else {
                    println!("xml: {}/{} ({:.1}%)", written_ev, total_ev, write_pct);
                }
            }

            if is_reader_done && written_ev >= total_ev && total_ev > 0 {
                break;
            }
        }
    });

    let input_file_handle = File::open(input_file)?;
    let file = BufReader::with_capacity(8 * 1024 * 1024, input_file_handle);
    let parser = EventReader::new(file);

    for e in parser {
        match e {
            Ok(XmlEvent::CData(cdata_string)) => {
                total_events.fetch_add(1, Ordering::Relaxed);
                let (dec_tx, dec_rx) = oneshot::channel::<Result<String, String>>();

                let bytecode_start_lf = "-- Bytecode (Base64):\n-- ";
                let bytecode_start_crlf = "-- Bytecode (Base64):\r\n-- ";
                
                let bytecode_position = cdata_string
                    .find(bytecode_start_lf)
                    .map(|it| it + bytecode_start_lf.len())
                    .or_else(|| cdata_string
                        .find(bytecode_start_crlf)
                        .map(|it| it + bytecode_start_crlf.len()));

                let Some(position) = bytecode_position else {
                    write_tx
                        .send(ToWrite::XmlEvent(XmlEvent::CData(cdata_string)))
                        .unwrap();
                    continue;
                };

                total_scripts.fetch_add(1, Ordering::Relaxed);

                let bytecode_end = cdata_string[position..]
                    .find(|c| c == '\n' || c == '\r')
                    .map(|idx| position + idx)
                    .unwrap_or(cdata_string.len());

                let header = cdata_string[..position].to_string();
                let bytecode = &cdata_string[position..bytecode_end];

                let bytecode_hash = format!("{:x}", Sha256::digest(bytecode.as_bytes()));
                let bytecode_len = bytecode.len() as u32;

                let bytecode: Arc<str> = Arc::from(bytecode);

                let request = DecompilationRequest {
                    bytecode: bytecode.clone(),
                    bytecode_hash,
                    bytecode_len,
                    tx: dec_tx,
                };

                decompiler.decompile_batch(vec![request]).await.unwrap();
                write_tx
                    .send(ToWrite::DecompilationResult {
                        header,
                        bytecode,
                        rx: dec_rx,
                    })
                    .unwrap();
            }
            Ok(e) => {
                total_events.fetch_add(1, Ordering::Relaxed);
                write_tx.send(ToWrite::XmlEvent(e)).unwrap();
            }
            Err(e) => {
                eprintln!("Error: {e}");
                return Err(e.into());
            }
        }
    }

    // and now we wait for the decompiler
    // to do its thing
    reader_done.store(true, Ordering::Relaxed);
    progress_handle.await?;
    // and now the decompiler has done its thing
    drop(write_tx);
    writer_handle.await?;

    if total_scripts.load(Ordering::Relaxed) == 0 {
        println!("no scripts found to decompile");
    }

    Ok(())
}
