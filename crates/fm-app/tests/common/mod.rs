#![allow(dead_code)]
use fm_ai::AiClient;
use fm_app::App;
use fm_config::Config;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn config(tmp: &Path, socket: &Path) -> Config {
    let mut c = Config::default();
    c.database.path = tmp.join("index.db").to_string_lossy().into_owned();
    c.ai.socket_path = socket.to_string_lossy().into_owned();
    c.ai.request_timeout_s = 30;
    c.core.threads = 2;
    c
}

pub fn app(tmp: &Path, socket: &Path) -> App {
    App::new(config(tmp, socket)).unwrap()
}

pub fn dead_socket(tmp: &Path) -> PathBuf {
    tmp.join("no-engine.sock")
}

/// The real Python engine (mock backend) as a child process, killed on drop.
pub struct Engine {
    child: Child,
    pub socket: PathBuf,
}

impl Engine {
    pub fn start(dir: &Path) -> Engine {
        let socket = dir.join("ai.sock");
        let ai_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ai-engine");
        let child = Command::new("python3")
            .args(["-m", "aiengine", "--mock", "--socket"])
            .arg(&socket)
            .env("PYTHONPATH", &ai_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("python3 must be available to run the integration tests");
        let client = AiClient::new(&socket, Duration::from_secs(5));
        let end = Instant::now() + Duration::from_secs(15);
        while client.ping().is_err() {
            assert!(Instant::now() < end, "the AI engine did not come up");
            std::thread::sleep(Duration::from_millis(50));
        }
        Engine { child, socket }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.kill();
    }
}

pub fn write(root: &Path, rel: &str, body: &[u8]) -> PathBuf {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, body).unwrap();
    p
}

pub fn set_mtime(p: &Path, secs_since_epoch: u64) {
    File::options().write(true).open(p).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(secs_since_epoch)).unwrap();
}

pub fn now_plus(secs: u64) -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + secs
}

pub fn png(tag: &str) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(tag.as_bytes());
    v
}
