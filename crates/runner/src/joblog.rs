//! Job log: masking, size cap, and streaming to the server (or stdout).

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rust_afariano_proto::{MAX_LOG_CHUNK, MAX_LOG_SIZE};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::client::{Client, LogPut};

const MASK: &[u8] = b"[MASKED]";
/// Shorter masked values are not masked (they would garble the log).
pub const MIN_MASK_LEN: usize = 4;
const FLUSH_BYTES: usize = 64 * 1024;
const HEARTBEAT: Duration = Duration::from_secs(5);

/// Replaces secret values with `[MASKED]` in a byte stream, holding back a
/// suffix that might be the start of a secret split across chunks.
#[derive(Debug, Default)]
pub struct Masker {
    /// Longest first.
    secrets: Vec<Vec<u8>>,
    pending: Vec<u8>,
}

impl Masker {
    pub fn new<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut secrets: Vec<Vec<u8>> = Vec::new();
        for v in values {
            let v = v.as_ref();
            if v.len() >= MIN_MASK_LEN {
                secrets.push(v.as_bytes().to_vec());
            }
            // A multi-line secret printed line by line (e.g. `cat key.pem`) is
            // still masked line by line.
            if v.contains('\n') {
                for line in v.lines() {
                    let line = line.trim_end_matches('\r');
                    if line.len() >= 8 {
                        secrets.push(line.as_bytes().to_vec());
                    }
                }
            }
        }
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets, pending: Vec::new() }
    }

    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        if self.secrets.is_empty() {
            return data.to_vec();
        }
        self.pending.extend_from_slice(data);
        self.process(false)
    }

    /// No more input: emit everything held back.
    pub fn finish(&mut self) -> Vec<u8> {
        self.process(true)
    }

    fn process(&mut self, last: bool) -> Vec<u8> {
        let buf = std::mem::take(&mut self.pending);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            let rest = &buf[i..];
            let mut matched = None;
            let mut partial = false;
            for s in &self.secrets {
                if rest.len() >= s.len() {
                    if rest.starts_with(s) {
                        matched = Some(s.len());
                        break;
                    }
                } else if !last && s.starts_with(rest) {
                    // A longer secret may still complete with the next chunk.
                    partial = true;
                }
            }
            if partial {
                self.pending = rest.to_vec();
                return out;
            }
            match matched {
                Some(len) => {
                    out.extend_from_slice(MASK);
                    i += len;
                }
                None => {
                    out.push(buf[i]);
                    i += 1;
                }
            }
        }
        out
    }
}

struct State {
    masker: Masker,
    /// Masked bytes not yet handed to the uploader.
    buf: Vec<u8>,
    emitted: u64,
    truncated: bool,
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    wake: Notify,
}

/// Cheap to clone; every clone writes to the same log.
#[derive(Clone)]
pub struct JobLog {
    inner: Arc<Inner>,
}

impl JobLog {
    pub fn new(masker: Masker) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State { masker, buf: Vec::new(), emitted: 0, truncated: false, closed: false }),
                wake: Notify::new(),
            }),
        }
    }

    pub fn write(&self, data: &[u8]) {
        let mut st = self.inner.state.lock().unwrap();
        if st.closed {
            return;
        }
        let masked = st.masker.push(data);
        Self::append(&mut st, &masked);
        if st.buf.len() >= FLUSH_BYTES {
            self.inner.wake.notify_one();
        }
    }

    fn append(st: &mut State, data: &[u8]) {
        if st.truncated || data.is_empty() {
            return;
        }
        let room = MAX_LOG_SIZE.saturating_sub(st.emitted) as usize;
        if data.len() <= room {
            st.buf.extend_from_slice(data);
            st.emitted += data.len() as u64;
        } else {
            st.buf.extend_from_slice(&data[..room]);
            st.emitted += room as u64;
            st.truncated = true;
            st.buf.extend_from_slice(
                format!(
                    "\n\x1b[31;1mJob log exceeded {} MiB, the rest is not shown.\x1b[0;m\n",
                    MAX_LOG_SIZE / 1024 / 1024
                )
                .as_bytes(),
            );
        }
    }

    pub fn line(&self, s: &str) {
        self.write(format!("{s}\n").as_bytes());
    }

    /// Runner message, bold cyan like GitLab's section headers.
    pub fn step(&self, s: &str) {
        self.write(format!("\x1b[36;1m{s}\x1b[0;m\n").as_bytes());
    }

    pub fn warn(&self, s: &str) {
        self.write(format!("\x1b[0;33mWARNING: {s}\x1b[0;m\n").as_bytes());
    }

    pub fn error(&self, s: &str) {
        self.write(format!("\x1b[31;1mERROR: {s}\x1b[0;m\n").as_bytes());
    }

    pub fn success(&self, s: &str) {
        self.write(format!("\x1b[32;1m{s}\x1b[0;m\n").as_bytes());
    }

    /// Flush the masker and mark the end; the uploader drains and stops.
    pub fn close(&self) {
        let mut st = self.inner.state.lock().unwrap();
        if st.closed {
            return;
        }
        let rest = st.masker.finish();
        Self::append(&mut st, &rest);
        st.closed = true;
        drop(st);
        self.inner.wake.notify_one();
    }

    fn take(&self) -> (Vec<u8>, bool) {
        let mut st = self.inner.state.lock().unwrap();
        (std::mem::take(&mut st.buf), st.closed)
    }
}

pub enum Sink {
    Server { client: Client, job_id: i64, token: String },
    Stdout,
}

/// Streams the log until it is closed and drained. `cancel` is triggered when
/// the server asks to cancel the job (or no longer knows it).
pub fn spawn_uploader(log: JobLog, sink: Sink, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        match sink {
            Sink::Stdout => loop {
                let woke = log.inner.wake.notified();
                let (data, closed) = log.take();
                if !data.is_empty() {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(&data);
                    let _ = out.flush();
                }
                if closed {
                    break;
                }
                tokio::select! {
                    _ = woke => {}
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
            },
            Sink::Server { client, job_id, token } => upload(log, client, job_id, token, cancel).await,
        }
    })
}

/// Server protocol: `offset` = bytes the server already holds. The whole
/// stream is kept (≤ MAX_LOG_SIZE) so a 409 with a smaller size can be
/// resent from that point; a larger size drops that many bytes of ours.
async fn upload(log: JobLog, client: Client, job_id: i64, token: String, cancel: CancellationToken) {
    let mut stream: Vec<u8> = Vec::new();
    // Index into `stream` of the first byte the server has not acknowledged.
    let mut pos: usize = 0;
    // What the server says it holds; the next PUT's offset.
    let mut server_size: u64 = 0;
    let mut last_contact = Instant::now() - HEARTBEAT;
    let mut failing_since: Option<Instant> = None;
    let mut backoff = Duration::from_secs(1);
    let mut conflicts = 0u32;
    loop {
        let (data, closed) = log.take();
        stream.extend_from_slice(&data);

        let pending = stream.len() - pos;
        let due = pending > 0 || last_contact.elapsed() >= HEARTBEAT || closed;
        if due {
            let n = pending.min(MAX_LOG_CHUNK);
            let chunk = stream[pos..pos + n].to_vec();
            match client.put_log(job_id, &token, server_size, chunk).await {
                Ok(LogPut::Ok(ack)) => {
                    pos += n;
                    server_size = ack.size;
                    last_contact = Instant::now();
                    failing_since = None;
                    backoff = Duration::from_secs(1);
                    conflicts = 0;
                    if ack.cancel {
                        cancel.cancel();
                    }
                }
                Ok(LogPut::Conflict(ack)) => {
                    // The server's size is authoritative.
                    server_size = ack.size;
                    pos = (ack.size as usize).min(stream.len());
                    last_contact = Instant::now();
                    if ack.cancel {
                        cancel.cancel();
                    }
                    conflicts += 1;
                    if conflicts > 5 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
                Ok(LogPut::Gone) => {
                    tracing::warn!(job_id, "server no longer knows this job; stopping it");
                    cancel.cancel();
                    // Keep draining so the log never grows unbounded, but stop sending.
                    loop {
                        let (_, closed) = log.take();
                        if closed {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
                Err(e) => {
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    tracing::warn!(job_id, "log upload failed: {e:#}");
                    if closed && since.elapsed() > Duration::from_secs(120) {
                        tracing::error!(job_id, "giving up on the log upload");
                        return;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(15));
                    continue;
                }
            }
        }
        if pos < stream.len() {
            continue;
        }
        if closed {
            return;
        }
        tokio::select! {
            _ = log.inner.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(m: &mut Masker, chunks: &[&str]) -> String {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(m.push(c.as_bytes()));
        }
        out.extend(m.finish());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn masks_whole_secret() {
        let mut m = Masker::new(["s3cr3tvalue"]);
        assert_eq!(run(&mut m, &["token=s3cr3tvalue done"]), "token=[MASKED] done");
    }

    #[test]
    fn masks_secret_split_across_chunks() {
        let mut m = Masker::new(["s3cr3tvalue"]);
        assert_eq!(run(&mut m, &["token=s3c", "r3t", "value done"]), "token=[MASKED] done");
        let mut m = Masker::new(["abcdef"]);
        assert_eq!(run(&mut m, &["xa", "b", "c", "d", "e", "fy"]), "x[MASKED]y");
    }

    #[test]
    fn holds_back_only_possible_prefixes() {
        let mut m = Masker::new(["abcdef"]);
        // "abc" could become the secret: held back.
        assert_eq!(String::from_utf8(m.push(b"hello abc")).unwrap(), "hello ");
        // It did not: released.
        assert_eq!(String::from_utf8(m.push(b"xyz")).unwrap(), "abcxyz");
        assert_eq!(m.finish(), b"");
    }

    #[test]
    fn prefix_at_end_flushed_on_finish() {
        let mut m = Masker::new(["abcdef"]);
        assert_eq!(run(&mut m, &["end abc"]), "end abc");
    }

    #[test]
    fn longer_secret_wins_over_shorter_prefix() {
        let mut m = Masker::new(["abcd", "abcdefgh"]);
        assert_eq!(run(&mut m, &["x abcd", "efgh y"]), "x [MASKED] y");
        let mut m = Masker::new(["abcd", "abcdefgh"]);
        assert_eq!(run(&mut m, &["x abcd", "zz"]), "x [MASKED]zz");
    }

    #[test]
    fn multiple_occurrences_and_secrets() {
        let mut m = Masker::new(["password1", "tokenXYZ"]);
        assert_eq!(run(&mut m, &["password1 tokenXYZ password1"]), "[MASKED] [MASKED] [MASKED]");
    }

    #[test]
    fn short_values_not_masked() {
        let mut m = Masker::new(["abc"]);
        assert_eq!(run(&mut m, &["abc abc"]), "abc abc");
    }

    #[test]
    fn multiline_secret_masked_per_line() {
        let key = "-----BEGIN KEY-----\nAAAABBBBCCCCDDDD\n-----END KEY-----";
        let mut m = Masker::new([key]);
        assert_eq!(run(&mut m, &[key]), "[MASKED]");
        let mut m = Masker::new([key]);
        assert_eq!(run(&mut m, &["line: AAAABBBB", "CCCCDDDD\n"]), "line: [MASKED]\n");
    }

    #[test]
    fn log_cap_truncates() {
        let log = JobLog::new(Masker::default());
        let big = vec![b'x'; (MAX_LOG_SIZE as usize) + 10];
        log.write(&big);
        log.write(b"more");
        let (data, _) = log.take();
        assert!(data.len() > MAX_LOG_SIZE as usize);
        assert!(data.len() < MAX_LOG_SIZE as usize + 200);
        assert!(String::from_utf8_lossy(&data[MAX_LOG_SIZE as usize..]).contains("exceeded"));
    }
}
