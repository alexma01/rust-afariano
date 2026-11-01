//! Job logs: one append-only file per job, plus a broadcast channel per job
//! that is being watched, so the log page streams new bytes as they arrive.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use bytes::Bytes;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum LogEvent {
    Chunk {
        offset: u64,
        data: Bytes,
    },
    /// The job reached a terminal status.
    End,
}

pub struct LogHub {
    dir: PathBuf,
    channels: Mutex<HashMap<i64, broadcast::Sender<LogEvent>>>,
}

pub enum Append {
    /// New total size.
    Ok(u64),
    /// The offset did not match: the server holds this many bytes.
    Mismatch(u64),
}

impl LogHub {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, channels: Mutex::new(HashMap::new()) }
    }

    pub fn path(&self, job_id: i64) -> PathBuf {
        self.dir.join(format!("{}", job_id / 1000)).join(format!("{job_id}.log"))
    }

    pub async fn size(&self, job_id: i64) -> u64 {
        tokio::fs::metadata(self.path(job_id)).await.map(|m| m.len()).unwrap_or(0)
    }

    /// Append `data` written by the runner at `offset`. Overlapping resends
    /// keep only the new tail.
    pub async fn append(&self, job_id: i64, offset: u64, data: &[u8], max: u64) -> std::io::Result<Append> {
        let path = self.path(job_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let size = self.size(job_id).await;
        let end = offset + data.len() as u64;
        if size >= max {
            // Already cut: swallow the rest so the runner keeps going.
            return Ok(Append::Ok(end.max(size)));
        }
        if offset > size || end < size {
            return Ok(Append::Mismatch(size));
        }
        let skip = (size - offset) as usize;
        let mut new = &data[skip..];
        let room = (max - size) as usize;
        let cut = new.len() > room;
        if cut {
            new = &new[..room];
        }
        if new.is_empty() {
            return Ok(Append::Ok(size));
        }
        let mut f = OpenOptions::new().create(true).append(true).open(&path).await?;
        f.write_all(new).await?;
        let mut written = new.len() as u64;
        if cut {
            let note = b"\n\x1b[31;1mJob log exceeded the size limit and was cut.\x1b[0m\n";
            f.write_all(note).await?;
            written += note.len() as u64;
        }
        f.flush().await?;
        self.publish(job_id, LogEvent::Chunk { offset: size, data: Bytes::copy_from_slice(new) });
        Ok(Append::Ok(size + written))
    }

    /// Server-side note (e.g. "canceled", "runner lost") appended to the log.
    pub async fn note(&self, job_id: i64, text: &str) {
        let size = self.size(job_id).await;
        let line = format!("\n\x1b[33;1m{text}\x1b[0m\n");
        let _ = self.append(job_id, size, line.as_bytes(), u64::MAX).await;
    }

    pub async fn read(&self, job_id: i64, offset: u64, max: usize) -> std::io::Result<Vec<u8>> {
        let mut f = match tokio::fs::File::open(self.path(job_id)).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buf = Vec::new();
        f.take(max as u64).read_to_end(&mut buf).await?;
        Ok(buf)
    }

    pub fn subscribe(&self, job_id: i64) -> broadcast::Receiver<LogEvent> {
        let mut map = self.channels.lock().expect("log channels");
        map.retain(|_, tx| tx.receiver_count() > 0);
        map.entry(job_id).or_insert_with(|| broadcast::channel(256).0).subscribe()
    }

    fn publish(&self, job_id: i64, ev: LogEvent) {
        let map = self.channels.lock().expect("log channels");
        if let Some(tx) = map.get(&job_id) {
            let _ = tx.send(ev);
        }
    }

    pub fn end(&self, job_id: i64) {
        self.publish(job_id, LogEvent::End);
    }

    pub async fn delete(&self, job_id: i64) {
        let _ = tokio::fs::remove_file(self.path(job_id)).await;
    }
}
