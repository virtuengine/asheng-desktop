// 本地语音入口：唤醒词「阿胜阿胜」+ 右 Ctrl 按住说话
//
// 识别全部在本机 asr-host 子进程里完成，音频不出电脑；桌面端只拿最终文字，
// 填进「问一句」的输入框并发送，对话仍然走原来的 /agent-api/chat。

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::menu::MenuItem;
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_dialog::DialogExt;

use crate::{log_line, ASK_LABEL, JS_ASK_AUTH_NOW};

const HOST: &str = "http://127.0.0.1:18095";
const PORT: &str = "18095";

// 语音总开关（托盘菜单控制，持久化到 voice.json）
static ENABLED: AtomicBool = AtomicBool::new(false);
// 正在启动/关闭，避免连点
static STARTING: AtomicBool = AtomicBool::new(false);
// 事件轮询线程是否在跑
static POLLING: AtomicBool = AtomicBool::new(false);
// 已处理到的事件序号
static CURSOR: AtomicI64 = AtomicI64::new(0);
static PROC: Mutex<Option<Child>> = Mutex::new(None);
static ITEM: Mutex<Option<MenuItem<Wry>>> = Mutex::new(None);
static APP: Mutex<Option<AppHandle>> = Mutex::new(None);
// 页面登录令牌：识别接口需要它，只在内存里，不落盘也不写日志
static TOKEN: Mutex<String> = Mutex::new(String::new());

// 右 Ctrl 按住说话的状态机：HELD 表示键还按着，STARTING 表示「开始收音」请求在路上
static PTT_HELD: AtomicBool = AtomicBool::new(false);
static PTT_STARTING: AtomicBool = AtomicBool::new(false);
static PTT_ACTIVE: AtomicBool = AtomicBool::new(false);
static PTT_CANCEL: AtomicBool = AtomicBool::new(false);
// 每次按下递增，用来作废旧的长按计时器（连按时不会误触发）
static PTT_GEN: AtomicI64 = AtomicI64::new(0);
// macOS 用系统全局快捷键代替低级键盘钩子（右 Option 按住说话）
#[cfg(not(windows))]
static PTT_HOTKEY: Mutex<Option<tauri_plugin_global_shortcut::Shortcut>> = Mutex::new(None);

pub fn init(app: &AppHandle, item: MenuItem<Wry>) {
    if let Ok(mut g) = ITEM.lock() {
        *g = Some(item);
    }
    if let Ok(mut g) = APP.lock() {
        *g = Some(app.clone());
    }
    paint(false);
    if load_pref(app) {
        let h = app.clone();
        std::thread::spawn(move || {
            if let Err(e) = start(&h) {
                log_line(&h, &format!("语音启动失败：{}", e));
                let _ = post_timeout("/shutdown", json!({}), 500);
                kill_host();
                paint(false);
            }
        });
    }
    #[cfg(windows)]
    hook::install(app.clone());
}

/// 页面登录后把令牌同步过来；语音服务没开时先存着，开启时再推。
pub fn set_token(app: &AppHandle, token: String) {
    let changed = {
        let mut g = TOKEN.lock().unwrap_or_else(|e| e.into_inner());
        if *g == token {
            false
        } else {
            *g = token;
            true
        }
    };
    if !changed {
        return;
    }
    let tok = current_token();
    let handle = app.clone();
    std::thread::spawn(move || {
        let role = fetch_role_key(&tok).unwrap_or_else(|| "staff".into());
        if post("/config", json!({ "token": tok, "role": role })).is_ok() {
            log_line(&handle, "语音：登录令牌与角色已同步");
        }
    });
}

fn current_token() -> String {
    TOKEN
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|e| e.into_inner().clone())
}

/// 拉一次登录人的角色，唤醒时用来决定称呼（董事长 / 总监 / 经理）
fn fetch_role_key(token: &str) -> Option<String> {
    let resp = ureq::get("https://43.142.55.26:8668/erp-api/v1/auth/me")
        .set("Authorization", &format!("Bearer {}", token))
        .timeout(Duration::from_secs(8))
        .call()
        .ok()?;
    let v: Value = resp.into_json().ok()?;
    let role = v
        .get("data")
        .and_then(|d| d.get("role"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    Some(
        match role {
            "chairman" | "admin" => "chairman",
            "director" => "director",
            "manager" => "manager",
            _ => "staff",
        }
        .to_string(),
    )
}

fn push_token(app: &AppHandle) {
    let tok = current_token();
    if tok.is_empty() {
        return;
    }
    let role = fetch_role_key(&tok).unwrap_or_else(|| "staff".into());
    match post("/config", json!({ "token": tok, "role": role })) {
        Ok(_) => log_line(app, "语音：登录令牌与角色已同步"),
        Err(e) => log_line(app, &format!("语音：令牌同步失败 {}", e)),
    }
}

/// 托盘菜单点击：开/关
pub fn toggle(app: &AppHandle) {
    if ENABLED.load(Ordering::SeqCst) {
        disable(app);
        return;
    }
    if STARTING.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        let result = start(&handle);
        STARTING.store(false, Ordering::SeqCst);
        match result {
            Ok(_) => log_line(&handle, "语音已开启"),
            Err(e) => {
                log_line(&handle, &format!("语音开启失败：{}", e));
                // 起不来就别把 340MB 的服务留在后台
                let _ = post_timeout("/shutdown", json!({}), 500);
                kill_host();
                let h = handle.clone();
                std::thread::spawn(move || {
                    h.dialog()
                        .message(format!(
                            "语音开启失败：{}\n\n可在托盘菜单里重试，或检查麦克风是否被占用。",
                            e
                        ))
                        .title("阿胜 · 语音")
                        .blocking_show();
                });
            }
        }
    });
}

/// 退出程序：把子进程关掉，别留下占着麦克风的孤儿进程
pub fn shutdown() {
    ENABLED.store(false, Ordering::SeqCst);
    PTT_HELD.store(false, Ordering::SeqCst);
    PTT_ACTIVE.store(false, Ordering::SeqCst);
    PTT_STARTING.store(false, Ordering::SeqCst);
    let _ = post_timeout("/wake", json!({ "on": false }), 300);
    let _ = post_timeout("/shutdown", json!({}), 300);
    if let Ok(mut g) = PROC.lock() {
        if let Some(mut child) = g.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn start(app: &AppHandle) -> Result<(), String> {
    ensure_host(app)?;
    post("/wake", json!({ "on": true }))?;
    ENABLED.store(true, Ordering::SeqCst);
    if current_token().is_empty() {
        // 识别要走服务端，先唤出一次小窗，让页面把登录令牌带回来
        ensure_ask(app);
    } else {
        push_token(app);
    }
    save_pref(app, true);
    paint(true);
    start_poll(app.clone());
    let ptt = if cfg!(windows) { "右 Ctrl" } else { "右 Option" };
    log_line(
        app,
        &format!("语音已就绪：说「阿胜阿胜」，或按住{}说话", ptt),
    );
    Ok(())
}

fn disable(app: &AppHandle) {
    ENABLED.store(false, Ordering::SeqCst);
    PTT_HELD.store(false, Ordering::SeqCst);
    PTT_ACTIVE.store(false, Ordering::SeqCst);
    PTT_STARTING.store(false, Ordering::SeqCst);
    PTT_CANCEL.store(false, Ordering::SeqCst);
    save_pref(app, false);
    paint(false);
    log_line(app, "语音已关闭");
    let _ = post_timeout("/wake", json!({ "on": false }), 800);
    std::thread::spawn(|| {
        let _ = post_timeout("/shutdown", json!({}), 800);
        // 服务正常退出后进程会自己结束；这里的 wait/kill 只是兜底
        std::thread::sleep(Duration::from_millis(600));
        if let Ok(mut g) = PROC.lock() {
            if let Some(mut child) = g.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    });
}

// --------------------------- asr-host 子进程 ---------------------------

fn ensure_host(app: &AppHandle) -> Result<(), String> {
    if get("/health").is_ok() {
        return Ok(());
    }
    // 健康检查不通：可能上次异常退出的进程还挂着，先清掉再拉
    kill_host();
    // macOS 没有安装器钩子，首次开启时把语音资源包下载到本机
    #[cfg(not(windows))]
    ensure_voice_pack(app)?;
    let exe = locate_exe(app)?;
    let models = locate_models(app, &exe)?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--models").arg(&models).arg("--port").arg(PORT);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    // 子进程输出落到应用数据目录，出问题时能直接看日志
    if let Ok(dir) = app.path().app_data_dir() {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("asr-host.log"))
        {
            if let Ok(err) = file.try_clone() {
                cmd.stdout(std::process::Stdio::from(file));
                cmd.stderr(std::process::Stdio::from(err));
            }
        }
    }
    log_line(app, &format!("启动语音服务：{}", exe.display()));
    let child = cmd
        .spawn()
        .map_err(|e| format!("无法启动语音服务：{}", e))?;
    if let Ok(mut g) = PROC.lock() {
        *g = Some(child);
    }
    // 首次启动要加载模型，给足 45 秒
    for _ in 0..180 {
        if get("/health").is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err("语音服务启动超时（模型加载失败或端口被占用）".into())
}

fn kill_host() {
    if let Ok(mut g) = PROC.lock() {
        if let Some(mut child) = g.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 依次尝试：环境变量 → 应用数据目录 → 安装目录 → 开发目录
fn asr_dirs(app: &AppHandle) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    // macOS 下载的语音包放在应用数据目录
    if let Ok(d) = app.path().app_data_dir() {
        dirs.push(d.join("asr"));
    }
    if let Ok(d) = app.path().resource_dir() {
        dirs.push(d.join("asr"));
        dirs.push(d.join("resources").join("asr"));
        dirs.push(d);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.join("asr"));
            dirs.push(d.join("resources").join("asr"));
            dirs.push(d.to_path_buf());
        }
    }
    dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("asr"));
    dirs
}

fn locate_exe(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("ASHENG_ASR_EXE") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
    }
    let name = if cfg!(windows) { "asr-host.exe" } else { "asr-host" };
    for dir in asr_dirs(app) {
        let p = dir.join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    Err("找不到语音组件 asr-host，请重新安装阿盛桌面端".into())
}

fn locate_models(app: &AppHandle, exe: &Path) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("ASHENG_ASR_MODELS") {
        let p = PathBuf::from(p);
        if p.join("vad").join("silero_vad.onnx").exists() {
            return Ok(p);
        }
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = exe.parent() {
        dirs.push(d.join("models"));
        if let Some(up) = d.parent() {
            dirs.push(up.join("models"));
        }
    }
    for d in asr_dirs(app) {
        dirs.push(d.join("models"));
    }
    for d in dirs {
        if d.join("vad").join("silero_vad.onnx").exists() {
            return Ok(d);
        }
    }
    Err("找不到语音模型文件，请重新安装阿盛桌面端".into())
}

/// macOS 没有安装器钩子：按芯片下载语音资源包，校验后解压到应用数据目录
#[cfg(not(windows))]
fn ensure_voice_pack(app: &AppHandle) -> Result<(), String> {
    let root = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("找不到应用数据目录：{}", e))?
        .join("asr");
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    };
    let manifest_url = format!(
        "https://43.142.55.26:8668/dl/desktop/voice-pack-mac-{}.json",
        arch
    );

    // 先取清单：本地版本和云端不一致就重新下载，升级客户端后能自动换掉旧语音包
    let resp = ureq::get(&manifest_url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| format!("获取语音模块信息失败：{}", e))?;
    let info: Value = resp
        .into_json()
        .map_err(|e| format!("语音模块信息解析失败：{}", e))?;
    let version = info
        .get("version")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let url = info
        .get("url")
        .and_then(|x| x.as_str())
        .ok_or("语音模块下载地址缺失")?
        .to_string();
    let sha = info
        .get("sha256")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_lowercase();

    let marker: Value = std::fs::read_to_string(root.join(".ok"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let local_sha = marker.get("sha256").and_then(|x| x.as_str()).unwrap_or("");
    let local_version = marker.get("version").and_then(|x| x.as_str()).unwrap_or("");
    let same_pack = if sha.is_empty() {
        local_version == version
    } else {
        local_sha == sha
    };
    let ready = same_pack
        && root.join("asr-host").exists()
        && root.join("models").join("vad").join("silero_vad.onnx").exists();
    if ready {
        return Ok(());
    }

    paint_text("语音：正在下载模块…");
    log_line(
        app,
        &format!(
            "下载语音模块 {}（约 30MB，本地 {}）",
            version,
            if local_version.is_empty() {
                "无"
            } else {
                local_version
            }
        ),
    );
    let result = download_voice_pack(&root, &url, &sha, &version);
    paint(ENABLED.load(Ordering::SeqCst));
    if result.is_ok() {
        log_line(app, "语音模块下载完成");
    }
    result
}

#[cfg(not(windows))]
fn download_voice_pack(root: &Path, url: &str, sha: &str, version: &str) -> Result<(), String> {
    use std::io::{Cursor, Read};

    let resp = ureq::get(url)
        .timeout(Duration::from_secs(300))
        .call()
        .map_err(|e| format!("下载语音模块失败：{}", e))?;
    let mut bytes: Vec<u8> = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("下载语音模块失败：{}", e))?;
    if bytes.is_empty() {
        return Err("下载到的语音模块为空".into());
    }
    if !sha.is_empty() {
        use sha2::{Digest, Sha256};
        let got = format!("{:x}", Sha256::digest(&bytes));
        if got != sha {
            return Err("语音模块校验失败，请重试".into());
        }
    }

    if root.exists() {
        std::fs::remove_dir_all(root).map_err(|e| format!("清理旧语音模块失败：{}", e))?;
    }
    std::fs::create_dir_all(root).map_err(|e| format!("创建语音目录失败：{}", e))?;

    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("语音包解压失败：{}", e))?;
    for i in 0..zip.len() {
        let mut item = zip
            .by_index(i)
            .map_err(|e| format!("语音包读取失败：{}", e))?;
        let rel = match item.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let out = root.join(&rel);
        if item.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("解压失败：{}", e))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("解压失败：{}", e))?;
        }
        let mut file = std::fs::File::create(&out).map_err(|e| format!("解压失败：{}", e))?;
        std::io::copy(&mut item, &mut file).map_err(|e| format!("解压失败：{}", e))?;
        #[cfg(unix)]
        if rel == Path::new("asr-host") {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
        }
    }
    let marker = json!({ "version": version, "sha256": sha });
    std::fs::write(root.join(".ok"), marker.to_string())
        .map_err(|e| format!("写入完成标记失败：{}", e))?;
    Ok(())
}

// --------------------------- 与服务通信 ---------------------------

fn get(path: &str) -> Result<Value, String> {
    let resp = ureq::get(&format!("{}{}", HOST, path))
        .timeout(Duration::from_millis(1200))
        .call()
        .map_err(|e| e.to_string())?;
    resp.into_json::<Value>().map_err(|e| e.to_string())
}

fn post(path: &str, body: Value) -> Result<Value, String> {
    post_timeout(path, body, 10_000)
}

fn post_timeout(path: &str, body: Value, ms: u64) -> Result<Value, String> {
    let resp = ureq::post(&format!("{}{}", HOST, path))
        .timeout(Duration::from_millis(ms))
        .send_json(body)
        .map_err(|e| e.to_string())?;
    resp.into_json::<Value>().map_err(|e| e.to_string())
}

// 回答播报会从扬声器回到麦克风，发送提问后先静音一段时间，避免把自己的声音当成新提问
static MUTE_SEQ: AtomicU64 = AtomicU64::new(0);

fn mute_after_ask() {
    let seq = MUTE_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    match post("/mute", json!({ "on": true })) {
        Ok(_) => {
            if let Some(app) = app_handle() {
                log_line(&app, "语音：播报期间静音采集");
            }
        }
        Err(e) => {
            if let Some(app) = app_handle() {
                log_line(&app, &format!("语音：静音失败 {}", e));
            }
        }
    }
    std::thread::spawn(move || {
        // 播报 60 秒后仍未出现打断，就恢复正常听写
        std::thread::sleep(Duration::from_secs(60));
        if MUTE_SEQ.load(Ordering::SeqCst) == seq {
            let _ = post("/mute", json!({ "on": false }));
        }
    });
}

// --------------------------- 事件轮询 ---------------------------

fn start_poll(app: AppHandle) {
    if POLLING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let mut fails = 0u32;
        let mut last_log = Instant::now();
        loop {
            if !ENABLED.load(Ordering::SeqCst) {
                break;
            }
            let since = CURSOR.load(Ordering::SeqCst);
            match get(&format!("/events?since={}", since)) {
                Ok(v) => {
                    fails = 0;
                    if let Some(next) = v.get("next").and_then(|x| x.as_i64()) {
                        if next > since {
                            CURSOR.store(next, Ordering::SeqCst);
                        }
                    }
                    if let Some(items) = v.get("events").and_then(|x| x.as_array()) {
                        for item in items {
                            handle_event(&app, item);
                        }
                    }
                }
                Err(err) => {
                    fails += 1;
                    if fails == 1 || last_log.elapsed() > Duration::from_secs(60) {
                        log_line(&app, &format!("语音服务连接异常：{}", err));
                        last_log = Instant::now();
                    }
                    // 服务意外退出：等一小会儿再拉起来，别让用户以为语音坏了
                    if fails >= 20 {
                        fails = 0;
                        if ENABLED.load(Ordering::SeqCst) {
                            log_line(&app, "语音服务无响应，尝试重启");
                            if let Err(e) = ensure_host(&app) {
                                log_line(&app, &format!("语音服务重启失败：{}", e));
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(400));
        }
        POLLING.store(false, Ordering::SeqCst);
    });
}

fn handle_event(app: &AppHandle, e: &Value) {
    match e.get("type").and_then(|x| x.as_str()).unwrap_or("") {
        "wake" => {
            log_line(app, "语音：听到唤醒词「阿胜阿胜」");
            ensure_ask(app);
        }
        "text" => {
            let text = e
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // 只认带实际文字的识别结果，纯标点（如「。」）是环境噪音的产物
            if text.chars().any(|c| c.is_alphanumeric()) {
                log_line(app, &format!("语音识别：{}", text));
                deliver(app, &text);
            }
        }
        "sleep" => log_line(app, "语音：这轮对话结束，回到待机"),
        "interrupt" => {
            log_line(app, "语音：收到打断，停止播报");
            if let Some(w) = app.get_webview_window(ASK_LABEL) {
                let _ = w.eval("try{window.__asgStopSpeech&&window.__asgStopSpeech()}catch(e){}");
            }
        }
        "bye" => {
            log_line(app, "语音：用户结束了这轮对话");
            if let Some(w) = app.get_webview_window(ASK_LABEL) {
                let _ = w.eval("try{window.__asgStopSpeech&&window.__asgStopSpeech()}catch(e){}");
            }
            MUTE_SEQ.fetch_add(1, Ordering::SeqCst);
            let _ = post("/mute", json!({ "on": false }));
            // 等告别语播完再收起面板
            let handle = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(2800));
                if let Some(w) = handle.get_webview_window(ASK_LABEL) {
                    let _ = w.close();
                }
            });
        }
        "empty" => log_line(app, "语音：这段没听清"),
        "error" => log_line(
            app,
            &format!(
                "语音识别出错：{}",
                e.get("text").and_then(|x| x.as_str()).unwrap_or("")
            ),
        ),
        _ => {}
    }
}

/// 打开（或唤出）问一句窗口；没有焦点也先把窗口摆出来
fn ensure_ask(app: &AppHandle) {
    match app.get_webview_window(ASK_LABEL) {
        Some(w) => {
            if w.is_visible().unwrap_or(false) {
                let _ = w.set_focus();
            } else {
                // 走页面既有的登录态探测，已登录才会显示
                let _ = w.eval(JS_ASK_AUTH_NOW);
            }
        }
        None => crate::toggle_ask(app),
    }
}

// 页面可能还在加载：同一个 id 循环注入，页面侧按 id 去重，不会重复发送
const INJECT: &str = r#"(function(id,text){
  try{
    if(window.__ashVoiceId===id) return;
    window.__ashVoiceId=id; window.__ashVoiceTok=id;
    var tries=0;
    function go(){
      if(window.__ashVoiceTok!==id) return;
      var i=document.getElementById('asgInput');
      if(!i){ if(++tries<80) setTimeout(go,250); return; }
      i.focus(); i.value=text;
      i.dispatchEvent(new Event('input',{bubbles:true}));
      // 告诉页面这是语音提问，它才会用语音播报回答
      window.__asgVoiceInput=true;
      var b=document.getElementById('asgSend');
      if(b&&!b.disabled){ b.click(); window.__ashVoiceTok=''; return; }
      if(++tries<80) setTimeout(go,250);
    }
    go();
  }catch(e){}
})(__ID__, __TEXT__);"#;

static INJECT_SEQ: AtomicI64 = AtomicI64::new(0);

fn deliver(app: &AppHandle, text: &str) {
    ensure_ask(app);
    // 提问一旦发出，回答很可能马上开始播报，先把自己的麦克风静音
    mute_after_ask();
    let id = INJECT_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let lit = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    let js = INJECT
        .replace("__ID__", &id.to_string())
        .replace("__TEXT__", &lit);
    let handle = app.clone();
    std::thread::spawn(move || {
        for _ in 0..30 {
            if let Some(w) = handle.get_webview_window(ASK_LABEL) {
                let _ = w.eval(&js);
            }
            std::thread::sleep(Duration::from_millis(350));
        }
    });
}

// --------------------------- 按住右 Ctrl / 右 Option 说话 ---------------------------

#[cfg(not(windows))]
pub fn register_ptt(app: &AppHandle) {
    use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut};

    // Mac 键盘没有右 Ctrl，优先用右 Option（右 Alt）按住说话
    for (code, label) in [(Code::AltRight, "右 Option"), (Code::ControlRight, "右 Ctrl")] {
        let sc = Shortcut::new(None, code);
        match app.global_shortcut().register(sc.clone()) {
            Ok(_) => {
                if let Ok(mut g) = PTT_HOTKEY.lock() {
                    *g = Some(sc);
                }
                log_line(app, &format!("按住{}说话已就绪", label));
                return;
            }
            Err(e) => log_line(app, &format!("按住{}说话不可用：{}", label, e)),
        }
    }
    log_line(app, "按住说话不可用，可继续用唤醒词「阿胜阿胜」");
}

/// 当前平台的按住说话快捷键；Windows 走键盘钩子，不用全局快捷键
pub fn ptt_shortcut() -> Option<tauri_plugin_global_shortcut::Shortcut> {
    #[cfg(not(windows))]
    {
        PTT_HOTKEY.lock().ok().and_then(|g| g.clone())
    }
    #[cfg(windows)]
    {
        None
    }
}

pub fn ptt_press() {
    ptt_down();
}

pub fn ptt_release() {
    ptt_up();
}

fn ptt_down() {
    if !ENABLED.load(Ordering::SeqCst) {
        return;
    }
    if PTT_HELD.swap(true, Ordering::SeqCst) {
        return;
    }
    PTT_CANCEL.store(false, Ordering::SeqCst);
    let gen = PTT_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        // 短按右 Ctrl 是正常操作，按住 360ms 之后才当说话
        std::thread::sleep(Duration::from_millis(360));
        if PTT_GEN.load(Ordering::SeqCst) != gen
            || !PTT_HELD.load(Ordering::SeqCst)
            || PTT_CANCEL.load(Ordering::SeqCst)
            || !ENABLED.load(Ordering::SeqCst)
        {
            return;
        }
        if PTT_STARTING.swap(true, Ordering::SeqCst) {
            return;
        }
        match post("/ptt", json!({ "on": true })) {
            Ok(_) => {
                PTT_STARTING.store(false, Ordering::SeqCst);
                PTT_ACTIVE.store(true, Ordering::SeqCst);
                // 起收音期间用户已经松手：立刻结束，别把麦克风一直开着
                if !PTT_HELD.load(Ordering::SeqCst) && PTT_ACTIVE.swap(false, Ordering::SeqCst) {
                    let _ = post("/ptt", json!({ "on": false }));
                }
            }
            Err(e) => {
                PTT_STARTING.store(false, Ordering::SeqCst);
                say(&format!("语音：按住说话启动失败 {}", e));
            }
        }
    });
}

fn ptt_up() {
    PTT_HELD.store(false, Ordering::SeqCst);
    PTT_CANCEL.store(false, Ordering::SeqCst);
    if !PTT_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let app = app_handle();
    std::thread::spawn(move || match post("/ptt", json!({ "on": false })) {
        Ok(v) => {
            let text = v
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                say("语音：没听清");
                return;
            }
            say(&format!("语音识别：{}", text));
            if let Some(app) = app {
                deliver(&app, &text);
            }
        }
        Err(e) => say(&format!("语音识别失败：{}", e)),
    });
}

#[cfg(windows)]
fn ptt_cancel() {
    if PTT_HELD.load(Ordering::SeqCst) {
        PTT_CANCEL.store(true, Ordering::SeqCst);
    }
}

fn app_handle() -> Option<AppHandle> {
    APP.lock().ok().and_then(|g| g.clone())
}

fn say(msg: &str) {
    if let Some(app) = app_handle() {
        log_line(&app, msg);
    }
}

#[cfg(windows)]
mod hook {
    use super::*;
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION,
        KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    pub fn install(app: AppHandle) {
        std::thread::spawn(move || unsafe {
            let hmod = GetModuleHandleW(std::ptr::null());
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), hmod, 0);
            if hook.is_null() {
                log_line(&app, "右 Ctrl 语音键不可用（键盘钩子安装失败）");
                return;
            }
            log_line(&app, "右 Ctrl 语音键已就绪");
            let mut msg: MSG = std::mem::zeroed();
            // 低级键盘钩子要求所在线程有消息循环，否则回调不会触发
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
            let _ = UnhookWindowsHookEx(hook);
        });
    }

    const VK_RCONTROL: u32 = 0xA3;

    unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
            let msg = wparam as u32;
            if kb.vkCode == VK_RCONTROL {
                if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                    ptt_down();
                } else if msg == WM_KEYUP || msg == WM_SYSKEYUP {
                    ptt_up();
                }
            } else if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                // 右 Ctrl 参与组合键（如 Ctrl+C）时不当作说话
                ptt_cancel();
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }
}

// --------------------------- 配置与托盘文字 ---------------------------

fn pref_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("voice.json"))
}

fn load_pref(app: &AppHandle) -> bool {
    let Some(p) = pref_path(app) else {
        return false;
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("on").and_then(|x| x.as_bool()))
        .unwrap_or(false)
}

fn save_pref(app: &AppHandle, on: bool) {
    let Some(p) = pref_path(app) else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, json!({ "on": on }).to_string());
}

fn paint(on: bool) {
    paint_text(if on {
        "语音：开（说「阿胜阿胜」）"
    } else {
        "语音：关（点击开启）"
    });
}

fn paint_text(text: &str) {
    if let Ok(g) = ITEM.lock() {
        if let Some(item) = g.as_ref() {
            let _ = item.set_text(text);
        }
    }
}
