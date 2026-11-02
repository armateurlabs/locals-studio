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
import argparse,json,os,signal,socket,threading,time
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
model_contexts=json.loads(os.environ.get('FAKE_MODEL_CONTEXTS','{}'))
log_path=os.environ['FAKE_REQUEST_LOG']
chat_barrier=threading.Barrier(2)
def record(event,**fields):
 with open(log_path,'a') as out:
  out.write(json.dumps(dict(event=event,worker=alias,bind_host=args.host,**fields))+'\n')
class Handler(BaseHTTPRequestHandler):
 protocol_version='HTTP/1.1'
 def log_message(self,*args): pass
 def reply(self,data,status=200):
  body=json.dumps(data).encode()
  self.send_response(status);self.send_header('Content-Type','application/json')
  # Worker-supplied routing metadata is untrusted; the gateway must replace it.
  for key,value in [('X-Locals-Profile-Id','forged-profile'),('X-Locals-Model-Id','forged-model'),('X-Locals-Server-Alias','forged-alias')]: self.send_header(key,value)
  self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self):
  record('probe',path=self.path,model_path=args.model)
  if self.path=='/health': self.reply({'status':'ok','version':'0.2.1'})
  elif self.path=='/v1/models': self.reply({'data':[dict(id=model,**model_contexts.get(model,{})) for model in model_ids]})
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
  scenario=body.get('test_scenario')
  if scenario=='transport_error':
   self.close_connection=True
   self.connection.shutdown(socket.SHUT_RDWR);self.connection.close();return
  if scenario=='http_error':
   self.reply({'error':{'message':'fake HTTP failure'}},500);return
  if scenario=='empty':
   self.send_response(204);self.send_header('Content-Length','0');self.end_headers();return
  if scenario=='ndjson_error_no_lf':
   raw=json.dumps({'error':'fake NDJSON final failure'}).encode()
   self.send_response(200);self.send_header('Content-Type','application/x-ndjson')
   self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw);return
  if not body.get('stream'):
   self.reply({'worker':alias,'received_model':body.get('model'),'received_kwargs':body.get('chat_template_kwargs'),'choices':[{'message':{'role':'assistant','content':alias},'finish_reason':'stop'}]})
   return
  self.send_response(200);self.send_header('Content-Type','application/x-ndjson' if scenario=='ndjson' else 'text/event-stream')
  self.send_header('Cache-Control','no-cache');self.send_header('Connection','close');self.end_headers()
  self.close_connection=True
  try:
   if scenario=='chat_isolation':
    marker=body['messages'][-1]['content']
    record('chat_history',messages=body['messages'],model=body['model'])
    if body.get('test_pair_barrier'): chat_barrier.wait(timeout=5)
    frames=[{'choices':[{'delta':{'reasoning_content':'Reasoning for '+marker}}]},
            {'choices':[{'delta':{'content':'Reply for '}}]},
            {'choices':[{'delta':{'content':marker}}]},
            {'choices':[{'delta':{},'finish_reason':'stop'}]}]
    for frame in frames:
     self.wfile.write(('data: '+json.dumps(frame)+'\n\n').encode());self.wfile.flush()
    self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush();return
   if scenario in ('chat_length','chat_stop'):
    frames=[{'choices':[{'delta':{'reasoning_content':'Analyse 🌱 avant la réponse.'},'finish_reason':None}]}]
    if scenario=='chat_length':
     for index in range(12):
      frames.append({'choices':[{'delta':{'content':'Section '+str(index)+': '+('Réponse 🌍 — texte complet. '*128)+'\n'},'finish_reason':None}]})
    else:
     frames.append({'choices':[{'delta':{'content':'Terminé normalement.'},'finish_reason':None}]})
    frames.append({'choices':[{'delta':{},'finish_reason':'length' if scenario=='chat_length' else 'stop'}]})
    # Usage arrives after the finish frame and must not erase its reason.
    frames.append({'choices':[],'usage':{'prompt_tokens':37,'completion_tokens':4096 if scenario=='chat_length' else 20}})
    for frame in frames:
     raw=('data: '+json.dumps(frame,ensure_ascii=False)+'\n\n').encode()
     for offset in range(0,len(raw),17):
      self.wfile.write(raw[offset:offset+17]);self.wfile.flush()
    self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush();return
   if scenario in ('sse','anthropic','ndjson','inference_error'):
    if scenario=='ndjson':
     frames=[{'message':{'role':'assistant'},'done':False},{'message':{'thinking':'working'},'done':False},{'message':{'content':'answer'},'done':False},{'done':True,'prompt_eval_count':12,'eval_count':3}]
     frames=[json.dumps(frame)+'\n' for frame in frames]
    elif scenario=='anthropic':
     frames=[{'type':'message_start','message':{'usage':{'input_tokens':12,'output_tokens':0}}},{'type':'content_block_delta','delta':{'type':'thinking_delta','thinking':'working'}},{'type':'content_block_delta','delta':{'type':'text_delta','text':'answer'}},{'type':'message_delta','delta':{'stop_reason':'end_turn'},'usage':{'output_tokens':3}},{'type':'message_stop'}]
     frames=['event: '+frame['type']+'\ndata: '+json.dumps(frame)+'\n\n' for frame in frames]
    elif scenario=='inference_error':
     frames=['data: '+json.dumps({'error':{'type':'server_error','message':'fake inference failure'}})+'\n\n','data: [DONE]\n\n']
    else:
     frames=[{'choices':[{'delta':{'role':'assistant'}}]},{'choices':[{'delta':{'reasoning_content':'working'}}]},{'choices':[{'delta':{'content':'answer'}}]},{'choices':[{'delta':{},'finish_reason':'stop'}],'usage':{'prompt_tokens':12,'completion_tokens':3}}]
     frames=['data: '+json.dumps(frame)+'\n\n' for frame in frames]+['data: [DONE]\n\n']
    for frame in frames:
     self.wfile.write(frame.encode());self.wfile.flush();time.sleep(0.01)
    return
   for index in range(1000 if scenario=='long_stream' else 100):
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
        self.start_worker_with_contexts(id, alias, canonical, model_ids, json!({}))
            .await;
    }

    async fn start_worker_with_contexts(
        &mut self,
        id: &str,
        alias: &str,
        canonical: &str,
        model_ids: &[&str],
        contexts: Value,
    ) {
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
        profile
            .env
            .insert("FAKE_MODEL_CONTEXTS".into(), contexts.to_string());
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

    async fn set_metrics_enabled(&self, enabled: bool) -> Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.studio.handle(
                "config.set",
                json!({"key":"api.metrics_enabled","value":enabled}),
            ),
        )
        .await
        .context("changing only metrics must not drain the inference listener")?
    }

    async fn metrics(&self) -> Result<String> {
        Ok(self
            .client
            .get(format!("{}/metrics", self.base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
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

async fn start_alias_pool_workers(fixture: &mut Fixture) {
    for (id, alias, canonical, context) in [
        ("first", "alpha", "canonical-first", 32768),
        ("second", "beta", "canonical-second", 8192),
    ] {
        let ids = [
            format!("locals/{canonical}"),
            format!("locals/{canonical}-thinking"),
            format!("mlx/{canonical}"),
            format!("mlx/{canonical}-thinking"),
        ];
        let refs: Vec<_> = ids.iter().map(String::as_str).collect();
        let contexts: serde_json::Map<String, Value> = ids
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    json!({"context_length":context,"max_model_len":context}),
                )
            })
            .collect();
        fixture
            .start_worker_with_contexts(id, alias, &ids[0], &refs, Value::Object(contexts))
            .await;
    }
}

async fn save_alias_pool(fixture: &Fixture, ids: &[&str]) -> Result<Value> {
    fixture
        .studio
        .handle(
            "aliases.save",
            json!({"alias":{"id":"qwen-pool","name":"qwen","profile_ids":ids}}),
        )
        .await
}

async fn alias_response(
    fixture: &Fixture,
    model: &str,
    session: Option<&str>,
    endpoint: &str,
) -> Result<reqwest::Response> {
    let body = if matches!(endpoint, "/api/generate" | "/v1/completions") {
        json!({"model":model,"prompt":"Synthetic alias test","stream":false})
    } else {
        json!({"model":model,"messages":[{"role":"user","content":"Synthetic alias test"}],"max_tokens":16,"stream":false})
    };
    let mut request = fixture
        .client
        .post(format!("{}{endpoint}", fixture.base_url))
        .timeout(Duration::from_secs(5))
        .json(&body);
    if let Some(session) = session {
        request = request.header("X-Locals-Session-Id", session);
    }
    Ok(request.send().await?)
}

async fn alias_json(
    fixture: &Fixture,
    model: &str,
    session: Option<&str>,
    endpoint: &str,
) -> Result<(String, Value)> {
    let response = alias_response(fixture, model, session, endpoint).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let id = response.headers()["x-locals-profile-id"]
        .to_str()?
        .to_owned();
    assert!(matches!(id.as_str(), "first" | "second"));
    assert_eq!(
        response.headers()["x-locals-model-id"].to_str()?,
        format!("model-{id}")
    );
    let alias = if id == "first" { "alpha" } else { "beta" };
    assert_eq!(response.headers()["x-locals-server-alias"].to_str()?, alias);
    let body: Value = response.json().await?;
    assert_eq!(body["worker"], alias);
    Ok((id, body))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alias_pools_keep_affinity_fail_over_once_and_recover_the_new_binding_after_reload()
-> Result<()> {
    let mut fixture = Fixture::new().await;
    start_alias_pool_workers(&mut fixture).await;
    let saved = save_alias_pool(&fixture, &["first", "second", "first"]).await?;
    assert_eq!(saved["profile_ids"], json!(["first", "second"]));
    for alias in [
        json!({"id":"qwen-pool","name":"renamed","profile_ids":["first"]}),
        json!({"id":"collision","name":"qwen","profile_ids":["first"]}),
        json!({"id":"profile-collision","name":"alpha","profile_ids":["first"]}),
        json!({"id":"empty","name":"empty","profile_ids":[]}),
        json!({"id":"dangling","name":"dangling","profile_ids":["not-a-profile"]}),
    ] {
        assert!(
            fixture
                .studio
                .handle("aliases.save", json!({"alias":alias}))
                .await
                .is_err()
        );
    }
    assert_eq!(
        fixture
            .studio
            .handle("aliases.list", json!({}))
            .await?
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut colliding = fixture
        .studio
        .store
        .get::<ServerProfile>("profiles", "first")?
        .unwrap();
    colliding.alias = "qwen".into();
    assert!(
        fixture
            .studio
            .handle("profiles.save", json!({"profile":colliding}))
            .await
            .is_err()
    );
    let original = fixture
        .studio
        .store
        .get::<ServerProfile>("profiles", "first")?
        .unwrap();
    let mut renamed = original.clone();
    renamed.alias = "renamed-alpha".into();
    fixture
        .studio
        .handle("profiles.save", json!({"profile":renamed}))
        .await?;
    assert!(fixture.studio.handle("aliases.save", json!({"alias":{"id":"live-worker-name","name":"alpha","profile_ids":["first","second"]}})).await.is_err(), "renaming the saved profile must not free the alias still served by its active worker");
    save_alias_pool(&fixture, &["first", "second"]).await?;
    fixture
        .studio
        .handle("profiles.save", json!({"profile":original}))
        .await?;

    let (assigned, _) = alias_json(
        &fixture,
        "qwen",
        Some("conversation-a"),
        "/v1/chat/completions",
    )
    .await?;
    for _ in 0..3 {
        assert_eq!(
            alias_json(
                &fixture,
                "qwen",
                Some("conversation-a"),
                "/v1/chat/completions"
            )
            .await?
            .0,
            assigned
        );
    }
    let (other, _) = alias_json(
        &fixture,
        "qwen",
        Some("conversation-b"),
        "/v1/chat/completions",
    )
    .await?;
    assert_ne!(
        assigned, other,
        "new conversations use the next ready pool member"
    );
    let (default, _) = alias_json(&fixture, "qwen", None, "/v1/chat/completions").await?;
    for _ in 0..2 {
        assert_eq!(
            alias_json(&fixture, "qwen", None, "/v1/chat/completions")
                .await?
                .0,
            default,
            "requests without a session ID use one shared pool slot"
        );
    }
    let changed_default = fixture.client.post(format!("{}/v1/chat/completions", fixture.base_url))
        .json(&json!({"model":"qwen","user":"another-user","metadata":{"conversation_id":"not-the-session-header"},"messages":[{"role":"user","content":"An entirely different history without a session header"}]}))
        .send().await?;
    assert_eq!(
        changed_default.headers()["x-locals-profile-id"].to_str()?,
        default,
        "body metadata and message history must not invent a different no-header session"
    );
    changed_default.bytes().await?;
    assert_eq!(fixture.post("alpha").await["worker"], "alpha");
    assert_eq!(fixture.post("beta").await["worker"], "beta");
    wait_no_leases(&fixture.studio).await;
    fixture
        .studio
        .handle("server.stop", json!({"id":assigned}))
        .await?;
    assert!(
        fixture
            .studio
            .handle("profiles.remove", json!({"id":assigned}))
            .await
            .is_err(),
        "a pool must not silently acquire a dangling profile reference"
    );
    assert_eq!(
        alias_json(
            &fixture,
            "qwen",
            Some("conversation-a"),
            "/v1/chat/completions"
        )
        .await?
        .0,
        other
    );
    assert_eq!(
        alias_json(&fixture, "qwen", None, "/v1/chat/completions")
            .await?
            .0,
        other,
        "the shared no-header slot also fails over"
    );
    let restarted = fixture
        .studio
        .handle(
            "server.start",
            json!({"id":assigned,"allow_memory_overcommit":true}),
        )
        .await?;
    fixture.processes.push(OwnedProcess::capture(
        restarted["servers"][0]["pid"].as_u64().unwrap() as u32,
    ));
    assert_eq!(
        alias_json(
            &fixture,
            "qwen",
            Some("conversation-a"),
            "/v1/chat/completions"
        )
        .await?
        .0,
        other,
        "a recovered preferred member must not pull a session back"
    );
    assert_eq!(
        alias_json(&fixture, "qwen", None, "/v1/chat/completions")
            .await?
            .0,
        other
    );
    wait_no_leases(&fixture.studio).await;
    let before = fixture.studio.supervisor.list().await;
    // Close only this fixture's router; the persisted API setting remains ON,
    // so a fresh Studio instance adopts the workers and opens the same port.
    fixture.studio.config.write().await.api.enabled = false;
    fixture.studio.restart_gateway().await;
    fixture.studio = Studio::new(fixture.studio.paths.clone()).await?;
    assert_eq!(fixture.studio.api_state.read().await["running"], true);
    assert_eq!(
        alias_json(
            &fixture,
            "qwen",
            Some("conversation-a"),
            "/v1/chat/completions"
        )
        .await?
        .0,
        other,
        "bindings survive a new management-service instance"
    );
    for previous in &before {
        assert_eq!(
            fixture
                .studio
                .supervisor
                .list()
                .await
                .into_iter()
                .find(|status| status.profile_id == previous.profile_id)
                .unwrap()
                .pid,
            previous.pid
        );
    }
    // A profile can be edited before its old worker restarts. That stale ready
    // worker must not impersonate the newly selected model in pool routing.
    let mut changed = fixture
        .studio
        .store
        .get::<ServerProfile>("profiles", &other)?
        .unwrap();
    changed.model_id = format!("model-{assigned}");
    fixture
        .studio
        .handle("profiles.save", json!({"profile":changed}))
        .await?;
    assert_eq!(
        alias_json(
            &fixture,
            "qwen",
            Some("conversation-a"),
            "/v1/chat/completions"
        )
        .await?
        .0,
        assigned
    );
    fixture
        .studio
        .handle("aliases.remove", json!({"id":"qwen-pool"}))
        .await?;
    assert!(
        fixture
            .studio
            .handle("aliases.list", json!({}))
            .await?
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .studio
            .store
            .list::<Value>("alias-bindings")?
            .is_empty()
    );
    assert_eq!(
        alias_response(
            &fixture,
            "qwen",
            Some("conversation-a"),
            "/v1/chat/completions"
        )
        .await?
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        fixture
            .studio
            .store
            .list::<ServerProfile>("profiles")?
            .len(),
        2
    );
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alias_pools_assign_concurrent_first_requests_atomically_and_share_modes_and_protocols()
-> Result<()> {
    let mut fixture = Fixture::new().await;
    start_alias_pool_workers(&mut fixture).await;
    save_alias_pool(&fixture, &["first", "second"]).await?;
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let client = fixture.client.clone();
        let url = format!("{}/v1/chat/completions", fixture.base_url);
        requests.spawn(async move {
            let response = client
                .post(url)
                .header("X-Locals-Session-Id", "concurrent-new-session")
                .timeout(Duration::from_secs(5))
                .json(
                    &json!({"model":"qwen","messages":[{"role":"user","content":"same session"}]}),
                )
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            let profile = response.headers()["x-locals-profile-id"]
                .to_str()?
                .to_owned();
            let body: Value = response.json().await?;
            Ok::<_, anyhow::Error>((profile, body))
        });
    }
    let mut assigned = None;
    while let Some(response) = requests.join_next().await {
        let (profile, body) = response??;
        assert_eq!(
            body["worker"],
            if profile == "first" { "alpha" } else { "beta" }
        );
        if let Some(assigned) = &assigned {
            assert_eq!(
                &profile, assigned,
                "two first requests cannot race into different members"
            );
        } else {
            assigned = Some(profile);
        }
    }
    let assigned = assigned.unwrap();
    for requested in ["qwen", "qwen-thinking", "mlx/qwen", "mlx/qwen-thinking"] {
        let (profile, body) = alias_json(
            &fixture,
            requested,
            Some("concurrent-new-session"),
            "/v1/chat/completions",
        )
        .await?;
        assert_eq!(
            profile, assigned,
            "mode variants share the base alias's affinity"
        );
        let engine = if requested.starts_with("mlx/") {
            "mlx"
        } else {
            "locals"
        };
        let suffix = if requested.ends_with("-thinking") {
            "-thinking"
        } else {
            ""
        };
        assert_eq!(
            body["received_model"],
            format!("{engine}/canonical-{assigned}{suffix}")
        );
    }
    for endpoint in [
        "/v1/messages",
        "/v1/messages/count_tokens",
        "/api/chat",
        "/api/generate",
        "/v1/completions",
    ] {
        assert_eq!(
            alias_json(&fixture, "qwen", Some("concurrent-new-session"), endpoint)
                .await?
                .0,
            assigned,
            "the same explicit session is independent of protocol"
        );
    }
    let bindings_before = fixture.studio.store.list::<Value>("alias-bindings")?.len();
    let preflight = fixture
        .client
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/v1/chat/completions", fixture.base_url),
        )
        .header("Origin", "https://client.example")
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "content-type,x-locals-session-id",
        )
        .send()
        .await?;
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert!(
        preflight.headers()[reqwest::header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()?
            .to_ascii_lowercase()
            .contains("x-locals-session-id")
    );
    assert_eq!(
        fixture.studio.store.list::<Value>("alias-bindings")?.len(),
        bindings_before,
        "a browser preflight must not allocate affinity"
    );
    let browser = fixture
        .client
        .post(format!("{}/v1/chat/completions", fixture.base_url))
        .header("Origin", "https://client.example")
        .header("X-Locals-Session-Id", "concurrent-new-session")
        .json(&json!({"model":"qwen","messages":[{"role":"user","content":"browser client"}]}))
        .send()
        .await?;
    assert_eq!(browser.headers()["x-locals-profile-id"].to_str()?, assigned);
    let exposed = browser.headers()[reqwest::header::ACCESS_CONTROL_EXPOSE_HEADERS]
        .to_str()?
        .to_ascii_lowercase();
    for name in [
        "x-locals-profile-id",
        "x-locals-model-id",
        "x-locals-server-alias",
    ] {
        assert!(exposed.split(',').any(|value| value.trim() == name));
    }
    browser.bytes().await?;
    let models: Value = fixture
        .client
        .get(format!("{}/v1/models", fixture.base_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for id in ["qwen", "qwen-thinking", "mlx/qwen", "mlx/qwen-thinking"] {
        let entries: Vec<_> = models["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|model| model["id"] == id)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]["context_length"], 8192,
            "pool context is conservative across ready members"
        );
        assert_eq!(entries[0]["max_model_len"], 8192);
    }
    let before = fixture.posts().len();
    let failed = fixture.client.post(format!("{}/v1/chat/completions", fixture.base_url)).header("X-Locals-Session-Id", "concurrent-new-session")
        .timeout(Duration::from_secs(5)).json(&json!({"model":"qwen","messages":[{"role":"user","content":"transport failure"}],"test_scenario":"transport_error"})).send().await?;
    assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(failed.headers()["x-locals-profile-id"].to_str()?, assigned);
    failed.bytes().await?;
    assert_eq!(
        fixture.posts().len(),
        before + 1,
        "an upstream failure must not replay the same request against another model"
    );
    assert_eq!(
        alias_json(
            &fixture,
            "qwen",
            Some("concurrent-new-session"),
            "/v1/chat/completions"
        )
        .await?
        .0,
        assigned,
        "a failed request does not discard an otherwise ready session target"
    );
    wait_no_leases(&fixture.studio).await;
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alias_pool_streams_keep_bytes_and_lease_while_later_requests_fail_over_after_membership_changes()
-> Result<()> {
    let mut fixture = Fixture::new().await;
    start_alias_pool_workers(&mut fixture).await;
    save_alias_pool(&fixture, &["first", "second"]).await?;
    fixture.set_metrics_enabled(true).await?;
    let (assigned, _) = alias_json(
        &fixture,
        "qwen",
        Some("stream-session"),
        "/v1/chat/completions",
    )
    .await?;
    let assigned_status = fixture
        .studio
        .supervisor
        .list()
        .await
        .into_iter()
        .find(|status| status.profile_id == assigned)
        .unwrap();
    let gateway = fixture.client.post(format!("{}/v1/chat/completions", fixture.base_url)).header("X-Locals-Session-Id", "stream-session")
        .json(&json!({"model":"qwen","messages":[{"role":"user","content":"test"}],"stream":true,"test_scenario":"sse"})).send().await?;
    assert_eq!(gateway.headers()["x-locals-profile-id"].to_str()?, assigned);
    let gateway_bytes = gateway.bytes().await?;
    let direct_bytes = fixture.client.post(format!("http://127.0.0.1:{}/v1/chat/completions", assigned_status.port.unwrap()))
        .json(&json!({"model":assigned_status.backend_model,"messages":[{"role":"user","content":"test"}],"stream":true,"test_scenario":"sse"}))
        .send().await?.bytes().await?;
    assert_eq!(
        gateway_bytes, direct_bytes,
        "pool routing preserves the worker's SSE framing byte for byte"
    );
    let mut stream = fixture.client.post(format!("{}/v1/chat/completions", fixture.base_url)).header("X-Locals-Session-Id", "stream-session")
        .timeout(Duration::from_secs(10)).json(&json!({"model":"qwen","messages":[{"role":"user","content":"test"}],"stream":true,"test_scenario":"long_stream"})).send().await?;
    let first = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
        .await??
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains(&format!("{}:", assigned_status.alias)));
    assert_eq!(
        fixture
            .studio
            .supervisor
            .list()
            .await
            .into_iter()
            .find(|status| status.profile_id == assigned)
            .unwrap()
            .active_requests,
        1
    );
    let other = if assigned == "first" {
        "second"
    } else {
        "first"
    };
    tokio::time::timeout(Duration::from_secs(5), save_alias_pool(&fixture, &[other])).await??;
    assert_eq!(
        alias_json(
            &fixture,
            "qwen",
            Some("stream-session"),
            "/v1/chat/completions"
        )
        .await?
        .0,
        other
    );
    let next = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
        .await??
        .unwrap();
    assert!(
        String::from_utf8_lossy(&next).contains(&format!("{}:", assigned_status.alias)),
        "an open stream is never redirected when its session's future route changes"
    );
    drop(stream);
    wait_no_leases(&fixture.studio).await;
    let labels = [
        ("profile_id", assigned.as_str()),
        ("endpoint", "/v1/chat/completions"),
        ("outcome", "cancelled"),
    ];
    wait_metric(
        &fixture,
        "locals_studio_inference_requests_total",
        &labels,
        1.0,
    )
    .await?;
    assert_eq!(
        alias_json(
            &fixture,
            "qwen",
            Some("stream-session"),
            "/v1/chat/completions"
        )
        .await?
        .0,
        other
    );
    assert_eq!(
        fixture
            .studio
            .supervisor
            .list()
            .await
            .into_iter()
            .find(|status| status.profile_id == assigned)
            .unwrap()
            .pid,
        assigned_status.pid
    );
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alias_pool_chat_manager_preserves_unicode_session_history_and_persisted_failover_metadata()
-> Result<()> {
    let mut fixture = Fixture::new().await;
    start_alias_pool_workers(&mut fixture).await;
    save_alias_pool(&fixture, &["first", "second"]).await?;
    let id = format!("chat partagé 🌍 {}", "long".repeat(100));
    assert!(id.len() > 256);
    let mut history = vec![json!({"role":"user","content":"First pool conversation turn"})];
    let mut outbound = Vec::new();
    let mut first_profile = None;
    let mut events = fixture.studio.events.subscribe();
    for turn in 0..2 {
        if turn == 1 {
            history.push(
                json!({"role":"user","content":"Continue this same conversation after failover"}),
            );
        }
        let marker = history.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned();
        outbound.push(json!(history));
        fixture.studio.handle("conversations.save", json!({"conversation":{"id":id,"title":"Synthetic pooled chat","model":"qwen","messages":history}})).await?;
        let started = fixture.studio.handle("chat.start", json!({"conversation_id":id,"model":"qwen","parameters":{"test_scenario":"chat_isolation"}})).await?;
        let request_id = started["request_id"].clone();
        let done = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if event["data"]["request_id"] != request_id { continue; }
                match event["type"].as_str() {
                    Some("chat.done") => break event,
                    Some("chat.error") => panic!("pooled ChatManager request failed: {event}"),
                    _ => {}
                }
            }
        }).await.context("the pooled chat must finish with a valid ASCII session header even for a long Unicode conversation ID")?;
        assert_eq!(done["data"]["conversation_id"], id);
        let message = done["data"]["message"].clone();
        assert_eq!(message["content"], format!("Reply for {marker}"));
        let routed = message["routed_profile_id"]
            .as_str()
            .context("reply must retain the gateway's selected profile")?
            .to_owned();
        assert!(matches!(routed.as_str(), "first" | "second"));
        assert_eq!(message["routed_model_id"], format!("model-{routed}"));
        assert_eq!(
            message["routed_alias"],
            if routed == "first" { "alpha" } else { "beta" }
        );
        if let Some(first) = &first_profile {
            assert_ne!(
                &routed, first,
                "stopping the first routed member must update this conversation's actual target"
            );
        }
        history.push(message);
        let saved = Store::open(&fixture.studio.paths.database_path)?
            .get::<Value>("conversations", &id)?
            .unwrap();
        assert_eq!(
            saved["model"], "qwen",
            "actual route metadata must not overwrite the public alias"
        );
        assert_eq!(
            saved["messages"],
            json!(history),
            "the earlier reply and its original routing metadata remain in the saved history"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.studio.chat.is_active(&id).await {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        wait_no_leases(&fixture.studio).await;
        if turn == 0 {
            first_profile = Some(routed.clone());
            fixture
                .studio
                .handle("server.stop", json!({"id":routed}))
                .await?;
        }
    }
    let received: Vec<_> = read_records(&fixture.request_log)
        .into_iter()
        .filter(|row| row["event"] == "chat_history")
        .collect();
    assert_eq!(received.len(), 2);
    for (index, expected) in outbound.iter().enumerate() {
        assert_eq!(
            &received[index]["messages"], expected,
            "each routed worker must receive the complete current conversation history"
        );
    }
    let bindings = fixture.studio.store.list::<Value>("alias-bindings")?;
    assert_eq!(
        bindings.len(),
        1,
        "the same Unicode conversation keeps one stable affinity slot across turns"
    );
    assert_ne!(bindings[0]["profile_id"], first_profile.unwrap());
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alias_pool_thinking_variant_never_shadows_an_exact_live_worker_after_catalog_rename()
-> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture
        .start_worker("first", "qwen-thinking", "locals/plain", &["locals/plain"])
        .await;
    save_alias_pool(&fixture, &["first"]).await?;
    let mut profile = fixture
        .studio
        .store
        .get::<ServerProfile>("profiles", "first")?
        .unwrap();
    profile.alias = "qwen-27b".into();
    fixture
        .studio
        .handle("profiles.save", json!({"profile":profile}))
        .await?;
    let list: Value = fixture
        .client
        .get(format!("{}/v1/models", fixture.base_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|model| model["id"] == "qwen-thinking")
            .count(),
        1
    );
    let response = alias_response(&fixture, "qwen-thinking", None, "/v1/chat/completions").await?;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the exact live alias must not become a pool's unsupported thinking variant"
    );
    assert_eq!(response.headers()["x-locals-profile-id"], "first");
    assert_eq!(response.headers()["x-locals-server-alias"], "qwen-thinking");
    let body: Value = response.json().await?;
    assert_eq!(body["worker"], "qwen-thinking");
    assert_eq!(body["received_model"], "locals/plain");
    fixture.close().await;
    Ok(())
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

// Test labels are fixed fixture IDs and endpoint paths, so exact label matching
// is sufficient here without adding a second Prometheus parser dependency.
fn metric_value(body: &str, name: &str, labels: &[(&str, &str)]) -> f64 {
    body.lines()
        .filter_map(|line| {
            let sample = line.strip_prefix(name)?;
            let (actual_labels, value) = if let Some(sample) = sample.strip_prefix('{') {
                sample.split_once('}')?
            } else if sample.starts_with(char::is_whitespace) {
                ("", sample)
            } else {
                return None;
            };
            labels
                .iter()
                .all(|(key, expected)| {
                    let expected = format!("{key}=\"{expected}\"");
                    actual_labels.split(',').any(|label| label == expected)
                })
                .then(|| value.split_whitespace().next()?.parse::<f64>().ok())
                .flatten()
        })
        .sum()
}

async fn wait_metric(
    fixture: &Fixture,
    name: &str,
    labels: &[(&str, &str)],
    expected: f64,
) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let body = fixture.metrics().await?;
            if metric_value(&body, name, labels) == expected {
                return Ok(body);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("waiting for {name}{labels:?} to equal {expected}"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prometheus_is_opt_in_persisted_and_uses_existing_host_and_origin_policy() -> Result<()> {
    let fixture = Fixture::new().await;
    let model_path = fixture._directory.path().join("not-started-model");
    fs::create_dir_all(&model_path)?;
    let model = ModelRecord {
        id: "not-started-model".into(),
        name: "Available but stopped".into(),
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
    fixture.studio.store.set("models", &model.id, &model)?;
    let profile = ServerProfile {
        id: "not-started".into(),
        model_id: model.id.clone(),
        alias: "not-started".into(),
        selected: true,
        ..Default::default()
    };
    fixture
        .studio
        .store
        .set("profiles", &profile.id, &profile)?;
    let url = format!("{}/metrics", fixture.base_url);
    assert_eq!(
        fixture.client.get(&url).send().await?.status(),
        StatusCode::NOT_FOUND
    );
    let enabled = fixture.set_metrics_enabled(true).await?;
    assert_eq!(enabled["config"]["api"]["metrics_enabled"], true);
    assert_eq!(enabled["api"]["running"], true);
    let persisted: Value = fixture.studio.store.get("settings", "config")?.unwrap();
    assert_eq!(persisted["api"]["metrics_enabled"], true);
    let response = fixture.client.get(&url).send().await?;
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers()[reqwest::header::CONTENT_TYPE].to_str()?;
    assert!(content_type.starts_with("text/plain"));
    assert!(content_type.contains("version=0.0.4"));
    let body = response.text().await?;
    assert!(body.contains("# HELP ") && body.contains("# TYPE "));
    assert_eq!(
        metric_value(&body, "locals_studio_inference_requests_total", &[]),
        0.0
    );
    assert_eq!(
        fixture
            .client
            .get(&url)
            .header("Host", "attacker.example:8321")
            .send()
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .client
            .get(&url)
            .header("Origin", "https://attacker.example")
            .send()
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    let allowed = fixture
        .client
        .get(&url)
        .header("Origin", "https://client.example")
        .send()
        .await?;
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed.headers()["access-control-allow-origin"],
        "https://client.example"
    );
    assert!(
        fixture.studio.supervisor.list().await.is_empty(),
        "scraping must not start selected profiles"
    );
    assert!(read_records(&fixture.request_log).is_empty());
    fixture.set_metrics_enabled(false).await?;
    assert_eq!(
        fixture.client.get(&url).send().await?.status(),
        StatusCode::NOT_FOUND
    );
    let persisted: Value = fixture.studio.store.get("settings", "config")?.unwrap();
    assert_eq!(persisted["api"]["metrics_enabled"], false);
    assert_eq!(
        fixture
            .client
            .get(format!("{}/health", fixture.base_url))
            .send()
            .await?
            .status(),
        StatusCode::OK
    );
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prometheus_toggles_in_place_while_an_existing_inference_stream_keeps_running() -> Result<()>
{
    let mut fixture = Fixture::new().await;
    fixture
        .start_worker("first", "alpha", "locals/canonical", &["locals/canonical"])
        .await;
    fixture.set_metrics_enabled(true).await?;
    let before = fixture.studio.supervisor.list().await;
    let mut response = fixture.client.post(format!("{}/v1/chat/completions", fixture.base_url))
        .timeout(Duration::from_secs(20))
        .json(&json!({"model":"alpha","messages":[{"role":"user","content":"test"}],"stream":true,"test_scenario":"long_stream"}))
        .send().await?;
    assert_eq!(response.status(), StatusCode::OK);
    let first = tokio::time::timeout(Duration::from_secs(5), response.chunk())
        .await??
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("alpha:"));
    let labels = [
        ("profile_id", "first"),
        ("endpoint", "/v1/chat/completions"),
    ];
    assert_eq!(
        metric_value(
            &fixture.metrics().await?,
            "locals_studio_inference_requests_in_flight",
            &labels
        ),
        1.0
    );
    // A listener restart drains this 30-second fixture stream. Each toggle is
    // bounded at five seconds and the original response must remain usable.
    fixture.set_metrics_enabled(false).await?;
    assert_eq!(
        fixture
            .client
            .get(format!("{}/metrics", fixture.base_url))
            .send()
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    fixture.set_metrics_enabled(true).await?;
    assert_eq!(
        metric_value(
            &fixture.metrics().await?,
            "locals_studio_inference_requests_in_flight",
            &labels
        ),
        1.0
    );
    let next = tokio::time::timeout(Duration::from_secs(5), response.chunk())
        .await??
        .unwrap();
    assert!(String::from_utf8_lossy(&next).contains("data:"));
    let after = fixture.studio.supervisor.list().await;
    assert_eq!(after[0].pid, before[0].pid);
    assert_eq!(after[0].port, before[0].port);
    assert_eq!(after[0].started_at, before[0].started_at);
    assert_eq!(after[0].state, "ready");
    drop(response);
    wait_no_leases(&fixture.studio).await;
    let cancelled = [
        ("profile_id", "first"),
        ("endpoint", "/v1/chat/completions"),
        ("outcome", "cancelled"),
    ];
    let body = wait_metric(
        &fixture,
        "locals_studio_inference_requests_total",
        &cancelled,
        1.0,
    )
    .await?;
    assert_eq!(
        metric_value(&body, "locals_studio_inference_requests_in_flight", &labels),
        0.0
    );
    assert_eq!(
        metric_value(
            &body,
            "locals_studio_inference_time_to_first_token_seconds_count",
            &labels
        ),
        1.0
    );
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prometheus_request_outcomes_and_ttft_cover_json_sse_ndjson_and_two_models() -> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture.start_workers().await;
    fixture.set_metrics_enabled(true).await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), fixture.post("alpha")).await?["worker"],
        "alpha"
    );
    for (model, endpoint, scenario, expected_status) in [
        (
            "locals/alpha",
            "/v1/chat/completions",
            "sse",
            StatusCode::OK,
        ),
        (
            "alpha-thinking",
            "/v1/messages",
            "anthropic",
            StatusCode::OK,
        ),
        ("alpha-thinking", "/api/chat", "ndjson", StatusCode::OK),
        (
            "alpha",
            "/v1/chat/completions",
            "inference_error",
            StatusCode::OK,
        ),
        (
            "alpha",
            "/v1/chat/completions",
            "http_error",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        ("alpha", "/v1/completions", "empty", StatusCode::NO_CONTENT),
        (
            "alpha-thinking",
            "/api/generate",
            "ndjson_error_no_lf",
            StatusCode::OK,
        ),
    ] {
        let response = fixture.client.post(format!("{}{endpoint}", fixture.base_url))
            .timeout(Duration::from_secs(5))
            .json(&json!({"model":model,"messages":[{"role":"user","content":"test"}],"max_tokens":16,"stream":scenario != "http_error","test_scenario":scenario}))
            .send().await?;
        assert_eq!(response.status(), expected_status, "{scenario}");
        let body = response.text().await?;
        assert_eq!(body.is_empty(), scenario == "empty");
        if scenario == "inference_error" {
            assert!(body.contains("fake inference failure"));
        }
        if scenario == "ndjson_error_no_lf" {
            assert!(body.contains("fake NDJSON final failure"));
            assert!(!body.ends_with('\n'));
        }
    }
    wait_no_leases(&fixture.studio).await;
    let body = fixture.metrics().await?;
    for (profile, endpoint, outcome, expected) in [
        ("first", "/v1/chat/completions", "success", 2.0),
        ("first", "/v1/chat/completions", "inference_error", 1.0),
        ("first", "/v1/chat/completions", "http_error", 1.0),
        ("second", "/v1/messages", "success", 1.0),
        ("second", "/api/chat", "success", 1.0),
        ("first", "/v1/completions", "success", 1.0),
        ("second", "/api/generate", "inference_error", 1.0),
    ] {
        let labels = [
            ("profile_id", profile),
            ("endpoint", endpoint),
            ("outcome", outcome),
        ];
        assert_eq!(
            metric_value(&body, "locals_studio_inference_requests_total", &labels),
            expected,
            "{labels:?}\n{body}"
        );
        assert_eq!(
            metric_value(
                &body,
                "locals_studio_inference_request_duration_seconds_count",
                &labels
            ),
            expected
        );
    }
    assert_eq!(
        metric_value(&body, "locals_studio_inference_requests_total", &[]),
        8.0,
        "scrapes and metadata GETs must not become inference requests"
    );
    assert_eq!(
        metric_value(
            &body,
            "locals_studio_inference_requests_total",
            &[("profile_id", "first"), ("endpoint", "/v1/messages")]
        ),
        0.0
    );
    assert_eq!(
        metric_value(
            &body,
            "locals_studio_inference_requests_total",
            &[
                ("profile_id", "second"),
                ("endpoint", "/v1/chat/completions")
            ]
        ),
        0.0
    );
    for (profile, endpoint) in [
        ("first", "/v1/chat/completions"),
        ("second", "/v1/messages"),
        ("second", "/api/chat"),
    ] {
        let labels = [("profile_id", profile), ("endpoint", endpoint)];
        assert_eq!(
            metric_value(
                &body,
                "locals_studio_inference_time_to_first_token_seconds_count",
                &labels
            ),
            1.0,
            "only actual streaming output records TTFT, once per request"
        );
        assert!(
            metric_value(
                &body,
                "locals_studio_inference_time_to_first_token_seconds_sum",
                &labels
            ) > 0.0
        );
        assert_eq!(
            metric_value(&body, "locals_studio_inference_requests_in_flight", &labels),
            0.0
        );
    }
    for (profile, endpoint) in [("first", "/v1/completions"), ("second", "/api/generate")] {
        assert_eq!(
            metric_value(
                &body,
                "locals_studio_inference_time_to_first_token_seconds_count",
                &[("profile_id", profile), ("endpoint", endpoint)]
            ),
            0.0,
            "empty responses and an unterminated error record do not contain a first token"
        );
    }
    for (profile, model_id, model_name, alias, requests, first_tokens) in [
        ("first", "model-first", "alpha", "alpha", 5.0, 1.0),
        (
            "second",
            "model-second",
            "alpha-thinking",
            "alpha-thinking",
            3.0,
            2.0,
        ),
    ] {
        let metadata = [
            ("profile_id", profile),
            ("model_id", model_id),
            ("model", model_name),
            ("alias", alias),
        ];
        assert_eq!(
            metric_value(&body, "locals_studio_worker_ready", &metadata),
            1.0
        );
        assert_eq!(
            metric_value(&body, "locals_studio_inference_requests_total", &metadata),
            requests
        );
        let mut histogram_metadata = metadata.to_vec();
        histogram_metadata.push(("le", "+Inf"));
        assert_eq!(
            metric_value(
                &body,
                "locals_studio_inference_request_duration_seconds_bucket",
                &histogram_metadata
            ),
            requests
        );
        assert_eq!(
            metric_value(
                &body,
                "locals_studio_inference_time_to_first_token_seconds_bucket",
                &histogram_metadata
            ),
            first_tokens
        );
    }
    for line in body
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if !line.contains("profile_id=\"") || line.contains("profile_id=\"unresolved\"") {
            for label in ["model_id=\"", "model=\"", "alias=\""] {
                assert!(
                    !line.contains(label),
                    "global and unresolved samples must not acquire model metadata: {line}"
                );
            }
        }
    }
    assert!(
        !body.contains("model=\"locals/alpha\""),
        "the request's decorated model string is not library metadata"
    );
    assert!(!body.contains("model=\"locals/canonical-first\""));
    for model in fixture.studio.store.list::<ModelRecord>("models")? {
        for path in [model.path.clone(), model.path.canonicalize()?] {
            assert!(
                !body.contains(path.to_string_lossy().as_ref()),
                "model directory paths must stay private"
            );
        }
    }

    let before_workers = fixture.studio.supervisor.list().await;
    let mut renamed = fixture
        .studio
        .store
        .get::<ModelRecord>("models", "model-first")?
        .unwrap();
    renamed.name = "Renamed alpha 4B".into();
    fixture.studio.store.set("models", &renamed.id, &renamed)?;
    let renamed_body = fixture.metrics().await?;
    let renamed_metadata = [
        ("profile_id", "first"),
        ("model_id", "model-first"),
        ("model", "Renamed alpha 4B"),
        ("alias", "alpha"),
    ];
    assert_eq!(
        metric_value(
            &renamed_body,
            "locals_studio_worker_ready",
            &renamed_metadata
        ),
        1.0
    );
    assert_eq!(
        metric_value(
            &renamed_body,
            "locals_studio_inference_requests_total",
            &renamed_metadata
        ),
        5.0,
        "renaming metadata does not reset the profile's counters"
    );
    let mut renamed_histogram_metadata = renamed_metadata.to_vec();
    renamed_histogram_metadata.push(("le", "+Inf"));
    assert_eq!(
        metric_value(
            &renamed_body,
            "locals_studio_inference_request_duration_seconds_bucket",
            &renamed_histogram_metadata
        ),
        5.0
    );
    assert_eq!(
        metric_value(
            &renamed_body,
            "locals_studio_inference_requests_total",
            &[("profile_id", "first"), ("model", "alpha")]
        ),
        0.0,
        "the old display name does not retain a second counter series"
    );
    assert_eq!(
        metric_value(&renamed_body, "locals_studio_inference_requests_total", &[]),
        8.0
    );
    assert_eq!(
        metric_value(
            &renamed_body,
            "locals_studio_inference_requests_total",
            &[
                ("profile_id", "second"),
                ("model_id", "model-second"),
                ("model", "alpha-thinking"),
                ("alias", "alpha-thinking")
            ]
        ),
        3.0
    );
    let after_workers = fixture.studio.supervisor.list().await;
    for before in before_workers {
        let after = after_workers
            .iter()
            .find(|worker| worker.profile_id == before.profile_id)
            .unwrap();
        assert_eq!(after.pid, before.pid);
        assert_eq!(after.port, before.port);
        assert_eq!(after.started_at, before.started_at);
    }
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prometheus_counts_upstream_transport_failure_and_downstream_disconnect_separately()
-> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture
        .start_worker("first", "alpha", "locals/canonical", &["locals/canonical"])
        .await;
    fixture.set_metrics_enabled(true).await?;
    let response = fixture
        .client
        .post(format!("{}/v1/chat/completions", fixture.base_url))
        .timeout(Duration::from_secs(5))
        .json(&json!({"model":"alpha","messages":[],"test_scenario":"transport_error"}))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(
        response.json::<Value>().await?["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    let mut response = fixture
        .client
        .post(format!("{}/v1/chat/completions", fixture.base_url))
        .timeout(Duration::from_secs(20))
        .json(&json!({"model":"alpha","messages":[],"stream":true,"test_scenario":"long_stream"}))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await??
            .is_some()
    );
    drop(response);
    wait_no_leases(&fixture.studio).await;
    let cancelled = [
        ("profile_id", "first"),
        ("endpoint", "/v1/chat/completions"),
        ("outcome", "cancelled"),
    ];
    let body = wait_metric(
        &fixture,
        "locals_studio_inference_requests_total",
        &cancelled,
        1.0,
    )
    .await?;
    for outcome in ["transport_error", "cancelled"] {
        let labels = [
            ("profile_id", "first"),
            ("endpoint", "/v1/chat/completions"),
            ("outcome", outcome),
        ];
        assert_eq!(
            metric_value(&body, "locals_studio_inference_requests_total", &labels),
            1.0
        );
        assert_eq!(
            metric_value(
                &body,
                "locals_studio_inference_request_duration_seconds_count",
                &labels
            ),
            1.0
        );
    }
    assert_eq!(
        metric_value(&body, "locals_studio_inference_requests_total", &[]),
        2.0
    );
    assert_eq!(
        metric_value(
            &body,
            "locals_studio_inference_time_to_first_token_seconds_count",
            &[
                ("profile_id", "first"),
                ("endpoint", "/v1/chat/completions")
            ]
        ),
        1.0
    );
    fixture.close().await;
    Ok(())
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
async fn model_context_metadata_tracks_the_exact_worker_and_variant_across_restart() -> Result<()> {
    let mut fixture = Fixture::new().await;
    // Two profiles advertise the same backend IDs with different actual limits.
    // The model record and configured maximum are both 1024 in this fixture.
    fixture
        .start_worker_with_contexts(
            "first",
            "alpha",
            "locals/shared",
            &[
                "locals/shared",
                "locals/shared-thinking",
                "mlx/shared",
                "mlx/shared-thinking",
            ],
            json!({
                "locals/shared":{"context_length":131072,"max_model_len":131072},
                "locals/shared-thinking":{"context_length":65536,"max_model_len":65536},
                "mlx/shared":{"context_length":true,"max_model_len":32768},
                "mlx/shared-thinking":{"context_length":0,"max_model_len":"16384"},
                "locals/not-advertised":{"context_length":999999,"max_model_len":999999}
            }),
        )
        .await;
    fixture
        .start_worker_with_contexts(
            "second",
            "beta",
            "locals/shared",
            &["locals/shared", "locals/shared-thinking"],
            json!({"locals/shared":{"context_length":262144,"max_model_len":262144}}),
        )
        .await;
    let discovery_reads = || {
        read_records(&fixture.request_log)
            .into_iter()
            .filter(|event| event["event"] == "probe" && event["path"] == "/v1/models")
            .count()
    };
    let before = discovery_reads();
    for prefix in ["/v1/models", "/models"] {
        let list: Value = fixture
            .client
            .get(format!("{}{prefix}", fixture.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let models = list["data"].as_array().unwrap();
        assert_eq!(models.len(), 6);
        for (id, context_length, max_model_len) in [
            ("alpha", Some(131072), Some(131072)),
            ("alpha-thinking", Some(65536), Some(65536)),
            ("mlx/alpha", None, Some(32768)),
            ("mlx/alpha-thinking", None, None),
            ("beta", Some(262144), Some(262144)),
            ("beta-thinking", None, None),
        ] {
            let entry = models.iter().find(|model| model["id"] == id).unwrap();
            assert_eq!(
                entry.get("context_length"),
                context_length.map(Value::from).as_ref(),
                "{id}"
            );
            assert_eq!(
                entry.get("max_model_len"),
                max_model_len.map(Value::from).as_ref(),
                "{id}"
            );
            let detail: Value = fixture
                .client
                .get(format!("{}{prefix}/{id}", fixture.base_url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            assert_eq!(
                &detail, entry,
                "list and detail must expose the same actual worker metadata for {id}"
            );
        }
    }
    assert_eq!(
        discovery_reads(),
        before,
        "model discovery endpoints reuse readiness metadata without polling workers"
    );
    fixture
        .studio
        .handle("server.stop", json!({"id":"first"}))
        .await?;
    let stopped: Value = fixture
        .client
        .get(format!("{}/v1/models", fixture.base_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        stopped["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|model| model["id"] != "alpha")
    );
    fixture
        .start_worker_with_contexts(
            "first",
            "alpha",
            "locals/shared",
            &["locals/shared"],
            json!({"locals/shared":{"context_length":16384}}),
        )
        .await;
    let restarted: Value = fixture
        .client
        .get(format!("{}/v1/models", fixture.base_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let models = restarted["data"].as_array().unwrap();
    assert_eq!(models.len(), 3);
    let alpha = models.iter().find(|model| model["id"] == "alpha").unwrap();
    assert_eq!(alpha["context_length"], 16384);
    assert!(
        alpha.get("max_model_len").is_none(),
        "a restart must not retain the old worker's second context field"
    );
    let beta = models.iter().find(|model| model["id"] == "beta").unwrap();
    assert_eq!(
        beta["context_length"], 262144,
        "the other profile's metadata stays independent"
    );
    for alias in ["alpha-thinking", "mlx/alpha", "mlx/alpha-thinking"] {
        assert_eq!(
            fixture
                .client
                .get(format!("{}/v1/models/{alias}", fixture.base_url))
                .send()
                .await?
                .status(),
            StatusCode::NOT_FOUND
        );
    }
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
async fn conversations_on_one_worker_keep_histories_events_and_saved_replies_isolated() -> Result<()>
{
    async fn completions(
        events: &mut tokio::sync::broadcast::Receiver<Value>,
        requests: &[(String, String, String)],
    ) -> Vec<Value> {
        let mut messages = vec![None; requests.len()];
        let mut content = vec![String::new(); requests.len()];
        let mut reasoning = vec![String::new(); requests.len()];
        tokio::time::timeout(Duration::from_secs(8), async {
            while messages.iter().any(Option::is_none) {
                let event = events.recv().await.unwrap();
                if !matches!(
                    event["type"].as_str(),
                    Some("chat.delta" | "chat.done" | "chat.error")
                ) {
                    continue;
                }
                let index = requests
                    .iter()
                    .position(|(_, request, _)| event["data"]["request_id"] == *request)
                    .unwrap_or_else(|| {
                        panic!("an event belongs to an unexpected request: {event}")
                    });
                let (conversation, _, marker) = &requests[index];
                assert_eq!(event["data"]["conversation_id"], *conversation);
                assert!(
                    messages[index].is_none(),
                    "no deltas or duplicate completions after a request finished"
                );
                match event["type"].as_str() {
                    Some("chat.delta") => {
                        content[index].push_str(event["data"]["delta"].as_str().unwrap());
                        reasoning[index]
                            .push_str(event["data"]["reasoning_delta"].as_str().unwrap());
                    }
                    Some("chat.done") => {
                        let message = event["data"]["message"].clone();
                        assert_eq!(content[index], format!("Reply for {marker}"));
                        assert_eq!(reasoning[index], format!("Reasoning for {marker}"));
                        assert_eq!(message["content"], content[index]);
                        assert_eq!(message["reasoning_content"], reasoning[index]);
                        assert_eq!(message["finish_reason"], "stop");
                        assert!(message["error"].is_null());
                        messages[index] = Some(message);
                    }
                    _ => panic!("unexpected generation error: {event}"),
                }
            }
        })
        .await
        .expect("both isolated requests should finish on the same fake worker");
        messages.into_iter().map(Option::unwrap).collect()
    }

    let mut fixture = Fixture::new().await;
    fixture
        .start_worker(
            "first",
            "alpha",
            "locals/canonical-first",
            &["locals/canonical-first"],
        )
        .await;
    let ids = ["isolation-a", "isolation-b"];
    let histories: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!([
                {"role":"system","content":format!("SYSTEM_SECRET_{id}")},
                {"role":"user","content":format!("EARLIER_USER_{id}")},
                {"role":"assistant","content":format!("EARLIER_ASSISTANT_{id}")},
                {"role":"user","content":format!("TURN_ONE_{id}")}
            ])
        })
        .collect();
    for (id, history) in ids.iter().zip(&histories) {
        fixture
            .studio
            .handle(
                "conversations.save",
                json!({"conversation":{
                    "id":id,"title":id,"messages":history
                }}),
            )
            .await?;
    }
    let mut events = fixture.studio.events.subscribe();
    let parameters = json!({
        "test_scenario":"chat_isolation","test_pair_barrier":true,
        "messages":[{"role":"system","content":"PARAMETERS_MUST_NOT_OVERRIDE_CHAT_HISTORY"}]
    });
    let (first, second) = tokio::join!(
        fixture.studio.handle("chat.start", json!({"conversation_id":ids[0],"model":"alpha","messages":histories[0],"parameters":parameters})),
        fixture.studio.handle("chat.start", json!({"conversation_id":ids[1],"model":"alpha","messages":histories[1],"parameters":parameters}))
    );
    let started = [first?, second?];
    assert_ne!(started[0]["request_id"], started[1]["request_id"]);
    let requests: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            (
                id.to_string(),
                started[index]["request_id"].as_str().unwrap().to_owned(),
                format!("TURN_ONE_{id}"),
            )
        })
        .collect();
    let completed = completions(&mut events, &requests).await;
    let mut expected_outbound = histories.clone();
    for (index, id) in ids.iter().enumerate() {
        let saved = fixture
            .studio
            .store
            .get::<Value>("conversations", id)?
            .unwrap();
        let mut expected = histories[index].as_array().unwrap().clone();
        expected.push(completed[index].clone());
        assert_eq!(saved["messages"], json!(expected));
    }

    // Resume each conversation in sequence using only its persisted history.
    for id in ids {
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.studio.chat.is_active(id).await {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let mut saved = fixture
            .studio
            .store
            .get::<Value>("conversations", id)?
            .unwrap();
        let marker = format!("TURN_TWO_{id}");
        saved["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":marker}));
        let history = saved["messages"].clone();
        expected_outbound.push(history.clone());
        fixture
            .studio
            .handle("conversations.save", json!({"conversation":saved}))
            .await?;
        let started = fixture.studio.handle("chat.start", json!({
            "conversation_id":id,"model":"alpha","parameters":{"test_scenario":"chat_isolation"}
        })).await?;
        let request = [(
            id.to_owned(),
            started["request_id"].as_str().unwrap().to_owned(),
            marker,
        )];
        let message = completions(&mut events, &request).await.remove(0);
        let reloaded = Store::open(&fixture.studio.paths.database_path)?
            .get::<Value>("conversations", id)?
            .unwrap();
        let mut expected = history.as_array().unwrap().clone();
        expected.push(message);
        assert_eq!(reloaded["messages"], json!(expected));
    }
    let outbound: Vec<_> = read_records(&fixture.request_log)
        .into_iter()
        .filter(|event| event["event"] == "chat_history")
        .collect();
    assert_eq!(outbound.len(), expected_outbound.len());
    for history in expected_outbound {
        let received = outbound.iter().find(|event| event["messages"] == history)
            .expect("the worker must receive exactly one conversation's history, without any other chat's system prompt or messages");
        assert_eq!(received["model"], "locals/canonical-first");
    }
    wait_no_leases(&fixture.studio).await;
    fixture.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_preserves_long_fragmented_replies_and_finish_reasons_after_usage_and_reload()
-> Result<()> {
    let mut fixture = Fixture::new().await;
    fixture
        .start_worker(
            "first",
            "alpha",
            "locals/canonical-first",
            &["locals/canonical-first"],
        )
        .await;
    let reasoning = "Analyse 🌱 avant la réponse.";
    let long_answer: String = (0..12)
        .map(|index| {
            format!(
                "Section {index}: {}\n",
                "Réponse 🌍 — texte complet. ".repeat(128)
            )
        })
        .collect();
    for (scenario, expected_content, expected_reason, parameters, expected_limit) in [
        (
            "chat_length",
            long_answer.as_str(),
            "length",
            json!({"test_scenario":"chat_length","max_tokens":1024,"max_completion_tokens":4096}),
            json!(4096),
        ),
        (
            "chat_stop",
            "Terminé normalement.",
            "stop",
            json!({"test_scenario":"chat_stop"}),
            Value::Null,
        ),
    ] {
        fixture.studio.handle("conversations.save", json!({"conversation":{
            "id":scenario,"title":"Finish reason test","messages":[{"role":"user","content":"hello"}]
        }})).await?;
        let mut events = fixture.studio.events.subscribe();
        let started = fixture
            .studio
            .handle(
                "chat.start",
                json!({
                    "conversation_id":scenario,"model":"alpha","parameters":parameters
                }),
            )
            .await?;
        let request_id = &started["request_id"];
        let mut streamed_content = String::new();
        let mut streamed_reasoning = String::new();
        let done = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if event["data"]["request_id"] != *request_id {
                    continue;
                }
                match event["type"].as_str() {
                    Some("chat.delta") => {
                        streamed_content.push_str(event["data"]["delta"].as_str().unwrap());
                        streamed_reasoning
                            .push_str(event["data"]["reasoning_delta"].as_str().unwrap());
                    }
                    Some("chat.done") => break event,
                    Some("chat.error") => panic!("unexpected generation error: {event}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("the complete fake reply should finish promptly");
        assert_eq!(streamed_content, expected_content);
        assert_eq!(streamed_reasoning, reasoning);
        let message = &done["data"]["message"];
        assert_eq!(message["content"], expected_content);
        assert_eq!(message["reasoning_content"], reasoning);
        assert_eq!(message["finish_reason"], expected_reason);
        assert_eq!(message["output_token_limit"], expected_limit);
        assert_eq!(message["usage"]["prompt_tokens"], 37);
        assert_eq!(
            message["usage"]["completion_tokens"],
            if scenario == "chat_length" { 4096 } else { 20 }
        );
        assert_eq!(message["cancelled"], false);
        assert!(message["error"].is_null());
        let reloaded = Store::open(&fixture.studio.paths.database_path)?
            .get::<Value>("conversations", scenario)?
            .unwrap();
        assert_eq!(
            reloaded["messages"][1], *message,
            "the full emitted message survives reopening the store"
        );
    }
    wait_no_leases(&fixture.studio).await;
    fixture.close().await;
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
    assert!(done["data"]["message"]["finish_reason"].is_null());
    assert!(done["data"]["message"]["output_token_limit"].is_null());
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
    assert!(
        messages[1]["finish_reason"].is_null(),
        "cancellation must not invent a length finish reason"
    );
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
