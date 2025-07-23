use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use quick_xml::events::{BytesCData, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use tokio::sync::{mpsc, oneshot};

use crate::decompiler::{DecompilationRequest, Decompiler};

enum ToWrite<'a> {
    XmlEvent(Event<'a>),
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
    let reader_done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<ToWrite>();
    let decompiled_count_clone = decompiled_count.clone();
    let output_file = output_file.to_string();
    let writer_handle = tokio::spawn(async move {
        let file = File::create(&output_file).expect("failed to create output file");
        let mut buf_writer = BufWriter::with_capacity(8 * 1024 * 1024, file);
        let mut writer = Writer::new(&mut buf_writer);

        while let Some(task) = write_rx.recv().await {
            match task {
                ToWrite::XmlEvent(e) => {
                    writer.write_event(e).unwrap();
                }
                ToWrite::DecompilationResult {
                    header,
                    bytecode,
                    rx,
                } => {
                    let result = rx.await.unwrap();
                    let result = match result {
                        Ok(it) => format!("-- decompilation:\n{}", it),
                        Err(it) => format!("-- decompilation failed:\n-- {}", it),
                    };
                    let formatted_result = format!("{}{}\n\n{}\n", header, bytecode, result);
                    let event = Event::CData(BytesCData::new(formatted_result));

                    decompiled_count_clone.fetch_add(1, Ordering::Relaxed);
                    writer.write_event(event).unwrap();
                }
            }
        }

        use std::io::Write;
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
    let reader_done_clone = reader_done.clone();
    let progress_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            let decompiled = decompiled_count_clone.load(Ordering::Relaxed);
            let total = total_scripts_clone_progress.load(Ordering::Relaxed);
            let is_reader_done = reader_done_clone.load(Ordering::Relaxed);

            if total > 0 {
                println!(
                    "decompilation progress: {}/{} ({:.1}%)",
                    decompiled,
                    total,
                    (decompiled as f64 / total as f64) * 100.0
                );
            }

            if is_reader_done && decompiled >= total && total > 0 {
                break;
            }
        }
    });

    let input_file_handle = File::open(input_file)?;
    let mut reader =
        Reader::from_reader(BufReader::with_capacity(8 * 1024 * 1024, input_file_handle));
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::CData(bob)) => {
                let (dec_tx, dec_rx) = oneshot::channel::<Result<String, String>>();
                let cdata_string = String::from_utf8(bob.to_vec()).unwrap();

                let bytecode_start = "-- Bytecode (Base64):\n-- ";
                let bytecode_position = cdata_string
                    .find(bytecode_start)
                    .map(|it| it + bytecode_start.len());

                let Some(position) = bytecode_position else {
                    write_tx
                        .send(ToWrite::XmlEvent(Event::CData(bob.into_owned())))
                        .unwrap();
                    continue;
                };

                total_scripts.fetch_add(1, Ordering::Relaxed);

                let bytecode_end = cdata_string[position..]
                    .find('\n')
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
                write_tx.send(ToWrite::XmlEvent(e.into_owned())).unwrap();
            }
            Err(e) => {
                eprintln!("error at position {}: {:?}", reader.error_position(), e);
                return Err(e.into());
            }
        }
        buf.clear();
    }

    // and now we wait for the decompiler
    // to do its thing
    reader_done.store(true, Ordering::Relaxed);
    progress_handle.await?;
    // and now the decompiler has done its thing
    drop(write_tx);
    writer_handle.await?;

    Ok(())
}
