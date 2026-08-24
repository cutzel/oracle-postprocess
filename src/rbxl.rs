use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::decompiler::{DecompilationRequest, Decompiler};

const HEADER_LEN: usize = 32;
const CHUNK_HEADER_LEN: usize = 16;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const BYTECODE_START_LF: &[u8] = b"-- Bytecode (Base64):\n-- ";
const BYTECODE_START_CRLF: &[u8] = b"-- Bytecode (Base64):\r\n-- ";

#[derive(Clone, Copy)]
struct ChunkHeader {
    name: [u8; 4],
    compressed_len: u32,
    uncompressed_len: u32,
    reserved: u32,
}

impl ChunkHeader {
    fn read<R: Read>(reader: &mut R) -> io::Result<Option<Self>> {
        let mut bytes = [0; CHUNK_HEADER_LEN];
        let read = reader.read(&mut bytes)?;
        if read == 0 {
            return Ok(None);
        }
        reader.read_exact(&mut bytes[read..])?;

        Ok(Some(Self {
            name: bytes[..4].try_into().unwrap(),
            compressed_len: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            uncompressed_len: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            reserved: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        }))
    }

    fn payload_len(self) -> usize {
        (if self.compressed_len == 0 {
            self.uncompressed_len
        } else {
            self.compressed_len
        }) as usize
    }

    fn write<W: Write>(self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&self.name)?;
        writer.write_all(&self.compressed_len.to_le_bytes())?;
        writer.write_all(&self.uncompressed_len.to_le_bytes())?;
        writer.write_all(&self.reserved.to_le_bytes())
    }
}

#[derive(Clone, Copy)]
enum Compression {
    None,
    Lz4,
    Zstd,
}

struct SourceValues<'a> {
    prefix: &'a [u8],
    values: Vec<&'a [u8]>,
}

struct PendingDecompilation {
    chunk_offset: u64,
    value_index: usize,
    header: Vec<u8>,
    bytecode: Arc<str>,
    rx: oneshot::Receiver<Result<String, String>>,
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn decode_chunk(header: ChunkHeader, body: &[u8]) -> io::Result<(Compression, Vec<u8>)> {
    if header.compressed_len == 0 {
        return Ok((Compression::None, body.to_vec()));
    }

    let size = header.uncompressed_len as usize;
    if body.starts_with(&ZSTD_MAGIC) {
        let data = zstd::bulk::decompress(body, size)
            .map_err(|error| invalid_data(format!("failed to decompress zstd chunk: {error}")))?;
        if data.len() != size {
            return Err(invalid_data(
                "zstd chunk decompressed to an unexpected length",
            ));
        }
        return Ok((Compression::Zstd, data));
    }

    let data = lz4_flex::block::decompress(body, size)
        .map_err(|error| invalid_data(format!("failed to decompress lz4 chunk: {error}")))?;
    Ok((Compression::Lz4, data))
}

fn encode_chunk(compression: Compression, data: &[u8]) -> io::Result<(u32, u32, Vec<u8>)> {
    let uncompressed_len =
        u32::try_from(data.len()).map_err(|_| invalid_data("chunk is too large to encode"))?;

    match compression {
        Compression::None => Ok((0, uncompressed_len, data.to_vec())),
        Compression::Lz4 => {
            let compressed = lz4_flex::block::compress(data);
            let compressed_len = u32::try_from(compressed.len())
                .map_err(|_| invalid_data("compressed chunk is too large to encode"))?;
            Ok((compressed_len, uncompressed_len, compressed))
        }
        Compression::Zstd => {
            let compressed = zstd::bulk::compress(data, 0)
                .map_err(|error| invalid_data(format!("failed to compress zstd chunk: {error}")))?;
            let compressed_len = u32::try_from(compressed.len())
                .map_err(|_| invalid_data("compressed chunk is too large to encode"))?;
            Ok((compressed_len, uncompressed_len, compressed))
        }
    }
}

fn source_values(data: &[u8]) -> io::Result<Option<SourceValues<'_>>> {
    if data.len() < 9 {
        return Ok(None);
    }

    let name_len = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    let name_end = 8usize
        .checked_add(name_len)
        .ok_or_else(|| invalid_data("property name length overflows"))?;
    let values_start = name_end
        .checked_add(1)
        .ok_or_else(|| invalid_data("property type offset overflows"))?;

    if values_start > data.len() || &data[8..name_end] != b"Source" || data[name_end] != 0x01 {
        return Ok(None);
    }

    let mut values = Vec::new();
    let mut position = values_start;
    while position < data.len() {
        let length_end = position
            .checked_add(4)
            .ok_or_else(|| invalid_data("source length offset overflows"))?;
        if length_end > data.len() {
            return Err(invalid_data("truncated source string length"));
        }
        let length = u32::from_le_bytes(data[position..length_end].try_into().unwrap()) as usize;
        position = length_end;
        let value_end = position
            .checked_add(length)
            .ok_or_else(|| invalid_data("source string length overflows"))?;
        if value_end > data.len() {
            return Err(invalid_data("truncated source string"));
        }
        values.push(&data[position..value_end]);
        position = value_end;
    }

    Ok(Some(SourceValues {
        prefix: &data[..values_start],
        values,
    }))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn bytecode_from_source(source: &[u8]) -> Option<(Vec<u8>, Arc<str>)> {
    let position = find_subslice(source, BYTECODE_START_LF)
        .map(|position| position + BYTECODE_START_LF.len())
        .or_else(|| {
            find_subslice(source, BYTECODE_START_CRLF)
                .map(|position| position + BYTECODE_START_CRLF.len())
        })?;

    let bytecode_end = source[position..]
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')
        .map(|offset| position + offset)
        .unwrap_or(source.len());
    let bytecode = std::str::from_utf8(&source[position..bytecode_end]).ok()?;

    Some((source[..position].to_vec(), Arc::from(bytecode)))
}

fn rebuild_source_chunk(
    values: SourceValues<'_>,
    replacements: &[Option<Vec<u8>>],
) -> io::Result<Vec<u8>> {
    if values.values.len() != replacements.len() {
        return Err(invalid_data(
            "source replacement count does not match source values",
        ));
    }

    let mut output = Vec::with_capacity(
        values.prefix.len()
            + values
                .values
                .iter()
                .enumerate()
                .map(|(index, value)| 4 + replacements[index].as_deref().unwrap_or(value).len())
                .sum::<usize>(),
    );
    output.extend_from_slice(values.prefix);

    for (index, value) in values.values.iter().enumerate() {
        let value = replacements[index].as_deref().unwrap_or(value);
        let length = u32::try_from(value.len())
            .map_err(|_| invalid_data("decompiled source is too large to encode"))?;
        output.extend_from_slice(&length.to_le_bytes());
        output.extend_from_slice(value);
    }

    Ok(output)
}

fn read_body<R: Read>(reader: &mut R, header: ChunkHeader) -> io::Result<Vec<u8>> {
    let mut body = vec![0; header.payload_len()];
    reader.read_exact(&mut body)?;
    Ok(body)
}

async fn scan_file(
    input_file: &str,
    decompiler: &Decompiler,
    bytes_read: &AtomicU64,
    total_scripts: &AtomicU32,
) -> Result<Vec<PendingDecompilation>, Box<dyn std::error::Error>> {
    let file = File::open(input_file)?;
    let mut reader = BufReader::with_capacity(8 * 1024 * 1024, file);
    let mut file_header = [0; HEADER_LEN];
    reader.read_exact(&mut file_header)?;
    if !file_header.starts_with(b"<roblox!") {
        return Err(invalid_data("input is not an rbxl binary file").into());
    }

    let mut pending = Vec::new();
    loop {
        let chunk_offset = reader.stream_position()?;
        let Some(header) = ChunkHeader::read(&mut reader)? else {
            break;
        };
        let body = read_body(&mut reader, header)?;
        bytes_read.store(reader.stream_position()?, Ordering::Relaxed);

        if header.name != *b"PROP" {
            continue;
        }

        let (_, data) = decode_chunk(header, &body)?;
        let Some(values) = source_values(&data)? else {
            continue;
        };

        for (value_index, source) in values.values.iter().enumerate() {
            let Some((source_header, bytecode)) = bytecode_from_source(source) else {
                continue;
            };

            total_scripts.fetch_add(1, Ordering::Relaxed);

            let (tx, rx) = oneshot::channel();
            let bytecode_hash = format!("{:x}", Sha256::digest(bytecode.as_bytes()));
            let bytecode_len = u32::try_from(bytecode.len())
                .map_err(|_| invalid_data("bytecode is too large to send"))?;
            decompiler
                .decompile_batch(vec![DecompilationRequest {
                    bytecode: bytecode.clone(),
                    bytecode_hash,
                    bytecode_len,
                    tx,
                }])
                .await?;

            pending.push(PendingDecompilation {
                chunk_offset,
                value_index,
                header: source_header,
                bytecode,
                rx,
            });
        }
    }

    Ok(pending)
}

async fn collect_results(
    pending: Vec<PendingDecompilation>,
    decompiled: &AtomicU32,
    failed: &AtomicU32,
) -> HashMap<(u64, usize), Vec<u8>> {
    let mut results = HashMap::with_capacity(pending.len());
    for job in pending {
        let result = match job.rx.await {
            Ok(Ok(source)) => {
                decompiled.fetch_add(1, Ordering::Relaxed);
                format!("-- decompilation:\n{source}")
            }
            Ok(Err(error)) => {
                failed.fetch_add(1, Ordering::Relaxed);
                format!("-- decompilation failed:\n-- {error}")
            }
            Err(_) => {
                failed.fetch_add(1, Ordering::Relaxed);
                "-- decompilation failed:\n-- oracle-postprocess error: sender dropped".to_string()
            }
        };

        let mut formatted = job.header;
        formatted.extend_from_slice(job.bytecode.as_bytes());
        formatted.extend_from_slice(b"\n\n");
        formatted.extend_from_slice(result.as_bytes());
        results.insert((job.chunk_offset, job.value_index), formatted);
    }
    results
}

fn write_file(
    input_file: &str,
    output_file: &str,
    replacements: &HashMap<(u64, usize), Vec<u8>>,
) -> Result<u32, Box<dyn std::error::Error>> {
    let input = File::open(input_file)?;
    let mut reader = BufReader::with_capacity(8 * 1024 * 1024, input);
    let output = File::create(output_file)?;
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, output);

    let mut file_header = [0; HEADER_LEN];
    reader.read_exact(&mut file_header)?;
    if !file_header.starts_with(b"<roblox!") {
        return Err(invalid_data("input is not an rbxl binary file").into());
    }
    writer.write_all(&file_header)?;

    let replacement_offsets: HashSet<u64> =
        replacements.keys().map(|(offset, _)| *offset).collect();
    let mut changed_chunks = 0;
    loop {
        let chunk_offset = reader.stream_position()?;
        let Some(mut header) = ChunkHeader::read(&mut reader)? else {
            break;
        };
        let body = read_body(&mut reader, header)?;

        if header.name != *b"PROP" || !replacement_offsets.contains(&chunk_offset) {
            header.write(&mut writer)?;
            writer.write_all(&body)?;
            continue;
        }

        let (compression, data) = decode_chunk(header, &body)?;
        let Some(values) = source_values(&data)? else {
            return Err(
                invalid_data("queued source chunk is no longer a Source string property").into(),
            );
        };
        let values_len = values.values.len();
        let mut changed = false;
        let mut chunk_replacements = Vec::with_capacity(values_len);
        for value_index in 0..values_len {
            let replacement = replacements.get(&(chunk_offset, value_index)).cloned();
            changed |= replacement.is_some();
            chunk_replacements.push(replacement);
        }

        if !changed {
            header.write(&mut writer)?;
            writer.write_all(&body)?;
            continue;
        }

        let data = rebuild_source_chunk(values, &chunk_replacements)?;
        let (compressed_len, uncompressed_len, body) = encode_chunk(compression, &data)?;
        header.compressed_len = compressed_len;
        header.uncompressed_len = uncompressed_len;
        header.write(&mut writer)?;
        writer.write_all(&body)?;
        changed_chunks += 1;
    }

    writer.flush()?;
    Ok(changed_chunks)
}

pub async fn process_rbxl_file(
    decompiler: &Decompiler,
    input_file: &str,
    output_file: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let file_size = std::fs::metadata(input_file)?.len();
    let bytes_read = Arc::new(AtomicU64::new(HEADER_LEN as u64));
    let total_scripts = Arc::new(AtomicU32::new(0));
    let decompiled = Arc::new(AtomicU32::new(0));
    let failed = Arc::new(AtomicU32::new(0));
    let scan_done = Arc::new(AtomicBool::new(false));
    let processing_done = Arc::new(AtomicBool::new(false));

    let progress_handle = {
        let bytes_read = bytes_read.clone();
        let total_scripts = total_scripts.clone();
        let decompiled = decompiled.clone();
        let failed = failed.clone();
        let scan_done = scan_done.clone();
        let processing_done = processing_done.clone();

        tokio::spawn(async move {
            let file_size_mib = file_size as f64 / (1024.0 * 1024.0);
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                if processing_done.load(Ordering::Relaxed) {
                    break;
                }

                let scripts = total_scripts.load(Ordering::Relaxed);
                let done = decompiled.load(Ordering::Relaxed);
                let failed = failed.load(Ordering::Relaxed);
                if !scan_done.load(Ordering::Relaxed) {
                    let read = bytes_read.load(Ordering::Relaxed);
                    let pct = (read as f64 / file_size as f64) * 100.0;
                    println!(
                        "reading: {:.1}% ({:.1}/{:.1} MiB) | {} scripts found, {} decompiled, {} failed",
                        pct,
                        read as f64 / (1024.0 * 1024.0),
                        file_size_mib,
                        scripts,
                        done,
                        failed
                    );
                } else if scripts > 0 {
                    println!(
                        "decompiling: {:.1}% ({}/{}) | {} failed",
                        (done + failed) as f64 / scripts as f64 * 100.0,
                        done + failed,
                        scripts,
                        failed
                    );
                }
            }
        })
    };

    let pending = scan_file(input_file, decompiler, &bytes_read, &total_scripts).await?;
    scan_done.store(true, Ordering::Relaxed);
    if pending.is_empty() {
        println!("no bytecode-bearing script sources found");
    } else {
        println!("{} script sources queued for decompilation", pending.len());
    }

    let replacements = collect_results(pending, &decompiled, &failed).await;
    let changed_chunks = write_file(input_file, output_file, &replacements)?;
    processing_done.store(true, Ordering::Relaxed);
    progress_handle.await?;
    let output_size = std::fs::metadata(output_file)?.len();
    println!(
        "wrote {} KiB to {} ({} script property chunks updated)",
        output_size / 1024,
        output_file,
        changed_chunks
    );

    Ok(())
}
