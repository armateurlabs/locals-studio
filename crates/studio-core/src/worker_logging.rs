//! Python writes are stamped inside the worker, retaining its inherited append-only descriptors.
//! Native descriptor writes and historical lines stay untouched and may have no timestamp.
use serde_json::{Value, json};

pub(crate) const BOOTSTRAP: &str = concat!(
    r#"
import datetime, runpy, sys, threading
_studio_log_lock = threading.RLock()
_studio_log_start = True
class _StudioLogStream:
    def __init__(self, raw): self._raw = raw
    def __getattr__(self, name): return getattr(self._raw, name)
    def write(self, text):
        global _studio_log_start
        if not isinstance(text, str): raise TypeError('write() argument must be str')
        with _studio_log_lock:
            parts = text.split('\n')
            for index, part in enumerate(parts):
                piece = part + ('\n' if index < len(parts) - 1 else '')
                if not piece: continue
                prefix = ''
                if _studio_log_start:
                    stamp = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
                    prefix = '[' + stamp + '] '
                self._raw.write(prefix + piece)
                _studio_log_start = piece.endswith('\n')
        return len(text)
    def writelines(self, lines):
        for line in lines: self.write(line)
    def flush(self):
        with _studio_log_lock: self._raw.flush()
sys.stdout = _StudioLogStream(sys.stdout)
sys.stderr = _StudioLogStream(sys.stderr)
if sys.argv[1:3] != ['-m', 'locals.server']: raise RuntimeError('Invalid Studio worker launch arguments')
"#,
    include_str!("../../../resources/runtime/gemma4_mtp_compat.py"),
    r#"
_studio_prepare_gemma4_mtp_compat(sys.argv[3:])
sys.argv = sys.argv[2:]
runpy.run_module('locals.server', run_name='__main__', alter_sys=True)
"#
);

pub(crate) fn entries(text: &str, truncated: bool) -> Vec<Value> {
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            // A bounded tail may start anywhere, including midway through a prefix or UTF-8
            // character. Keep that fragment verbatim without inventing a line timestamp.
            let stamped = (!(truncated && index == 0))
                .then(|| line.strip_prefix('[')?.split_once("] "))
                .flatten()
                .filter(|(stamp, _)| chrono::DateTime::parse_from_rfc3339(stamp).is_ok());
            match stamped {
                Some((timestamp, line)) => json!({"line":line,"timestamp":timestamp}),
                None => json!({"line":line,"timestamp":null}),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    #[test]
    fn gemma4_mtp_compatibility_is_scoped_and_keeps_decoder_loading_strict() {
        let output = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/runtime/test_gemma4_mtp_compat.py"
            ))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn worker_installs_gemma_compatibility_before_the_model_loader_runs() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("locals");
        let models = directory.path().join("mlx_lm/models");
        let distribution = directory.path().join("mlx_lm-0.32.0.dist-info");
        let checkpoint = directory.path().join("checkpoint");
        for path in [&package, &models, &distribution, &checkpoint] {
            fs::create_dir_all(path).unwrap();
        }
        for path in [
            package.join("__init__.py"),
            models.parent().unwrap().join("__init__.py"),
            models.join("__init__.py"),
        ] {
            fs::write(path, "").unwrap();
        }
        fs::write(
            distribution.join("METADATA"),
            "Metadata-Version: 2.1\nName: mlx-lm\nVersion: 0.32.0\n",
        )
        .unwrap();
        fs::write(checkpoint.join("config.json"), r#"{"model_type":"gemma4"}"#).unwrap();
        for name in ["gemma4", "gemma4_text"] {
            fs::write(
                models.join(format!("{name}.py")),
                format!(
                    r#"
class Model:
    model_type = '{name}'
    def sanitize(self, weights):
        if set(weights) != {{'decoder.weight'}}: raise ValueError('Unexpected model weights')
        return weights
"#
                ),
            )
            .unwrap();
        }
        fs::write(package.join("server.py"), r#"
from mlx_lm.models.gemma4 import Model
weights = {'decoder.weight': object(), **{'language_model.mtp.head.' + str(i): object() for i in range(48)}}
assert set(Model().sanitize(weights)) == {'decoder.weight'}
try: Model().sanitize({'decoder.weight': object(), 'unexpected.weight': object()})
except ValueError: pass
else: raise AssertionError('Unrelated unexpected weights must still fail strict loading')
print('gemma_compat_before_loader=True')
"#).unwrap();
        let output = Command::new("python3")
            .args(["-u", "-c", BOOTSTRAP, "-m", "locals.server", "--model"])
            .arg(&checkpoint)
            .env("PYTHONPATH", directory.path())
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("gemma_compat_before_loader=True")
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("ignoring 48 auxiliary MTP tensors")
        );
    }

    #[test]
    fn legacy_lines_and_partial_tails_never_receive_invented_timestamps() {
        let text = "historical\n[2026-10-26T10:20:30.123Z] known\n\n[not a date] unchanged\n";
        let expected = json!([
            {"line":"historical","timestamp":null},
            {"line":"known","timestamp":"2026-10-26T10:20:30.123Z"},
            {"line":"","timestamp":null},
            {"line":"[not a date] unchanged","timestamp":null}
        ]);
        assert_eq!(json!(entries(text, false)), expected);
        assert_eq!(
            entries(text, false),
            entries(text, false),
            "polling must retain emission timestamps"
        );
        let text = "[2026-10-26T10:20:30.123Z] possible tail fragment\n[2026-10-26T10:20:31.456Z] complete\n";
        let tail = entries(text, true);
        assert!(tail[0]["timestamp"].is_null());
        assert_eq!(
            tail[0]["line"],
            "[2026-10-26T10:20:30.123Z] possible tail fragment"
        );
        assert_eq!(tail[1]["timestamp"], "2026-10-26T10:20:31.456Z");
        assert!(entries("", false).is_empty());
    }

    #[test]
    fn same_process_bootstrap_stamps_python_streams_and_preserves_argv_control_and_partial_writes()
    {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("locals");
        fs::create_dir(&package).unwrap();
        fs::write(package.join("__init__.py"), "").unwrap();
        fs::write(package.join("server.py"),r#"
import json, os, sys, threading
print('argv=' + json.dumps(sys.argv[1:]))
print('first\n\nthird')
sys.stderr.write('stderr\n')
assert sys.stdout.write('partial') == 7
sys.stdout.flush()
sys.stdout.write(' finish\n')
sys.stdout.write('\x1b[31mred\x1b[0m\rcontrol\n')
assert sys.stdout.fileno() == 1 and sys.stderr.fileno() == 2
assert sys.stdout.encoding
threads=[threading.Thread(target=lambda i=i: sys.stdout.write('thread-' + str(i) + '\n')) for i in range(8)]
for thread in threads: thread.start()
for thread in threads: thread.join()
os.write(1,b'native descriptor line\n')
"#).unwrap();
        let path = directory.path().join("worker.log");
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let before = chrono::Utc::now();
        let output = Command::new("python3")
            .args([
                "-u",
                "-c",
                BOOTSTRAP,
                "-m",
                "locals.server",
                "--host",
                "127.0.0.1",
                "--port",
                "1234",
            ])
            .env("PYTHONPATH", directory.path())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .status()
            .unwrap();
        assert!(output.success(), "{}", fs::read_to_string(&path).unwrap());
        let after = chrono::Utc::now();
        let text = fs::read_to_string(path).unwrap();
        let lines = entries(&text, false);
        assert_eq!(
            lines[0]["line"],
            "argv=[\"--host\", \"127.0.0.1\", \"--port\", \"1234\"]"
        );
        assert!(lines.iter().any(|line| line["line"] == "partial finish"));
        assert!(text.contains("\x1b[31mred\x1b[0m\rcontrol\n"));
        let native = lines
            .iter()
            .find(|line| line["line"] == "native descriptor line")
            .unwrap();
        assert!(native["timestamp"].is_null());
        let stamped: Vec<_> = lines
            .iter()
            .filter(|line| line["timestamp"].is_string())
            .collect();
        assert_eq!(stamped.len(), 15);
        for line in stamped {
            let time =
                chrono::DateTime::parse_from_rfc3339(line["timestamp"].as_str().unwrap()).unwrap();
            assert!(time.timestamp_millis() >= before.timestamp_millis() && time <= after);
        }
    }

    #[test]
    fn timestamped_worker_keeps_logging_after_its_launcher_exits() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("locals");
        fs::create_dir(&package).unwrap();
        fs::write(package.join("__init__.py"), "").unwrap();
        fs::write(package.join("server.py"),"import time\nprint('before launcher exit')\ntime.sleep(0.15)\nprint('after launcher exit')\n").unwrap();
        let log = directory.path().join("detached.log");
        let launcher = r#"import subprocess,sys
with open(sys.argv[1],'ab',buffering=0) as log:
 subprocess.Popen([sys.executable,'-u','-c',sys.argv[2],'-m','locals.server'],stdout=log,stderr=log,start_new_session=True)
"#;
        assert!(
            Command::new("python3")
                .args(["-c", launcher])
                .arg(&log)
                .arg(BOOTSTRAP)
                .env("PYTHONPATH", directory.path())
                .status()
                .unwrap()
                .success()
        );
        let started = Instant::now();
        let text = loop {
            let text = fs::read_to_string(&log).unwrap_or_default();
            if text.contains("after launcher exit") {
                break text;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "{text}");
            std::thread::sleep(Duration::from_millis(10));
        };
        let lines = entries(&text, false);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| line["timestamp"].is_string()));
    }
}
