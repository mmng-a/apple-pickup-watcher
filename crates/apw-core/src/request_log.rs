//! 有容量上限的出站请求日志，不保存地址、cookie 或响应内容。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::apple::RequestRecord;

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub(crate) struct RequestLog {
    path: PathBuf,
    max_bytes: u64,
}

impl RequestLog {
    pub(crate) fn new(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_owned(),
            max_bytes: MAX_BYTES,
        })
    }

    pub(crate) fn append(&self, record: &RequestRecord) -> io::Result<()> {
        let mut line = serde_json::to_vec(&serde_json::json!({
            "atMs": record.at_ms,
            "kind": record.kind,
            "route": record.route,
            "status": record.status,
            "outcome": record.outcome,
            "durationMs": record.duration_ms,
        }))?;
        line.push(b'\n');
        if line.len() as u64 > self.max_bytes {
            return Err(io::Error::other("通信ログの1行が容量上限を超えています"));
        }
        if fs::metadata(&self.path)?.len() + line.len() as u64 > self.max_bytes {
            let previous = self.path.with_extension("previous.jsonl");
            match fs::remove_file(&previous) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            fs::rename(&self.path, previous)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(&line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_log_is_bounded_and_contains_only_diagnostic_fields() {
        let path =
            std::env::temp_dir().join(format!("apw-request-log-{}.jsonl", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut log = RequestLog::new(&path).unwrap();
        log.max_bytes = 300;
        let mut record = RequestRecord {
            at_ms: 1,
            kind: "pickup",
            transport: "rustls",
            route: "proxy#2".into(),
            locale: "ja_JP".into(),
            store: None,
            location: Some("private-location".into()),
            parts: 2,
            status: Some(200),
            http_version: None,
            duration_ms: 120,
            outcome: "ok".into(),
            cookies_sent: vec!["private-cookie".into()],
            cookies_after: vec![],
            final_url: Some("https://private-url/".into()),
        };
        for at in 1..=20 {
            record.at_ms = at;
            log.append(&record).unwrap();
        }
        for file in [&path, &path.with_extension("previous.jsonl")] {
            let data = fs::read_to_string(file).unwrap();
            assert!(data.len() as u64 <= log.max_bytes);
            assert!(!data.contains("private"));
            for line in data.lines() {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                assert_eq!(value.as_object().unwrap().len(), 6);
                assert_eq!(value["route"], "proxy#2");
            }
        }
        drop(log);
        let reopened = RequestLog::new(&path).unwrap();
        record.status = Some(541);
        record.outcome = "blocked".into();
        reopened.append(&record).unwrap();
        let data = fs::read_to_string(&path).unwrap();
        assert!(data.contains("\"outcome\":\"blocked\""));
        fs::remove_file(&path).unwrap();
        fs::remove_file(path.with_extension("previous.jsonl")).unwrap();
    }
}
