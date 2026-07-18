use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use std::{
    collections::VecDeque,
    io::Read,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

const MAX_LINES: usize = 2000;

pub struct TerminalSession {
    output: Arc<Mutex<VecDeque<String>>>,
    partial: Arc<Mutex<String>>,
    handler: Option<JoinHandle<()>>,
    writer: Option<Box<dyn std::io::Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
}

impl TerminalSession {
    pub fn new() -> std::io::Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        let pair = pty_system.openpty(size).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
        let cmd = CommandBuilder::new(shell);
        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;

        let mut reader: Box<dyn Read + Send> = pair.master.try_clone_reader().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        let writer: Box<dyn std::io::Write + Send> =
            pair.master.take_writer().map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
            })?;

        let output = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LINES)));
        let partial = Arc::new(Mutex::new(String::new()));
        let output_clone = output.clone();
        let partial_clone = partial.clone();

        let handler = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut line_buf: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for &b in &buf[..n] {
                            match b {
                                b'\n' => {
                                    let s = String::from_utf8_lossy(&line_buf).into_owned();
                                    line_buf.clear();
                                    let mut out = output_clone.lock().unwrap();
                                    out.push_back(s);
                                    while out.len() > MAX_LINES {
                                        out.pop_front();
                                    }
                                }
                                b'\r' => {
                                    if !line_buf.is_empty() {
                                        let s =
                                            String::from_utf8_lossy(&line_buf).into_owned();
                                        line_buf.clear();
                                        let mut out = output_clone.lock().unwrap();
                                        out.push_back(s);
                                        while out.len() > MAX_LINES {
                                            out.pop_front();
                                        }
                                    }
                                }
                                0x08 | 0x7f => {
                                    line_buf.pop();
                                }
                                _ => {
                                    line_buf.push(b);
                                }
                            }
                        }
                        // Update partial for rendering incomplete lines (prompts)
                        let s = String::from_utf8_lossy(&line_buf).into_owned();
                        let mut p = partial_clone.lock().unwrap();
                        *p = s;
                    }
                    Err(_) => break,
                }
            }
            // Flush remaining
            if !line_buf.is_empty() {
                let s = String::from_utf8_lossy(&line_buf).into_owned();
                let mut out = output_clone.lock().unwrap();
                out.push_back(s);
            }
        });

        Ok(Self {
            output,
            partial,
            handler: Some(handler),
            writer: Some(writer),
            child: Some(child),
            master: Some(pair.master),
        })
    }

    pub fn write(&mut self, data: &[u8]) {
        if let Some(ref mut w) = self.writer {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    pub fn tail(&self, n: usize, offset: usize) -> Vec<String> {
        let output = self.output.lock().unwrap();
        let partial = self.partial.lock().unwrap();
        let has_partial = !partial.is_empty();
        let total = output.len() + if has_partial { 1 } else { 0 };

        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(n);

        let mut result = Vec::with_capacity(n);
        let completed = output.len();

        for i in start..end {
            if i < completed {
                result.push(output[i].clone());
            } else {
                result.push(partial.clone());
            }
        }

        result
    }

    pub fn total_lines(&self) -> usize {
        let output = self.output.lock().unwrap();
        let partial = self.partial.lock().unwrap();
        output.len() + if partial.is_empty() { 0 } else { 1 }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if let Some(ref m) = self.master {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handler) = self.handler.take() {
            let _ = handler.join();
        }
    }
}
