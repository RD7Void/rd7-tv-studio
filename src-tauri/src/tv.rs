// Rust / tv.rs / Tauri v2 native backend / LG webOS over SSH, SSAP and CDP
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::os::windows::process::CommandExt;
use std::{
    fs,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};
type R<T> = Result<T, String>;
type Ws = WebSocket<MaybeTlsStream<TcpStream>>;
const APP: &str = "io.freemytv";
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub model: String,
}
#[derive(Clone, Serialize, Deserialize, Default)]
struct Secrets {
    private_key: String,
    client_key: String,
}
pub struct Backend {
    dir: PathBuf,
    profiles: Vec<Profile>,
    current: Option<Profile>,
    secret: Secrets,
    socket: Option<Ws>,
    pointer: Option<Ws>,
    seq: u64,
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn cmd(name: &str) -> Command {
    let mut c = Command::new(name);
    c.creation_flags(0x08000000);
    c
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() < 129
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn http() -> R<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(err)
}
fn entry(id: &str) -> R<keyring::Entry> {
    keyring::Entry::new("RD7 TV Studio", id).map_err(err)
}
fn load_secret(id: &str) -> R<Secrets> {
    let raw = entry(id)?.get_password().map_err(err)?;
    let metadata: Value = serde_json::from_str(&raw).map_err(err)?;
    if let Some(count) = metadata["chunks"].as_u64() {
        if count > 64 {
            return Err("Credencial salva inválida".into());
        }
        let mut data = String::new();
        for i in 0..count {
            data.push_str(
                &entry(&format!("{id}:secret:{i}"))?
                    .get_password()
                    .map_err(err)?,
            );
        }
        serde_json::from_str(&data).map_err(err)
    } else {
        serde_json::from_str(&raw).map_err(err)
    }
}
fn store_secret(id: &str, secret: &Secrets) -> R<()> {
    let raw = serde_json::to_string(secret).map_err(err)?;
    let chars: Vec<char> = raw.chars().collect();
    let parts: Vec<String> = chars.chunks(500).map(|c| c.iter().collect()).collect();
    if parts.len() > 64 {
        return Err("Credencial maior que o limite permitido".into());
    }
    for (i, part) in parts.iter().enumerate() {
        entry(&format!("{id}:secret:{i}"))?
            .set_password(part)
            .map_err(err)?;
    }
    entry(id)?
        .set_password(&json!({"format":1,"chunks":parts.len()}).to_string())
        .map_err(err)
}
fn delete_secret(id: &str) -> R<()> {
    if let Ok(raw) = entry(id)?.get_password() {
        if let Ok(metadata) = serde_json::from_str::<Value>(&raw) {
            for i in 0..metadata["chunks"].as_u64().unwrap_or(0).min(64) {
                let _ = entry(&format!("{id}:secret:{i}"))?.delete_credential();
            }
        }
    }
    let _ = entry(id)?.delete_credential();
    Ok(())
}
fn ws_open(url: &str, timeout: u64) -> R<Ws> {
    let parsed = reqwest::Url::parse(url).map_err(err)?;
    let host = parsed.host_str().ok_or("Endereço inválido")?;
    let ip: Ipv4Addr = host
        .parse()
        .map_err(|_| "A TV precisa ter um endereço IPv4 válido".to_string())?;
    let tcp = TcpStream::connect_timeout(
        &SocketAddr::from((ip, parsed.port_or_known_default().unwrap_or(80))),
        Duration::from_secs(5),
    )
    .map_err(err)?;
    tcp.set_nodelay(true).map_err(err)?;
    tcp.set_read_timeout(Some(Duration::from_secs(timeout)))
        .map_err(err)?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(err)?;
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(err)?;
    tungstenite::client_tls_with_config(
        url,
        tcp,
        None,
        Some(tungstenite::Connector::NativeTls(tls)),
    )
    .map(|x| x.0)
    .map_err(err)
}
fn ws_json(ws: &mut Ws) -> R<Value> {
    loop {
        match ws.read().map_err(err)? {
            Message::Text(s) => return serde_json::from_str(s.as_str()).map_err(err),
            Message::Close(_) => return Err("A TV fechou a conexão".into()),
            _ => {}
        }
    }
}
fn send(ws: &mut Ws, v: Value) -> R<()> {
    ws.send(Message::Text(v.to_string().into())).map_err(err)
}
impl Backend {
    pub fn new(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).ok();
        let profiles = fs::read(dir.join("profiles.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let _ = fs::remove_file(dir.join("session/devkey"));
        Self {
            dir,
            profiles,
            current: None,
            secret: Secrets::default(),
            socket: None,
            pointer: None,
            seq: 0,
        }
    }
    fn profile(&self) -> R<&Profile> {
        self.current
            .as_ref()
            .ok_or("Conecte uma TV primeiro".into())
    }
    fn save(&self) -> R<()> {
        let p = self.profile()?;
        store_secret(&p.id, &self.secret)?;
        let tmp = self.dir.join("profiles.new");
        fs::write(
            &tmp,
            serde_json::to_vec_pretty(&self.profiles).map_err(err)?,
        )
        .map_err(err)?;
        fs::rename(tmp, self.dir.join("profiles.json")).map_err(err)
    }
    fn keyfile(&self) -> PathBuf {
        self.dir.join("session/devkey")
    }
    fn secure_session(&self) -> R<()> {
        let dir = self.dir.join("session");
        fs::create_dir_all(&dir).map_err(err)?;
        let out = cmd("whoami.exe")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .map_err(err)?;
        let raw = String::from_utf8_lossy(&out.stdout);
        let sid = raw
            .split('"')
            .find(|s| s.starts_with("S-1-"))
            .ok_or("Não foi possível identificar a conta Windows")?;
        let out = cmd("icacls.exe")
            .arg(&dir)
            .args(["/inheritance:r", "/grant:r", &format!("*{sid}:(OI)(CI)F")])
            .output()
            .map_err(err)?;
        if !out.status.success() {
            return Err("Não foi possível proteger a chave SSH no Windows".into());
        }
        Ok(())
    }
    fn write_key(&self) -> R<()> {
        self.secure_session()?;
        fs::write(self.keyfile(), &self.secret.private_key).map_err(err)
    }
    fn ssh(&self, remote: &str, input: Option<&[u8]>) -> R<String> {
        let p = self.profile()?;
        let mut c = cmd("ssh.exe");
        c.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=6",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "HostKeyAlgorithms=+ssh-rsa",
            "-o",
            "PubkeyAcceptedKeyTypes=+ssh-rsa",
        ])
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            self.dir.join("known_hosts").display()
        ))
        .arg("-p")
        .arg(p.port.to_string())
        .arg("-i")
        .arg(self.keyfile())
        .arg(format!("prisoner@{}", p.ip))
        .arg(remote)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        if input.is_some() {
            c.stdin(Stdio::piped());
        }
        let mut child = c
            .spawn()
            .map_err(|e| format!("OpenSSH não encontrado: {e}"))?;
        if let Some(bytes) = input {
            child
                .stdin
                .take()
                .ok_or("Falha de upload")?
                .write_all(bytes)
                .map_err(err)?;
        }
        let output = child.wait_with_output().map_err(err)?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() && stdout.trim().is_empty() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(stdout)
    }
    fn luna(&self, uri: &str, payload: Value, replies: u8, seconds: u8) -> R<String> {
        let body = quote(&payload.to_string());
        self.ssh(&format!("if timeout -t 1 true 2>/dev/null; then timeout -t {seconds} luna-send-pub -n {replies} {uri} {body}; else timeout {seconds} luna-send-pub -n {replies} {uri} {body}; fi"),None)
    }
    fn connect(&mut self, payload: &Value) -> R<Value> {
        let ip = payload["ip"].as_str().unwrap_or("").trim();
        let _: Ipv4Addr = ip
            .parse()
            .map_err(|_| "Digite o IPv4 exibido no Developer Mode")?;
        let id = ip.replace('.', "_");
        let name = payload["name"].as_str().unwrap_or("LG webOS").trim();
        let profile = Profile {
            id: id.clone(),
            name: if name.is_empty() {
                "LG webOS".into()
            } else {
                name.into()
            },
            ip: ip.into(),
            port: payload["port"]
                .as_u64()
                .unwrap_or(9922)
                .try_into()
                .map_err(|_| "Porta inválida")?,
            model: String::new(),
        };
        self.socket = None;
        self.pointer = None;
        self.current = Some(profile);
        self.secret = load_secret(&id).unwrap_or_default();
        let pass = payload["passphrase"].as_str().unwrap_or("").trim();
        if self.secret.private_key.is_empty() || !pass.is_empty() {
            if pass.len() != 6 {
                return Err(
                    "No primeiro acesso, ative Key Server e informe a passphrase de 6 caracteres"
                        .into(),
                );
            }
            let key = http()?
                .get(format!("http://{ip}:9991/webos_rsa"))
                .send()
                .map_err(|_| "Key Server indisponível. Ative-o no app Developer Mode.")?
                .error_for_status()
                .map_err(err)?
                .bytes()
                .map_err(err)?;
            if key.len() > 32768 {
                return Err("Chave SSH inválida".into());
            }
            self.secure_session()?;
            fs::write(self.keyfile(), key).map_err(err)?;
            let out = cmd("ssh-keygen.exe")
                .args(["-p", "-P", pass, "-N", ""])
                .arg("-f")
                .arg(self.keyfile())
                .output()
                .map_err(err)?;
            if !out.status.success() {
                let _ = fs::remove_file(self.keyfile());
                return Err("Passphrase incorreta ou chave incompatível".into());
            }
            self.secret.private_key = fs::read_to_string(self.keyfile()).map_err(err)?;
        } else {
            self.write_key()?;
        }
        self.ssh("printf RD7_CONNECTED", None).and_then(|s| {
            if s.contains("RD7_CONNECTED") {
                Ok(())
            } else {
                Err("Não foi possível validar o SSH".into())
            }
        })?;
        let info = self
            .luna(
                "luna://com.webos.service.tv.systemproperty/getSystemInfo",
                json!({"keys":["modelName","firmwareVersion","UHD"]}),
                1,
                5,
            )
            .unwrap_or_default();
        let info = info
            .lines()
            .find_map(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(json!({}));
        self.current.as_mut().unwrap().model =
            info["modelName"].as_str().unwrap_or("LG webOS").into();
        let p = self.profile()?.clone();
        self.profiles.retain(|x| x.id != p.id);
        self.profiles.push(p.clone());
        self.save()?;
        Ok(json!({"profile":p,"info":info,"paired":!self.secret.client_key.is_empty()}))
    }
    fn pair(&mut self, force: bool) -> R<Value> {
        let ip = self.profile()?.ip.clone();
        let mut ws = ws_open(&format!("wss://{ip}:3001"), if force { 90 } else { 10 })?;
        let mut payload = json!({"forcePairing":force,"pairingType":"PROMPT","manifest":{"manifestVersion":1,"appVersion":"1.0","appId":"com.rd7.tvstudio","localizedAppNames":{"":"RD7 TV Studio"},"permissions":["LAUNCH","LAUNCH_WEBAPP","CLOSE","CONTROL_AUDIO","CONTROL_POWER","CONTROL_INPUT_TV","CONTROL_INPUT_MEDIA_PLAYBACK","CONTROL_MOUSE_AND_KEYBOARD","CONTROL_INPUT_TEXT","READ_APP_STATUS","READ_RUNNING_APPS","READ_INSTALLED_APPS","READ_INPUT_DEVICE_LIST","READ_CURRENT_CHANNEL","READ_POWER_STATE","READ_NETWORK_STATE","READ_SETTINGS","CONTROL_TV_SCREEN","TEST_PROTECTED","READ_TV_CURRENT_TIME"]}});
        if !self.secret.client_key.is_empty() && !force {
            payload["client-key"] = json!(self.secret.client_key);
        }
        send(
            &mut ws,
            json!({"type":"register","id":"register","payload":payload}),
        )?;
        for _ in 0..20 {
            let m = ws_json(&mut ws)?;
            if m["type"] == "registered" {
                self.secret.client_key = m["payload"]["client-key"]
                    .as_str()
                    .ok_or("Pareamento sem chave")?
                    .into();
                self.socket = Some(ws);
                self.save()?;
                return Ok(json!({"paired":true}));
            }
            if m["type"] == "error" {
                return Err(format!("Pareamento recusado: {}", m["error"]));
            }
        }
        Err("Pareamento não confirmado. Permita RD7 TV Studio na TV.".into())
    }
    fn ssap(&mut self, uri: &str, payload: Value) -> R<Value> {
        if self.secret.client_key.is_empty() {
            return Err("Clique em Parear controle e confirme a permissão na TV".into());
        }
        if self.socket.is_none() {
            self.pair(false)?;
        }
        self.seq += 1;
        let id = format!("rd7-{}", self.seq);
        let result = (|| {
            let ws = self.socket.as_mut().unwrap();
            send(
                ws,
                json!({"type":"request","id":id,"uri":format!("ssap://{uri}"),"payload":payload}),
            )?;
            for _ in 0..40 {
                let m = ws_json(ws)?;
                if m["id"] == id {
                    if m["type"] == "error" || m["payload"]["returnValue"] == false {
                        return Err(m["error"]
                            .as_str()
                            .or(m["payload"]["errorText"].as_str())
                            .unwrap_or("Operação negada pela TV")
                            .into());
                    }
                    return Ok(m["payload"].clone());
                }
            }
            Err("Sem resposta da TV".into())
        })();
        if result.is_err() {
            self.socket = None;
            self.pointer = None;
        }
        result
    }
    fn button(&mut self, name: &str) -> R<Value> {
        if name == "HOME" {
            let out = self.luna(
                "luna://com.webos.applicationManager/launch",
                json!({"id":APP}),
                1,
                5,
            )?;
            if !out.contains("\"returnValue\":true") {
                return Err(out);
            }
            return Ok(json!({"sent":true}));
        }
        const KEYS: &[&str] = &[
            "UP",
            "DOWN",
            "LEFT",
            "RIGHT",
            "ENTER",
            "BACK",
            "HOME",
            "MENU",
            "EXIT",
            "INFO",
            "MUTE",
            "VOLUMEUP",
            "VOLUMEDOWN",
            "CHANNELUP",
            "CHANNELDOWN",
            "PLAY",
            "PAUSE",
            "STOP",
            "RED",
            "GREEN",
            "YELLOW",
            "BLUE",
            "0",
            "1",
            "2",
            "3",
            "4",
            "5",
            "6",
            "7",
            "8",
            "9",
        ];
        if !KEYS.contains(&name) {
            return Err("Botão inválido".into());
        }
        if self.pointer.is_none() {
            let v = self.ssap(
                "com.webos.service.networkinput/getPointerInputSocket",
                json!({}),
            )?;
            let url = v["socketPath"].as_str().ok_or("Controle indisponível")?;
            self.validate_url(url)?;
            self.pointer = Some(ws_open(url, 8)?);
        }
        self.pointer
            .as_mut()
            .unwrap()
            .send(Message::Text(
                format!("type:button\nname:{name}\n\n").into(),
            ))
            .map_err(|e| {
                self.pointer = None;
                err(e)
            })?;
        Ok(json!({"sent":true}))
    }
    fn validate_url(&self, url: &str) -> R<reqwest::Url> {
        let parsed = reqwest::Url::parse(url).map_err(err)?;
        if !["http", "https", "ws", "wss"].contains(&parsed.scheme())
            || parsed.host_str() != Some(self.profile()?.ip.as_str())
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err("A TV retornou um endereço externo ou inválido".into());
        }
        Ok(parsed)
    }
    fn cdp(&self, method: &str, params: Value) -> R<Value> {
        let ip = &self.profile()?.ip;
        let list: Value = http()?
            .get(format!("http://{ip}:9998/json/list"))
            .send()
            .map_err(|_| "Abra o RD7 TV na TV para usar esta função")?
            .json()
            .map_err(err)?;
        let target = list
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|x| x["url"].as_str().unwrap_or("").contains("/io.freemytv/"))
            })
            .ok_or("Abra o RD7 TV na TV para usar esta função")?;
        let url = target["webSocketDebuggerUrl"]
            .as_str()
            .ok_or("Inspetor indisponível")?;
        self.validate_url(url)?;
        let mut ws = ws_open(url, 12)?;
        send(&mut ws, json!({"id":1,"method":method,"params":params}))?;
        for _ in 0..60 {
            let m = ws_json(&mut ws)?;
            if m["id"] == 1 {
                if !m["error"].is_null() {
                    return Err(m["error"].to_string());
                }
                let _ = ws.close(None);
                return Ok(m["result"].clone());
            }
        }
        Err("Sem resposta do RD7 TV".into())
    }
    fn screenshot(&mut self) -> R<Value> {
        let mut reason = String::new();
        if !self.secret.client_key.is_empty() {
            match self.ssap("tv/executeOneShot", json!({})) {
                Ok(result) => {
                    if let Some(uri) = result["imageUri"].as_str() {
                        self.validate_url(uri)?;
                        let bytes = http()?
                            .get(uri)
                            .send()
                            .map_err(err)?
                            .error_for_status()
                            .map_err(err)?
                            .bytes()
                            .map_err(err)?;
                        if bytes.len() > 12_000_000 {
                            return Err("Captura maior que o limite".into());
                        }
                        image::load_from_memory(&bytes)
                            .map_err(|_| "A captura não é uma imagem válida")?;
                        let mime = if bytes.starts_with(b"\x89PNG") {
                            "image/png"
                        } else {
                            "image/jpeg"
                        };
                        return Ok(
                            json!({"image":format!("data:{mime};base64,{}",B64.encode(bytes)),"source":"Tela da TV","limited":false,"time":now()}),
                        );
                    }
                    reason = "A TV não retornou uma captura".into();
                }
                Err(e) => reason = e,
            }
        }
        let shot = self
            .cdp(
                "Page.captureScreenshot",
                json!({"format":"jpeg","quality":72}),
            )
            .map_err(|e| format!("Captura da TV: {reason}. Prévia RD7 TV: {e}"))?;
        Ok(
            json!({"image":format!("data:image/jpeg;base64,{}",shot["data"].as_str().ok_or("Captura vazia")?),"source":"Prévia do RD7 TV","limited":true,"time":now()}),
        )
    }
    fn install(&self, bytes: &[u8]) -> R<Value> {
        let id = package_id(bytes)?;
        self.ssh(
            "mkdir -p /media/developer/temp && cat > /media/developer/temp/rd7-studio-install.ipk",
            Some(bytes),
        )?;
        let out=self.luna("luna://com.webos.appInstallService/dev/install",json!({"id":id,"ipkUrl":"/media/developer/temp/rd7-studio-install.ipk","subscribe":true}),40,60)?;
        let installed = out
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|v| v["details"]["state"] == "installed");
        if !installed {
            return Err(format!(
                "A TV não confirmou a instalação. {}",
                out.chars()
                    .rev()
                    .take(1600)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
            ));
        }
        let _ = self.ssh("rm -f /media/developer/temp/rd7-studio-install.ipk", None);
        Ok(json!({"installed":true,"id":id}))
    }
    fn background(&self, settings: Value) -> R<Value> {
        let images = settings["images"]
            .as_array()
            .ok_or("Selecione ao menos uma imagem")?;
        if images.len() > 8 {
            return Err("Use até 8 imagens no slideshow".into());
        }
        if settings.to_string().len() > 3_200_000 {
            return Err(
                "As imagens ultrapassam o limite de armazenamento da TV. Selecione menos imagens."
                    .into(),
            );
        }
        for image in images {
            if !image["data"]
                .as_str()
                .unwrap_or("")
                .starts_with("data:image/jpeg;base64,")
            {
                return Err("Formato de imagem inválido".into());
            }
        }
        self.luna(
            "luna://com.webos.applicationManager/launch",
            json!({"id":APP}),
            1,
            5,
        )?;
        let expression=format!("JSON.stringify(window.RD7Background ? window.RD7Background.set({}) : {{error:'Atualize o RD7 TV pelo painel antes de personalizar'}})",settings);
        let mut last = String::new();
        for _ in 0..8 {
            match self.cdp(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true}),
            ) {
                Ok(v) => {
                    let raw = v["result"]["value"]
                        .as_str()
                        .ok_or("O RD7 TV ainda não carregou")?;
                    let res: Value = serde_json::from_str(raw).map_err(err)?;
                    if let Some(e) = res["error"].as_str() {
                        return Err(e.into());
                    }
                    return Ok(res);
                }
                Err(e) => last = e,
            }
            std::thread::sleep(Duration::from_millis(650));
        }
        Err(last)
    }
    pub fn dispatch(&mut self, action: &str, p: Value) -> R<Value> {
        match action {
            "profiles" => Ok(json!({"profiles":self.profiles,"current":self.current})),
            "connect" => self.connect(&p),
            "disconnect" => {
                self.socket = None;
                self.pointer = None;
                self.current = None;
                self.secret = Secrets::default();
                let _ = fs::remove_file(self.keyfile());
                Ok(json!({"disconnected":true}))
            }
            "forget" => {
                let id = p["id"].as_str().ok_or("Perfil inválido")?;
                if !self.profiles.iter().any(|x| x.id == id) {
                    return Err("Perfil não encontrado".into());
                }
                delete_secret(id)?;
                self.profiles.retain(|x| x.id != id);
                fs::write(
                    self.dir.join("profiles.json"),
                    serde_json::to_vec(&self.profiles).map_err(err)?,
                )
                .map_err(err)?;
                if self.current.as_ref().map(|x| x.id.as_str()) == Some(id) {
                    self.current = None;
                    self.socket = None;
                    self.pointer = None;
                    self.secret = Secrets::default();
                    let _ = fs::remove_file(self.keyfile());
                }
                Ok(json!({"forgotten":true}))
            }
            "pair" => self.pair(self.secret.client_key.is_empty()),
            "button" => self.button(p["name"].as_str().unwrap_or("")),
            "apps" => self.ssap("com.webos.applicationManager/listLaunchPoints", json!({})),
            "inputs" => self.ssap("tv/getExternalInputList", json!({})),
            "launch" => {
                let id = p["id"].as_str().ok_or("App inválido")?;
                if !valid_id(id) {
                    return Err("App inválido".into());
                }
                let out = self.luna(
                    "luna://com.webos.applicationManager/launch",
                    json!({"id":id}),
                    1,
                    5,
                )?;
                if !out.contains("\"returnValue\":true") {
                    return Err(out);
                }
                Ok(json!({"launched":true}))
            }
            "volume" => {
                let volume = p["value"].as_u64().ok_or("Volume inválido")?.min(100);
                self.ssap("audio/setVolume", json!({"volume":volume}))
            }
            "mute" => self.ssap(
                "audio/setMute",
                json!({"mute":p["value"].as_bool().unwrap_or(true)}),
            ),
            "text" => {
                let text = p["text"].as_str().ok_or("Texto vazio")?;
                if text.len() > 4000 {
                    return Err("Texto muito longo".into());
                }
                self.ssap(
                    "com.webos.service.ime/insertText",
                    json!({"text":text,"replace":false}),
                )
            }
            "input" => {
                let id = p["id"].as_str().ok_or("Entrada inválida")?;
                if !valid_id(id) {
                    return Err("Entrada inválida".into());
                }
                self.ssap("tv/switchInput", json!({"inputId":id}))
            }
            "power_off" => self.ssap("system/turnOff", json!({})),
            "screenshot" => self.screenshot(),
            "status" => {
                let raw = self.ssh("free -m; df -h /media/developer; cat /proc/uptime", None)?;
                Ok(
                    json!({"profile":self.profile()?,"raw":raw,"paired":!self.secret.client_key.is_empty()}),
                )
            }
            "install" => {
                let path = rfd::FileDialog::new()
                    .add_filter("Pacote webOS", &["ipk"])
                    .pick_file();
                match path {
                    Some(path) => {
                        let data = fs::read(path).map_err(err)?;
                        if data.len() > 200_000_000 {
                            return Err("IPK maior que 200 MB".into());
                        }
                        self.install(&data)
                    }
                    None => Ok(json!({"cancelled":true})),
                }
            }
            "install_rd7" => self.install(include_bytes!("../resources/rd7-tv.ipk")),
            "download_info" => Ok(json!({
                "name":"RD7 TV", "version":"1.6.2", "platform":"LG webOS",
                "filename":"RD7-TV_1.6.2.ipk",
                "size":include_bytes!("../resources/rd7-tv.ipk").len(),
                "id":APP,
                "studioVersion":env!("CARGO_PKG_VERSION"),
                "helperVersion":"1.0.1", "helperSize":include_bytes!("../resources/home-helper.zip").len()
            })),
            "download_rd7" => {
                let selected = rfd::FileDialog::new()
                    .set_title("Salvar RD7 TV para LG webOS")
                    .set_file_name("RD7-TV_1.6.2.ipk")
                    .add_filter("Pacote webOS", &["ipk"])
                    .save_file();
                if let Some(mut path) = selected {
                    if path
                        .extension()
                        .and_then(|x| x.to_str())
                        .map(|x| !x.eq_ignore_ascii_case("ipk"))
                        .unwrap_or(true)
                    {
                        path.set_extension("ipk");
                    }
                    fs::write(&path, include_bytes!("../resources/rd7-tv.ipk")).map_err(err)?;
                    Ok(
                        json!({"saved":true,"filename":path.file_name().unwrap_or_default().to_string_lossy(),"path":path.to_string_lossy()}),
                    )
                } else {
                    Ok(json!({"cancelled":true}))
                }
            }
            "download_helper" => {
                let selected = rfd::FileDialog::new()
                    .set_title("Salvar Home Helper para RD7 TV")
                    .set_file_name("RD7-TV-Home-Helper_1.0.1.zip")
                    .add_filter("Pacote ZIP", &["zip"])
                    .save_file();
                if let Some(path) = selected {
                    fs::write(&path, include_bytes!("../resources/home-helper.zip"))
                        .map_err(err)?;
                    Ok(
                        json!({"saved":true,"filename":path.file_name().unwrap_or_default().to_string_lossy(),"path":path.to_string_lossy()}),
                    )
                } else {
                    Ok(json!({"cancelled":true}))
                }
            }
            "helper_status" => {
                let raw=self.ssh("cat /media/developer/apps/usr/palm/services/io.github.theogal.homehelper.settings.service/state/helper.json 2>/dev/null || printf '{}'",None)?;
                let status: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
                Ok(
                    json!({"running":status["running"].as_bool().unwrap_or(false),"paired":status["paired"].as_bool().unwrap_or(false),"target":status["target"],"configured":status["running"]==true&&status["target"]["id"]==APP}),
                )
            }
            "install_helper" => {
                let present=self.ssh("if test -f /media/developer/apps/usr/palm/applications/io.github.theogal.homehelper/appinfo.json && test -f /media/developer/apps/usr/palm/applications/io.github.theogal.homehelper.settings/appinfo.json; then printf RD7_HELPER_PRESENT; else printf RD7_HELPER_MISSING; fi",None)?;
                let already = present.contains("RD7_HELPER_PRESENT");
                if !already {
                    self.install(include_bytes!("../resources/home-helper.ipk"))?;
                    self.install(include_bytes!("../resources/home-helper-settings.ipk"))?;
                }
                let root =
                    "/media/developer/apps/usr/palm/applications/io.github.theogal.homehelper";
                self.ssh(&format!("/usr/bin/node {root}/helper-entry.js {} && {root}/detach {} >/dev/null 2>&1 </dev/null",quote(&json!({"nativeHomeTarget":APP}).to_string()),quote(&json!({"nativeHomeStart":true}).to_string())),None)?;
                std::thread::sleep(Duration::from_secs(2));
                let status = self.dispatch("helper_status", Value::Null)?;
                Ok(
                    json!({"installed":true,"alreadyInstalled":already,"configured":status["configured"],"status":status,"permissionMayBeRequired":status["configured"]!=true}),
                )
            }
            "pick_images" => {
                let paths = rfd::FileDialog::new()
                    .add_filter("Imagens", &["jpg", "jpeg", "png", "webp"])
                    .pick_files();
                let mut images = Vec::new();
                if let Some(paths) = paths {
                    if paths.len() > 8 {
                        return Err("Selecione no máximo 8 imagens".into());
                    }
                    for path in paths {
                        let img = image::ImageReader::open(&path)
                            .map_err(err)?
                            .with_guessed_format()
                            .map_err(err)?
                            .decode()
                            .map_err(err)?
                            .resize(1600, 900, image::imageops::FilterType::Triangle)
                            .to_rgb8();
                        let mut data = Vec::new();
                        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, 78)
                            .encode_image(&img)
                            .map_err(err)?;
                        images.push(json!({"name":path.file_name().unwrap_or_default().to_string_lossy(),"data":format!("data:image/jpeg;base64,{}",B64.encode(data))}));
                    }
                }
                Ok(json!({"images":images}))
            }
            "background" => self.background(p),
            "theme" => {
                let theme = p["theme"].as_str().unwrap_or("dark");
                if !["dark", "light", "auto"].contains(&theme) {
                    return Err("Tema inválido".into());
                }
                self.cdp("Runtime.evaluate",json!({"expression":format!("Prefs.set('theme',{});document.dispatchEvent(new Event('webOSRelaunch'));true",json!(theme)),"returnByValue":true}))
            }
            _ => Err("Operação desconhecida".into()),
        }
    }
    pub fn smoke(&mut self) -> Value {
        let ip = std::env::var("RD7_TV_IP").unwrap_or_else(|_| "192.168.1.100".into());
        let result = (|| -> R<Value> {
            if let Ok(path) = std::env::var("RD7_IMPORT_DEV_KEY") {
                self.secure_session()?;
                fs::copy(path, self.keyfile()).map_err(err)?;
                let pass = std::env::var("FMTV_PASSPHRASE").unwrap_or_default();
                let out = cmd("ssh-keygen.exe")
                    .args(["-p", "-P", &pass, "-N", ""])
                    .arg("-f")
                    .arg(self.keyfile())
                    .output()
                    .map_err(err)?;
                if !out.status.success() {
                    return Err("Não foi possível importar a chave de desenvolvimento".into());
                }
                let secret = Secrets {
                    private_key: fs::read_to_string(self.keyfile()).map_err(err)?,
                    client_key: String::new(),
                };
                store_secret(&ip.replace('.', "_"), &secret)?;
            }
            let connect = self.connect(&json!({"ip":ip,"name":"LG Sala"}))?;
            if let Ok(path) = std::env::var("RD7_IMPORT_CONTROL_KEY") {
                let saved: Value =
                    serde_json::from_slice(&fs::read(path).map_err(err)?).map_err(err)?;
                self.secret.client_key = saved["client_key"]
                    .as_str()
                    .ok_or("Chave de controle inválida")?
                    .into();
                self.save()?;
            }
            let status = self.dispatch("status", Value::Null)?;
            let screenshot = self.screenshot().map(|mut s| {
                s.as_object_mut().unwrap().remove("image");
                s
            });
            Ok(json!({"connect":connect,"status":status,"screenshot":screenshot}))
        })();
        match result {
            Ok(v) => json!({"ok":true,"results":v}),
            Err(e) => json!({"ok":false,"error":e}),
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.keyfile());
    }
}
fn package_id(data: &[u8]) -> R<String> {
    if !data.starts_with(b"!<arch>\n") {
        return Err("Este arquivo não é um IPK válido".into());
    }
    let mut at = 8;
    while at + 60 <= data.len() {
        let header = &data[at..at + 60];
        let name = String::from_utf8_lossy(&header[..16])
            .trim()
            .trim_end_matches('/')
            .to_string();
        let size: usize = String::from_utf8_lossy(&header[48..58])
            .trim()
            .parse()
            .map_err(err)?;
        at += 60;
        if size > data.len().saturating_sub(at) {
            return Err("IPK incompleto".into());
        }
        if name == "control.tar.gz" {
            let decoder = flate2::read::GzDecoder::new(&data[at..at + size]);
            let mut archive = tar::Archive::new(decoder);
            for item in archive.entries().map_err(err)? {
                let item = item.map_err(err)?;
                if item.path().map_err(err)?.file_name() == Some(std::ffi::OsStr::new("control")) {
                    let mut text = String::new();
                    item.take(65536).read_to_string(&mut text).map_err(err)?;
                    if let Some(id) = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Package:").map(str::trim))
                    {
                        if valid_id(id) {
                            return Ok(id.into());
                        }
                    }
                }
            }
        }
        at += size + (size % 2);
    }
    Err("Não foi possível identificar o aplicativo no IPK".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validate_package() {
        assert_eq!(
            package_id(include_bytes!("../resources/rd7-tv.ipk")).unwrap(),
            APP
        );
        assert!(package_id(b"not-an-ipk").is_err());
    }
    #[test]
    fn identifiers_are_not_shell_code() {
        assert!(valid_id("io.freemytv"));
        assert!(!valid_id("app';reboot"));
        assert!(!valid_id("../app"));
        assert!(!valid_id(""));
    }
    #[test]
    fn helper_packages_have_expected_ids() {
        assert_eq!(
            package_id(include_bytes!("../resources/home-helper.ipk")).unwrap(),
            "io.github.theogal.homehelper"
        );
        assert_eq!(
            package_id(include_bytes!("../resources/home-helper-settings.ipk")).unwrap(),
            "io.github.theogal.homehelper.settings"
        );
        assert!(include_bytes!("../resources/home-helper.zip").starts_with(b"PK"));
    }
    #[test]
    fn shell_json_quotes() {
        assert_eq!(quote("a'b"), "'a'\\''b'");
    }
}
