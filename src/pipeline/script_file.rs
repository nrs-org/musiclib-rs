//! A file handle for match scripts, shared across the scoring threads.
//!
//! ```rhai
//! let f = file_create("out.csv");   // or file_append(path); relative to the CWD
//! f.write_line("a,b");              // safe from any thread, whole string under one lock
//! f.flush();                        // also flushed when the last copy is dropped
//! ```
//!
//! The script opens it once (in `init`) and keeps it in its context; the
//! host's per-call copies of the context copy only the `Arc`.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::sync::{Arc, Mutex};

use rhai::{Engine, EvalAltResult};

#[derive(Clone)]
pub struct ScriptFile {
    path: Arc<str>,
    inner: Arc<Mutex<BufWriter<File>>>,
}

type RhaiResult<T> = Result<T, Box<EvalAltResult>>;

impl ScriptFile {
    fn open(path: &str, append: bool) -> RhaiResult<Self> {
        let file = if append {
            OpenOptions::new().create(true).append(true).open(path)
        } else {
            File::create(path)
        };
        let file = file.map_err(|e| format!("opening {path}: {e}"))?;
        Ok(Self {
            path: path.into(),
            inner: Arc::new(Mutex::new(BufWriter::new(file))),
        })
    }

    fn write(&self, s: &str, newline: bool) -> RhaiResult<()> {
        let mut w = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let result = if newline {
            w.write_all(s.as_bytes()).and_then(|()| w.write_all(b"\n"))
        } else {
            w.write_all(s.as_bytes())
        };
        result.map_err(|e| format!("writing {}: {e}", self.path).into())
    }

    fn flush(&self) -> RhaiResult<()> {
        let mut w = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        w.flush()
            .map_err(|e| format!("flushing {}: {e}", self.path).into())
    }
}

pub fn register(engine: &mut Engine) {
    engine.register_type_with_name::<ScriptFile>("File");
    engine.register_fn("file_create", |path: &str| ScriptFile::open(path, false));
    engine.register_fn("file_append", |path: &str| ScriptFile::open(path, true));
    engine.register_fn("write", |f: &mut ScriptFile, s: &str| f.write(s, false));
    engine.register_fn("write_line", |f: &mut ScriptFile, s: &str| f.write(s, true));
    engine.register_fn("flush", |f: &mut ScriptFile| f.flush());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Many threads writing through copies of one handle: every line lands
    /// whole, and everything is on disk once the copies are gone.
    #[test]
    fn concurrent_lines_stay_whole() {
        let path = std::env::temp_dir().join(format!("script-file-{}.txt", std::process::id()));
        let path_s = path.display().to_string();
        let mut engine = Engine::new();
        register(&mut engine);
        let f: ScriptFile = engine.eval(&format!("file_create({path_s:?})")).unwrap();
        std::thread::scope(|scope| {
            for t in 0..8 {
                let f = f.clone();
                scope.spawn(move || {
                    for i in 0..500 {
                        f.write(&format!("thread {t} line {i} {}", "x".repeat(200)), true)
                            .unwrap();
                    }
                });
            }
        });
        drop(f);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 8 * 500);
        assert!(
            lines
                .iter()
                .all(|l| l.starts_with("thread ") && l.ends_with(&"x".repeat(200)))
        );
        std::fs::remove_file(path).ok();
    }
}
