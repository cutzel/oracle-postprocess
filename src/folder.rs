use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::compiled::get_bytecode_from_file;
use crate::decompiler::{DecompilationRequest, Decompiler};

struct FileJob {
    input_path: PathBuf,
    output_path: PathBuf,
    bytecode: Arc<str>,
    header: Option<String>,
    rx: oneshot::Receiver<Result<String, String>>,
}

fn collect_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

pub async fn process_folder(
    decompiler: &Decompiler,
    input_dir: &str,
    output_dir: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let input_path = Path::new(input_dir).canonicalize()?;
    let output_path = Path::new(output_dir);

    let all_files = collect_files(&input_path);
    println!("found {} files in {}", all_files.len(), input_dir);

    let mut jobs: Vec<FileJob> = Vec::new();
    let mut skipped = 0u32;

    for file in &all_files {
        let file_str = file.to_string_lossy();
        let Ok((bytecode, header)) = get_bytecode_from_file(&file_str) else {
            skipped += 1;
            continue;
        };

        let rel = file.strip_prefix(&input_path)?;
        let mut out = output_path.join(rel);
        out.set_extension("lua");

        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let (tx, rx) = oneshot::channel();
        let bytecode_arc: Arc<str> = Arc::from(bytecode.as_str());
        let bytecode_hash = format!("{:x}", Sha256::digest(bytecode.as_bytes()));
        let bytecode_len = bytecode.len() as u32;

        let request = DecompilationRequest {
            bytecode: bytecode_arc.clone(),
            bytecode_hash,
            bytecode_len,
            tx,
        };

        decompiler.decompile_batch(vec![request]).await?;

        jobs.push(FileJob {
            input_path: file.clone(),
            output_path: out,
            bytecode: bytecode_arc,
            header,
            rx,
        });
    }

    let total = jobs.len() as u32;
    println!(
        "{} bytecode files queued for decompilation, {} skipped (not bytecode)",
        total, skipped
    );

    if total == 0 {
        println!("nothing to decompile");
        return Ok(());
    }

    let decompiled = Arc::new(AtomicU32::new(0));
    let failed = Arc::new(AtomicU32::new(0));

    let decompiled_progress = decompiled.clone();
    let failed_progress = failed.clone();
    let progress_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            let done = decompiled_progress.load(Ordering::Relaxed);
            let fail = failed_progress.load(Ordering::Relaxed);
            let pct = (done as f64 / total as f64) * 100.0;
            println!(
                "decompiling: {:.1}% ({}/{}) | {} failed",
                pct, done, total, fail
            );
            if done + fail >= total {
                break;
            }
        }
    });

    for job in jobs {
        let result = match job.rx.await {
            Ok(Ok(source)) => {
                decompiled.fetch_add(1, Ordering::Relaxed);
                match job.header {
                    Some(header) => {
                        format!("{}{}\n\n-- decompilation:\n{}", header, job.bytecode, source)
                    }
                    None => source,
                }
            }
            Ok(Err(err)) => {
                failed.fetch_add(1, Ordering::Relaxed);
                eprintln!("failed: {} — {}", job.input_path.display(), err);
                match job.header {
                    Some(header) => format!(
                        "{}{}\n\n-- decompilation failed:\n-- {}",
                        header, job.bytecode, err
                    ),
                    None => format!("-- decompilation failed:\n-- {}", err),
                }
            }
            Err(_) => {
                failed.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "failed: {} — sender dropped",
                    job.input_path.display()
                );
                format!("-- decompilation failed:\n-- sender dropped")
            }
        };

        std::fs::write(&job.output_path, result)?;
    }

    progress_handle.await?;

    let ok = decompiled.load(Ordering::Relaxed);
    let fail = failed.load(Ordering::Relaxed);
    println!("done. {} decompiled, {} failed", ok, fail);

    Ok(())
}
