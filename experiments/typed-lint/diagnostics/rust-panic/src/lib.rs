use backtrace::Backtrace;
use serde::Serialize;
use std::{panic::PanicHookInfo, path::PathBuf};

#[derive(Serialize)]
pub struct Report {
  pub message: String,
  pub location: Option<Location>,
  pub traces: Vec<Trace>,
}

#[derive(Serialize)]
pub struct Location {
  pub file: PathBuf,
  pub line: Option<u32>,
  pub column: Option<u32>,
}

#[derive(Serialize)]
pub struct Trace {
  pub label: Option<String>,
  pub frames: Vec<Frame>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub function_name: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub address: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub location: Option<Location>,
}

/// Capture the panic site and symbols while the panic hook is running.
pub fn capture(info: &PanicHookInfo<'_>) -> Report {
  let payload = info.payload();
  let message = payload
    .downcast_ref::<&str>()
    .copied()
    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
    .unwrap_or("non-string panic payload")
    .to_owned();
  let mut frames = Vec::new();
  for frame in Backtrace::new().frames() {
    if frame.symbols().is_empty() {
      frames.push(Frame { function_name: None, address: Some(format!("{:p}", frame.ip())), location: None });
    }
    for symbol in frame.symbols() {
      frames.push(Frame {
        function_name: symbol.name().map(|name| format!("{name:#}")),
        address: None,
        location: symbol.filename().map(|file| Location { file: file.to_path_buf(), line: symbol.lineno(), column: symbol.colno() }),
      });
    }
  }
  Report {
    message,
    location: info.location().map(|site| Location { file: site.file().into(), line: Some(site.line()), column: Some(site.column()) }),
    traces: vec![Trace { label: std::thread::current().name().map(str::to_owned), frames }],
  }
}
