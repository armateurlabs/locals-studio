//! End-to-end local control/gateway tests. The workers use Python's standard library only.
use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use studio_core::{
    models::ModelRecord,
    paths::AppPaths,
    service::Studio,
    store::Store,
    types::{ServerProfile, StudioConfig},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const FAKE_WORKER: &str = r#"
import argparse,json,os,signal,time
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from socketserver import TCPServer
if os.environ.get('FAKE_FAIL_REVERSE_DNS') == '1':
 import socket
 def reject_reverse_dns(*args): raise AssertionError('Fake workers must not perform reverse DNS')
 socket.getfqdn=reject_reverse_dns
class LocalHTTPServer(ThreadingHTTPServer):
 def server_bind(self):
  # HTTPServer.server_bind performs reverse DNS even for our numeric loopback host.
  TCPServer.server_bind(self)
  self.server_name='localhost';self.server_port=self.server_address[1]
p=argparse.ArgumentParser()
p.add_argument('--host');p.add_argument('--port',type=int);p.add_argument('--model')
args,_=p.parse_known_args()
alias=os.environ['FAKE_ALIAS']
canonical=os.environ['FAKE_CANONICAL']
model_ids=json.loads(os.environ.get('FAKE_MODEL_IDS','null')) or [canonical]
log_path=os.environ['FAKE_REQUEST_LOG']
def record(event,**fields):
 with open(log_path,'a') as out:
  out.write(json.dumps(dict(event=event,worker=alias,bind_host=args.host,**fields))+'\n')
class Handler(BaseHTTPRequestHandler):
 protocol_version='HTTP/1.1'
 def log_message(self,*args): pass
 def reply(self,data):
  body=json.dumps(data).encode()
  self.send_response(200);self.send_header('Content-Type','application/json')
  self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self):
  record('probe',path=self.path,model_path=args.model)
  if self.path=='/health': self.reply({'status':'ok','version':'0.2.1'})
  elif self.path=='/v1/models': self.reply({'data':[{'id':model} for model in model_ids]})
  elif self.path=='/stats': self.reply({'model':args.model,'active':[],'memory':{'gpu_active_mb':1,'gpu_cache_mb':2,'gpu_peak_mb':3,'rss_mb':4}})
  else: self.reply({})
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))))
  origin=self.headers.get('Origin')
  origins=os.environ.get('LOCALS_ORIGINS','http://localhost')
  record('post',model=body.get('model'),chat_template_kwargs=body.get('chat_template_kwargs'),origin=origin,path=self.path,host=self.headers.get('Host'),locals_origins=origins)
  allowed=[value.strip() for value in origins.split(',')]
  if origin and '*' not in allowed and origin not in allowed:
   self.send_response(403);self.send_header('Content-Length','0');self.end_headers();return
  if not body.get('stream'):
   self.reply({'worker':alias,'received_model':body.get('model'),'received_kwargs':body.get('chat_template_kwargs'),'choices':[{'message':{'role':'assistant','content':alias},'finish_reason':'stop'}]})
   return
  self.send_response(200);self.send_header('Content-Type','text/event-stream')
  self.send_header('Cache-Control','no-cache');self.send_header('Connection','close');self.end_headers()
  self.close_connection=True
  try:
   for index in range(100):
    content=alias+':'+str(index)+' '
    frame={'choices':[{'index':0,'delta':{'content':content},'finish_reason':None}]}
    self.wfile.write(('data: '+json.dumps(frame)+'\n\n').encode());self.wfile.flush()
    time.sleep(0.03)
   self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
  except (BrokenPipeError,ConnectionResetError): record('disconnected')
ignore_sigterm=os.environ.get('FAKE_IGNORE_SIGTERM') == '1'
if ignore_sigterm: signal.signal(signal.SIGTERM,signal.SIG_IGN)
record('booting',pid=os.getpid(),ignores_sigterm=ignore_sigterm)
time.sleep(float(os.environ.get('FAKE_STARTUP_DELAY','0')))
server=LocalHTTPServer((args.host,args.port),Handler)
record('started')
server.serve_forever()
"#;

// Clean up test workers even if an assertion panics. PID identity is rechecked before signalling.
struct OwnedProcess {
    pid: u32,
    start_time: u64,
    command: Vec<std::ffi::OsString>,
}

impl OwnedProcess {
    fn capture(pid: u32) -> Self {
        Self::try_capture(pid).expect("fake worker is running")
    }

    fn try_capture(pid: u32) -> Option<Self> {
        let system = worker_process(pid);
        let process = system.process(Pid::from_u32(pid))?;
        Some(Self {
            pid,
            start_time: process.start_time(),
            command: process.cmd().to_vec(),
        })
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let system = worker_process(self.pid);
        if system
            .process(Pid::from_u32(self.pid))
            .is_some_and(|process| {
                process.start_time() == self.start_time && process.cmd() == self.command.as_slice()
            })
        {
            // The fixture is finished or unwinding. Kill only its exact recorded process,
            // including fake workers deliberately configured to ignore SIGTERM.
            unsafe {
                libc::kill(self.pid as i32, libc::SIGKILL);
            }
        }
    }
}

// The fixture only needs one worker's birth time and argv, not a full CPU/memory
// inventory of every process on a shared CI runner.
fn worker_process(pid: u32) -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .without_tasks(),
    );
    system
}

struct Fixture {
    studio: Arc<Studio>,
    client: Client,
    base_url: String,
    request_log: PathBuf,
    processes: Vec<OwnedProcess>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let port = unused_port().await;
        Self::with_port(port).await
    }

    async fn with_port(port: u16) -> Self {
        Self::with_policy(port, false).await
    }

    async fn with_policy(port: u16, open: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let paths = AppPaths {
            data_dir: root.join("data"),
            database_path: root.join("data/store.sqlite3"),
            socket_path: root.join("data/studio.sock"),
            models_dir: root.join("models"),
            locals_source: root.join("source"),
        };
        paths.prepare().unwrap();
        let python = tokio::process::Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .await
            .expect("python3 is required for fake-worker tests");
        assert!(python.status.success());
        let python = String::from_utf8(python.stdout).unwrap().trim().to_owned();
        fs::create_dir_all(paths.data_dir.join("runtime")).unwrap();
        fs::write(
            paths.data_dir.join("runtime/current.json"),
            serde_json::to_vec(&json!({
                "python_path":python,"python_version":"3.12","locals_version":"0.2.1",
                "installed_at":"test","source_path":root
            }))
            .unwrap(),
        )
        .unwrap();
        let package = root.join("fake/locals");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("__init__.py"), "").unwrap();
        fs::write(package.join("server.py"), FAKE_WORKER).unwrap();
        let store = Store::open(&paths.database_path).unwrap();
        let mut config = StudioConfig::default();
        config.api.port = port;
        if !open {
            config.api.allowed_hosts = vec!["localhost".into(), "127.0.0.1".into()];
            config.api.allowed_origins = vec!["https://client.example".into()];
        }
        store.set("settings", "config", &config).unwrap();
        let studio = Studio::new(paths).await.unwrap();
        Self {
            studio,
            client: Client::builder().no_proxy().build().unwrap(),
            base_url: format!("http://127.0.0.1:{port}"),
            request_log: root.join("requests.jsonl"),
            processes: Vec::new(),
            _directory: directory,
        }
    }

    async fn start_workers(&mut self) {
        for (id, alias, canonical) in [
            ("first", "alpha", "locals/canonical-first"),
            ("second", "alpha-thinking", "locals/canonical-second"),
        ] {
            self.start_worker(id, alias, canonical, &[canonical]).await;
        }
    }

    async fn start_worker(&mut self, id: &str, alias: &str, canonical: &str, model_ids: &[&str]) {
        let model_path = self._directory.path().join(format!("model-{id}"));
        fs::create_dir_all(&model_path).unwrap();
        let model = ModelRecord {
            id: format!("model-{id}"),
            name: alias.into(),
            source: "import".into(),
            repo_id: None,
            revision: None,
            path: model_path,
            weights_bytes: 1024,
            compatible: true,
            architecture: Some("test".into()),
            quantization: None,
            context_length: Some(1024),
        };
        self.studio.store.set("models", &model.id, &model).unwrap();
        let mut profile = ServerProfile {
            id: id.into(),
            alias: alias.into(),
            model_id: model.id.clone(),
            selected: true,
            ..Default::default()
        };
        profile.options =
            serde_json::from_value(json!({"max_context":1024,"cache_gb":0.25,"slots":1})).unwrap();
        profile.env.insert(
            "PYTHONPATH".into(),
            self._directory
                .path()
                .join("fake")
                .to_string_lossy()
                .into_owned(),
        );
        // Every integration start must succeed even if reverse DNS is unavailable.
        profile
            .env
            .insert("FAKE_FAIL_REVERSE_DNS".into(), "1".into());
        profile.env.insert("FAKE_ALIAS".into(), alias.into());
        profile
            .env
            .insert("FAKE_CANONICAL".into(), canonical.into());
        profile.env.insert(
            "FAKE_MODEL_IDS".into(),
            serde_json::to_string(model_ids).unwrap(),
        );
        profile.env.insert(
            "FAKE_REQUEST_LOG".into(),
            self.request_log.to_string_lossy().into_owned(),
        );
        self.studio
            .handle("profiles.save", json!({"profile":profile}))
            .await
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            self.studio.handle(
                "server.start",
                json!({"id":id,"allow_memory_overcommit":true}),
            ),
        )
        .await;
        let result = match result {
            Ok(Ok(result)) => result,
            failure => {
                let statuses = self.studio.supervisor.list().await;
                // A timed-out start can leave an owned loading worker behind. Register
                // it before panicking only if its current identity is still the one
                // persisted at launch; a recycled PID never authorizes cleanup.
                if let Some(process) = statuses
                    .iter()
                    .find(|status| status.profile_id == id)
                    .and_then(|status| status.pid)
                    .and_then(OwnedProcess::try_capture)
                    .filter(|process| {
                        self.studio.store.get::<Value>("workers", id).ok().flatten()
                            .is_some_and(|record| record["identity"] == json!({
                                "pid": process.pid,
                                "start_time": process.start_time,
                                "command": process.command.iter().map(|value| value.to_string_lossy()).collect::<Vec<_>>(),
                            }))
                    })
                {
                    self.processes.push(process);
                }
                let logs = self.studio.supervisor.logs(id).await;
                let mut probes = Vec::new();
                if let Some(port) = statuses
                    .iter()
                    .find(|status| status.profile_id == id)
                    .and_then(|status| status.port)
                {
                    for path in ["/health", "/stats", "/v1/models"] {
                        let response = async {
                            let response = self
                                .client
                                .get(format!("http://127.0.0.1:{port}{path}"))
                                .timeout(Duration::from_secs(1))
                                .send()
                                .await?;
                            Ok::<_, reqwest::Error>((response.status(), response.text().await?))
                        }
                        .await;
                        probes.push(format!("{path}: {response:?}"));
                    }
                }
                panic!(
                    "fake worker {id} did not start within the existing 10-second bound: {failure:?}\nservers: {statuses:?}\nworker records: {:?}\nworker logs: {logs:?}\nreadiness probes: {probes:?}",
                    read_records(&self.request_log),
                );
            }
        };
        let pid = result["servers"][0]["pid"].as_u64().unwrap() as u32;
        self.processes.push(OwnedProcess::capture(pid));
    }

    async fn post(&self, model: &str) -> Value {
        let response = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&json!({"model":model,"messages":[{"role":"user","content":"test"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    fn posts(&self) -> Vec<Value> {
        read_records(&self.request_log)
            .into_iter()
            .filter(|row| row["event"] == "post")
            .collect()
    }

    async fn close(self) {
        self.studio.shutdown().await.unwrap();
    }
}

fn read_records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

async fn unused_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_no_leases(studio: &Studio) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if studio
                .supervisor
                .list()
                .await
                .iter()
                .all(|server| server.active_requests == 0)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("gateway must release the worker when the client disconnects");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn common_port_routes_two_workers_and_preserves_exact_thinking_aliases() -> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture
        .start_worker(
            "first",
            "alpha",
            "locals/canonical-first",
            &["locals/canonical-first", "locals/canonical-first-thinking"],
        )
        .await;
    fixture
        .start_worker(
            "second",
            "alpha-thinking",
            "locals/canonical-second",
            &["locals/canonical-second"],
        )
        .await;
    let servers = fixture.studio.supervisor.list().await;
    assert_eq!(servers.len(), 2);
    assert_ne!(servers[0].port, servers[1].port);
    let public_port = fixture.studio.config.read().await.api.port;
    assert!(
        servers
            .iter()
            .all(|server| server.port != Some(public_port))
    );
    let models: Value = fixture
        .client
        .get(format!("{}/v1/models", fixture.base_url))
        .send()
        .await?
        .json()
        .await?;
    let aliases: Vec<_> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect();
    assert_eq!(aliases, ["alpha", "alpha-thinking"]);
    assert!(!aliases.contains(&"alpha-thinking-thinking"));

    let first = fixture.post("alpha").await;
    assert_eq!(first["worker"], "alpha");
    assert_eq!(first["received_model"], "locals/canonical-first");
    let exact = fixture.post("alpha-thinking").await;
    assert_eq!(exact["worker"], "alpha-thinking");
    assert_eq!(exact["received_model"], "locals/canonical-second");
    let decorated = fixture.post("mlx/alpha-thinking-thinking").await;
    assert_eq!(decorated["worker"], "alpha-thinking");
    assert_eq!(decorated["received_model"], "mlx/canonical-second-thinking");
    assert!(
        fixture
            .posts()
            .iter()
            .all(|row| row["bind_host"] == "127.0.0.1")
    );
    wait_no_leases(&fixture.studio).await;
    fixture.studio.supervisor.stop("second").await?;
    // A stopped exact alias remains reserved; it must not turn into another worker's variant.
    let models: Value = fixture
        .client
        .get(format!("{}/models", fixture.base_url))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "alpha");
    assert_eq!(
        fixture
            .client
            .get(format!("{}/models/alpha-thinking", fixture.base_url))
            .send()
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advertised_thinking_variants_and_explicit_engines_work_without_a_profile_default()
-> Result<()> {
    let mut fixture = Fixture::new().await;
    // The canonical model's own name also ends in -thinking; it must not be stripped.
    fixture
        .start_worker(
            "thinking",
            "beta",
            "locals/real-thinking",
            &[
                "locals/real-thinking",
                "locals/real-thinking-thinking",
                "mlx/real-thinking",
                "mlx/real-thinking-thinking",
            ],
        )
        .await;
    let profile = fixture
        .studio
        .store
        .get::<ServerProfile>("profiles", "thinking")?
        .unwrap();
    assert!(!profile.options.contains_key("thinking"));
    for prefix in ["/v1/models", "/models"] {
        let models: Value = fixture
            .client
            .get(format!("{}{prefix}", fixture.base_url))
            .send()
            .await?
            .json()
            .await?;
        let ids: Vec<_> = models["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["beta", "beta-thinking", "mlx/beta", "mlx/beta-thinking"]
        );
        for id in &ids {
            let response = fixture
                .client
                .get(format!("{}{prefix}/{id}", fixture.base_url))
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.json::<Value>().await?["id"], *id);
        }
        assert_eq!(
            fixture
                .client
                .get(format!(
                    "{}{prefix}/beta-thinking-thinking",
                    fixture.base_url
                ))
                .send()
                .await?
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    for (requested, canonical) in [
        ("beta", "locals/real-thinking"),
        ("beta-thinking", "locals/real-thinking-thinking"),
        ("mlx/beta", "mlx/real-thinking"),
        ("mlx/beta-thinking", "mlx/real-thinking-thinking"),
    ] {
        assert_eq!(fixture.post(requested).await["received_model"], canonical);
    }
    for (enabled, model) in [(true, "beta"), (false, "mlx/beta-thinking")] {
        let kwargs = json!({"enable_thinking":enabled,"custom":"preserved"});
        let response: Value = fixture.client.post(format!("{}/v1/chat/completions", fixture.base_url))
            .json(&json!({"model":model,"messages":[{"role":"user","content":"test"}],"chat_template_kwargs":kwargs}))
            .send().await?.error_for_status()?.json().await?;
        assert_eq!(response["received_kwargs"], kwargs);
        assert_eq!(
            fixture.posts().last().unwrap()["chat_template_kwargs"],
            kwargs
        );
    }
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_is_progressive_and_client_disconnect_releases_its_lease() -> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture.start_workers().await;
    let mut events = fixture.studio.events.subscribe();
    let started = Instant::now();
    let mut response = fixture
        .client
        .post(format!("{}/v1/chat/completions", fixture.base_url))
        .json(
            &json!({"model":"alpha","messages":[{"role":"user","content":"stream"}],"stream":true}),
        )
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let first = tokio::time::timeout(Duration::from_secs(2), response.chunk())
        .await??
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(String::from_utf8_lossy(&first).contains("data:"));
    assert!(!String::from_utf8_lossy(&first).contains("[DONE]"));
    let telemetry = fixture.studio.supervisor.telemetry().await?;
    let activity = &telemetry["workers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|worker| worker["profile_id"] == "first")
        .unwrap()["activity"];
    assert_eq!(activity["phase"], "generating");
    assert_eq!(activity["active_requests"], 1);
    assert!(activity["tokens_per_second"].is_null());
    assert_eq!(
        fixture
            .studio
            .supervisor
            .list()
            .await
            .iter()
            .find(|s| s.alias == "alpha")
            .unwrap()
            .active_requests,
        1
    );
    drop(response);
    wait_no_leases(&fixture.studio).await;
    let activity_events: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event["type"] == "server.activity")
        .collect();
    assert!(
        activity_events
            .iter()
            .any(|event| event["data"]["activity"]["phase"] == "generating")
    );
    assert_eq!(
        activity_events.last().unwrap()["data"]["activity"]["phase"],
        "idle"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !read_records(&fixture.request_log)
            .iter()
            .any(|row| row["event"] == "disconnected")
        {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("dropping the downstream body should disconnect the upstream worker");
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_policy_rejects_requests_before_workers_and_handles_preflight_locally() -> Result<()>
{
    let mut fixture = Fixture::new().await;
    fixture.start_workers().await;
    let url = format!("{}/v1/chat/completions", fixture.base_url);
    let request = json!({"model":"alpha","messages":[{"role":"user","content":"test"}]});
    let before = fixture.posts().len();
    let wrong_host = fixture
        .client
        .post(&url)
        .header("Host", "attacker.example:8321")
        .json(&request)
        .send()
        .await?;
    assert_eq!(wrong_host.status(), StatusCode::FORBIDDEN);
    let wrong_origin = fixture
        .client
        .post(&url)
        .header("Origin", "https://attacker.example")
        .json(&request)
        .send()
        .await?;
    assert_eq!(wrong_origin.status(), StatusCode::FORBIDDEN);
    let opaque_origin = fixture
        .client
        .post(&url)
        .header("Origin", "null")
        .json(&request)
        .send()
        .await?;
    assert_eq!(opaque_origin.status(), StatusCode::FORBIDDEN);
    let preflight = fixture
        .client
        .request(reqwest::Method::OPTIONS, &url)
        .header("Origin", "https://client.example")
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "content-type, authorization",
        )
        .send()
        .await?;
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        preflight.headers()["access-control-allow-origin"],
        "https://client.example"
    );
    assert_eq!(fixture.posts().len(), before);

    let allowed = fixture
        .client
        .post(&url)
        .header("Origin", "https://client.example")
        .json(&request)
        .send()
        .await?;
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed.headers()["access-control-allow-origin"],
        "https://client.example"
    );
    let _: Value = allowed.json().await?;
    assert_eq!(fixture.posts().len(), before + 1);
    assert_eq!(
        fixture.posts().last().unwrap()["origin"],
        "https://client.example"
    );
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_policies_allow_arbitrary_host_origin_and_stream_to_wildcard_workers() -> Result<()> {
    let mut fixture = Fixture::with_policy(unused_port().await, true).await;
    fixture.start_workers().await;
    let origin = "https://arbitrary-client.example";
    let host = "arbitrary-api.example:9443";
    let models = fixture
        .client
        .get(format!("{}/v1/models", fixture.base_url))
        .header("Host", host)
        .header("Origin", origin)
        .send()
        .await?;
    assert_eq!(models.status(), StatusCode::OK);
    assert_eq!(models.headers()["access-control-allow-origin"], origin);
    let _: Value = models.json().await?;

    let url = format!("{}/v1/chat/completions", fixture.base_url);
    let preflight = fixture
        .client
        .request(reqwest::Method::OPTIONS, &url)
        .header("Host", host)
        .header("Origin", origin)
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await?;
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(preflight.headers()["access-control-allow-origin"], origin);

    let mut response = fixture
        .client
        .post(&url)
        .header("Host", host)
        .header("Origin", origin)
        .json(&json!({"model":"alpha","messages":[],"stream":true}))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["access-control-allow-origin"], origin);
    let chunk = tokio::time::timeout(Duration::from_secs(2), response.chunk())
        .await??
        .unwrap();
    assert!(String::from_utf8_lossy(&chunk).contains("data:"));
    drop(response);
    wait_no_leases(&fixture.studio).await;

    let posts = fixture.posts();
    let forwarded = posts.last().unwrap();
    assert_eq!(forwarded["origin"], origin);
    assert_eq!(forwarded["locals_origins"], "*");
    assert_eq!(forwarded["bind_host"], "127.0.0.1");
    let servers = fixture.studio.supervisor.list().await;
    let first = servers
        .iter()
        .find(|server| server.alias == "alpha")
        .unwrap();
    assert_eq!(
        forwarded["host"],
        format!("127.0.0.1:{}", first.port.unwrap())
    );
    let saved = fixture
        .studio
        .store
        .get::<ServerProfile>("profiles", "first")?
        .unwrap();
    assert!(!saved.env.contains_key("LOCALS_ORIGINS"));
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_origin_policy_requires_restart_then_allows_arbitrary_origins() -> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture.start_workers().await;
    let url = format!("{}/v1/chat/completions", fixture.base_url);
    let origin = "https://new-client.example";
    let request = json!({"model":"alpha","messages":[]});
    let denied = fixture
        .client
        .post(&url)
        .header("Origin", origin)
        .json(&request)
        .send()
        .await?;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    drop(denied);
    assert!(fixture.posts().is_empty());

    let changed = fixture
        .studio
        .handle(
            "config.set",
            json!({"key":"api.allowed_origins","value":[]}),
        )
        .await?;
    assert_eq!(changed["worker_restart_required"], true);
    // The gateway accepts the new policy immediately; an existing worker still needs restart.
    let old_worker = fixture
        .client
        .post(&url)
        .header("Origin", origin)
        .json(&request)
        .send()
        .await?;
    assert_eq!(old_worker.status(), StatusCode::FORBIDDEN);
    assert_eq!(old_worker.headers()["access-control-allow-origin"], origin);
    drop(old_worker);
    assert_eq!(
        fixture.posts().last().unwrap()["locals_origins"],
        "https://client.example"
    );

    fixture
        .studio
        .handle("server.stop", json!({"id":"first"}))
        .await?;
    let started = fixture
        .studio
        .handle(
            "server.start",
            json!({"id":"first","allow_memory_overcommit":true}),
        )
        .await?;
    fixture.processes.push(OwnedProcess::capture(
        started["servers"][0]["pid"].as_u64().unwrap() as u32,
    ));
    let mut response = fixture
        .client
        .post(&url)
        .header("Origin", origin)
        .json(&json!({"model":"alpha","messages":[],"stream":true}))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["access-control-allow-origin"], origin);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), response.chunk())
            .await??
            .is_some()
    );
    drop(response);
    wait_no_leases(&fixture.studio).await;
    assert_eq!(fixture.posts().last().unwrap()["locals_origins"], "*");
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn occupied_public_port_is_reported_without_disabling_management() -> Result<()> {
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let fixture = Fixture::with_port(occupied.local_addr()?.port()).await;
    assert_eq!(fixture.studio.api_state.read().await["running"], false);
    assert!(
        fixture.studio.api_state.read().await["error"]
            .as_str()
            .is_some_and(|e| e.contains("bind"))
    );
    assert_eq!(
        fixture.studio.handle("ping", json!({})).await?["service"],
        "locals-studio"
    );
    assert!(
        fixture
            .studio
            .handle("models.list", json!({}))
            .await?
            .is_array()
    );
    let new_port = unused_port().await;
    let changed = fixture
        .studio
        .handle("config.set", json!({"key":"api.port","value":new_port}))
        .await?;
    assert_eq!(changed["api"]["running"], true);
    let health: Value = fixture
        .client
        .get(format!("http://127.0.0.1:{new_port}/health"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(health["status"], "ok");
    fixture.close().await;
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wildcard_and_loopback_cannot_share_the_public_port_and_saved_settings_can_retry()
-> Result<()> {
    for (external_host, studio_host) in [
        ("127.0.0.1", "0.0.0.0"),
        ("0.0.0.0", "127.0.0.1"),
        ("::1", "127.0.0.1"),
    ] {
        let external = tokio::net::TcpListener::bind((external_host, 0)).await?;
        let port = external.local_addr()?.port();
        let fixture = Fixture::with_port(port).await;
        let changed = fixture
            .studio
            .handle("config.set", json!({"key":"api.host","value":studio_host}))
            .await?;
        assert_eq!(changed["api"]["running"], false);
        let error = changed["api"]["error"].as_str().unwrap();
        assert!(error.contains(&format!("port {port}")), "{error}");
        assert!(
            error.contains(&format!("PID {}", std::process::id())),
            "{error}"
        );
        assert!(error.contains("listening on"), "{error}");
        assert!(fixture.studio.supervisor.list().await.is_empty());
        let failed_start = fixture
            .studio
            .handle("server.start", json!({"all":true}))
            .await
            .unwrap_err();
        assert!(
            failed_start
                .to_string()
                .contains(&format!("PID {}", std::process::id()))
        );
        assert_eq!(
            external.local_addr()?.port(),
            port,
            "the external listener was preserved"
        );

        drop(external);
        let retried = fixture
            .studio
            .handle("config.set", json!({"key":"api.host","value":studio_host}))
            .await?;
        assert_eq!(retried["api"]["running"], true, "{retried}");
        assert!(retried["api"]["error"].is_null());
        let health: Value = fixture
            .client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(health["service"], "locals-studio");
        fixture.close().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_chat_saves_partial_output_and_releases_the_worker() -> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture.start_workers().await;
    fixture.studio.handle("conversations.save", json!({"conversation":{
        "id":"conversation-one","title":"Cancel test","messages":[{"role":"user","content":"hello"}]
    }})).await?;
    let mut events = fixture.studio.events.subscribe();
    let started = fixture
        .studio
        .handle(
            "chat.start",
            json!({"conversation_id":"conversation-one","model":"alpha"}),
        )
        .await?;
    let request_id = started["request_id"].as_str().unwrap().to_owned();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if event["type"] == "chat.delta" {
                break;
            }
        }
    })
    .await
    .expect("the chat should stream before cancellation");
    assert!(
        fixture
            .studio
            .handle("conversations.remove", json!({"id":"conversation-one"}))
            .await
            .is_err()
    );
    assert!(
        fixture
            .studio
            .handle(
                "chat.start",
                json!({"conversation_id":"conversation-one","model":"alpha"})
            )
            .await
            .is_err()
    );
    fixture
        .studio
        .handle("chat.cancel", json!({"id":request_id}))
        .await?;
    let done = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if event["type"] == "chat.done" {
                break event;
            }
        }
    })
    .await
    .expect("cancellation should finish promptly");
    assert_eq!(done["data"]["cancelled"], true);
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.studio.chat.is_active("conversation-one").await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let conversation: Value = fixture
        .studio
        .store
        .get("conversations", "conversation-one")?
        .unwrap();
    let messages = conversation["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["cancelled"], true);
    assert!(!messages[1]["content"].as_str().unwrap().is_empty());
    wait_no_leases(&fixture.studio).await;
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_folder_during_chat_preserves_partial_reply_and_folder_removal() -> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture.start_workers().await;
    fixture
        .studio
        .handle(
            "folders.save",
            json!({"folder":{"id":"stream-folder","name":"Streaming work"}}),
        )
        .await?;
    fixture
        .studio
        .handle(
            "conversations.save",
            json!({"conversation":{
                "id":"folder-stream-chat","title":"Folder mutation during streaming",
                "folder_id":"stream-folder","messages":[{"role":"user","content":"hello"}]
            }}),
        )
        .await?;
    let mut events = fixture.studio.events.subscribe();
    let started = fixture
        .studio
        .handle(
            "chat.start",
            json!({"conversation_id":"folder-stream-chat","model":"alpha"}),
        )
        .await?;
    let request_id = started["request_id"].as_str().unwrap().to_owned();
    let first_delta = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if event["type"] == "chat.delta" && event["data"]["request_id"] == request_id {
                break event["data"]["delta"].as_str().unwrap().to_owned();
            }
        }
    })
    .await
    .expect("the reply should begin streaming before the folder is removed");
    assert!(!first_delta.is_empty());
    assert!(fixture.studio.chat.is_active("folder-stream-chat").await);

    fixture
        .studio
        .handle("folders.remove", json!({"id":"stream-folder"}))
        .await?;
    fixture
        .studio
        .handle("chat.cancel", json!({"id":request_id}))
        .await?;
    let done = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if event["type"] == "chat.done" && event["data"]["request_id"] == request_id {
                break event;
            }
        }
    })
    .await
    .expect("the cancelled reply should finish after the folder mutation");
    assert_eq!(done["data"]["cancelled"], true);

    let conversation: Value = fixture
        .studio
        .store
        .get("conversations", "folder-stream-chat")?
        .unwrap();
    assert!(conversation["folder_id"].is_null());
    let messages = conversation["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["cancelled"], true);
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .starts_with(&first_delta)
    );
    assert!(
        fixture
            .studio
            .store
            .get::<Value>("folders", "stream-folder")?
            .is_none()
    );
    wait_no_leases(&fixture.studio).await;
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_stop_cancels_an_active_batch_before_another_worker_can_launch() -> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture.start_workers().await;
    fixture
        .studio
        .handle("server.stop", json!({"all":true}))
        .await?;
    for mut profile in fixture.studio.store.list::<ServerProfile>("profiles")? {
        profile.env.insert("FAKE_STARTUP_DELAY".into(), "30".into());
        profile.env.insert("FAKE_IGNORE_SIGTERM".into(), "1".into());
        fixture
            .studio
            .handle("profiles.save", json!({"profile":profile}))
            .await?;
    }
    let mut events = fixture.studio.events.subscribe();
    let starting = fixture.studio.clone();
    let batch = tokio::spawn(async move {
        starting
            .handle(
                "server.start",
                json!({"all":true,"allow_memory_overcommit":true}),
            )
            .await
    });
    let loading = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = fixture
                .studio
                .supervisor
                .list()
                .await
                .into_iter()
                .find(|status| status.state == "starting" && status.pid.is_some())
            {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("The batch did not publish a starting worker within 5 seconds")?;
    fixture
        .processes
        .push(OwnedProcess::capture(loading.pid.unwrap()));
    // Observe the Python handshake as well as the Rust status: SIGTERM must already
    // be ignored before Stop exercises the supervisor's real grace-period fallback.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if read_records(&fixture.request_log).iter().any(|event| {
                event["event"] == "booting"
                    && event["pid"] == loading.pid.unwrap()
                    && event["ignores_sigterm"] == true
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("The loading fake worker did not acknowledge its SIGTERM handler within 5 seconds")?;
    // The supervisor permits a 3s stats probe, 5s SIGTERM grace period and 5s
    // child reap. Leave 2s for scheduling; the old 5s bound cut off valid cleanup.
    let stopped = tokio::time::timeout(
        Duration::from_secs(15),
        fixture.studio.handle("server.stop", json!({"all":true})),
    )
    .await
    .context("Global Stop did not finish within its 15-second shutdown deadline")??;
    assert_eq!(stopped["api"]["enabled"], false);
    assert_eq!(stopped["api"]["running"], false);
    let result = tokio::time::timeout(Duration::from_secs(2), batch)
        .await
        .context(
            "The cancelled start batch did not return within 2 seconds after Stop completed",
        )???;
    assert_eq!(result["cancelled"], true);
    assert!(
        fixture
            .studio
            .supervisor
            .list()
            .await
            .iter()
            .all(|status| status.pid.is_none())
    );
    let launched: std::collections::HashSet<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event["type"] == "server.state" && event["data"]["state"] == "starting")
        .filter_map(|event| event["data"]["pid"].as_u64())
        .collect();
    assert_eq!(
        launched,
        std::collections::HashSet::from([loading.pid.unwrap() as u64]),
        "A cancelled global batch must not launch its next model, even before Python boots"
    );
    assert_eq!(
        fixture.studio.handle("ping", json!({})).await?["service"],
        "locals-studio"
    );
    assert!(
        fixture
            .client
            .get(format!("{}/health", fixture.base_url))
            .send()
            .await
            .is_err()
    );
    // A targeted start is also an explicit request to reactivate the global API.
    let mut first = fixture
        .studio
        .store
        .get::<ServerProfile>("profiles", "first")?
        .unwrap();
    first.env.remove("FAKE_STARTUP_DELAY");
    first.env.remove("FAKE_IGNORE_SIGTERM");
    fixture
        .studio
        .handle("profiles.save", json!({"profile":first}))
        .await?;
    let restarted = fixture
        .studio
        .handle(
            "server.start",
            json!({"id":"first","allow_memory_overcommit":true}),
        )
        .await?;
    assert_eq!(restarted["api"]["running"], true);
    let pid = restarted["servers"][0]["pid"].as_u64().unwrap() as u32;
    fixture.processes.push(OwnedProcess::capture(pid));
    assert_eq!(fixture.post("alpha").await["worker"], "alpha");
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_global_off_finishes_owned_worker_shutdown_after_reload() -> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture.start_workers().await;
    let mut config = fixture.studio.config.read().await.clone();
    config.api.enabled = false;
    fixture.studio.store.set("settings", "config", &config)?;
    *fixture.studio.config.write().await = config;
    // Simulate the persisted-OFF crash boundary: listener closed, owned workers still alive.
    fixture.studio.restart_gateway().await;
    assert!(
        fixture
            .studio
            .supervisor
            .list()
            .await
            .iter()
            .any(|status| status.pid.is_some())
    );
    let recovered = Studio::new(fixture.studio.paths.clone()).await?;
    assert_eq!(recovered.api_state.read().await["enabled"], false);
    assert_eq!(recovered.api_state.read().await["running"], false);
    assert!(
        recovered
            .supervisor
            .list()
            .await
            .iter()
            .all(|status| status.pid.is_none())
    );
    assert_eq!(
        recovered.handle("ping", json!({})).await?["service"],
        "locals-studio"
    );
    recovered.shutdown().await?;
    fixture.close().await;
    Ok(())
}
