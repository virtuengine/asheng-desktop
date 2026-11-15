#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// 阿盛桌面端 · 第一版（P0）
// 形态：主窗口加载现有大屏页面 + 托盘常驻 + Ctrl+Alt+A 快捷小窗 + 开机自启 + 版本检查

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
#[cfg(not(windows))]
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

const MAIN_URL: &str = "https://43.142.55.26:8668/erp/";
const ASK_URL: &str = "https://43.142.55.26:8668/ask/";
const SCREEN_URL: &str = "https://43.142.55.26:8668/?client=1";
const VERSION_URL_WIN: &str = "https://43.142.55.26:8668/dl/desktop/latest-win.json";
const VERSION_URL_MAC: &str = "https://43.142.55.26:8668/dl/desktop/latest-mac.json";

fn version_url() -> &'static str {
    if cfg!(target_os = "macos") {
        VERSION_URL_MAC
    } else {
        VERSION_URL_WIN
    }
}
const MAIN_LABEL: &str = "main";
const ASK_LABEL: &str = "ask";
const BALL_LABEL: &str = "ball";

// 让壳内的链接在自己窗口里跳转，不甩给系统浏览器
const JS_NAV_FIX: &str = r#"(function(){
  try{
    var fix=function(){
      var list=document.querySelectorAll('a[target="_blank"]');
      for(var i=0;i<list.length;i++){ list[i].removeAttribute('target'); }
    };
    fix();
    document.addEventListener('click',function(e){
      var a=e.target&&e.target.closest?e.target.closest('a[target="_blank"]'):null;
      if(a){ a.removeAttribute('target'); }
    },true);
    window.open=function(u){ if(u){ location.href=u; } return null; };
  }catch(e){}
})();"#;

// 前端登录是 history 路由，不会触发 WebView 导航；这里轮询路径，把登录/工作状态告诉壳
const JS_WIN_STATE: &str = r#"(function(){
  try{
    var last='';
    function tick(){
      try{
        var p=location.pathname||'';
        if(p.indexOf('/erp')!==0) return;
        var s=(p.indexOf('/erp/login')===0||p==='/erp/'||p==='/erp')?'login':'app';
        if(s!==last){ last=s; location.href='/ask/win/'+s; }
      }catch(e){}
    }
    setInterval(tick,700);
  }catch(e){}
})();"#;

// 实际生效的呼出快捷键（默认 Ctrl+Alt+A，被占用时自动换下一个）
static ASK_HOTKEY: Mutex<Option<Shortcut>> = Mutex::new(None);
// 退出快捷键：Ctrl+Alt+Shift+Q
static QUIT_HOTKEY: Mutex<Option<Shortcut>> = Mutex::new(None);
// 小窗是否真正获得过焦点（没获得过就别自动收，否则刚弹出就消失）
static ASK_ARMED: AtomicBool = AtomicBool::new(false);
// 小窗是否正在创建（避免连点重复建窗）
static ASK_CREATING: AtomicBool = AtomicBool::new(false);
// 主窗口是否已切到工作尺寸（登录页用紧凑尺寸，登录后放大）
static MAIN_EXPANDED: AtomicBool = AtomicBool::new(false);
// 阿盛面板是否停靠在 ERP 右侧
static DOCKED: AtomicBool = AtomicBool::new(false);
// 是否已登录并进入工作区（决定悬浮球贴 ERP 还是回桌面）
static WORKSPACE: AtomicBool = AtomicBool::new(false);
// 停靠相关窗口操作时的重入保护（我们自己挪窗口会触发 Moved/Resized）
static DOCK_GUARD: AtomicBool = AtomicBool::new(false);
// 面板是否因为 ERP 最小化被一起收起（恢复时再放出来）
static ASK_FOLLOW: AtomicBool = AtomicBool::new(false);
// ERP 关闭/最小化后，面板独立贴右侧；ERP 回来后是否自动回停靠
static ASK_REDOCK: AtomicBool = AtomicBool::new(false);
// 跟随排布的合并队列：拖动/缩放时事件很密，合并成每 50ms 一次，避免掉帧
static ALIGN_TX: Mutex<Option<std::sync::mpsc::Sender<()>>> = Mutex::new(None);
// 停靠面板宽度（逻辑像素）
const DOCK_W: f64 = 420.0;

// 小窗打开前先检查登录态：没登录就切主窗口，不弹第二个登录框
// 页面脚本可能还没执行完，这里等 askHasLogin 就绪再判定，避免误判未登录
const JS_ASK_AUTH: &str = r#"(function(){
  if(window.__askProbed) return;
  var tries=0;
  function probe(){
    if(typeof window.askHasLogin!=='function' && tries<20){ tries++; setTimeout(probe,150); return; }
    window.__askProbed=1;
    var ok=false;
    try{ ok=!!(window.askHasLogin&&window.askHasLogin()); }catch(e){}
    var len=0;
    try{ len=(localStorage.getItem('sf-erp-token')||localStorage.getItem('sf_token')||'').length; }catch(e){}
    location.href='/ask/state/'+(ok?'1':'0')+'?len='+len+'&fn='+(typeof window.askHasLogin);
  }
  probe();
})();"#;
// 用户再次按热键/点悬浮球时，允许重新探测
const JS_ASK_AUTH_NOW: &str = r#"(function(){
  var ok=false;
  try{ ok=!!(window.askHasLogin&&window.askHasLogin()); }catch(e){}
  var len=0;
  try{ len=(localStorage.getItem('sf-erp-token')||localStorage.getItem('sf_token')||'').length; }catch(e){}
  location.href='/ask/state/'+(ok?'1':'0')+'?len='+len+'&fn='+(typeof window.askHasLogin);
})();"#;

// 客户端里把页面滚动条收细成中性灰，和 ERP 的扁平规范一致
const JS_SCROLL_STYLE: &str = r#"(function(){
  try{
    if(document.getElementById('asg-scroll-style')) return;
    var s=document.createElement('style');
    s.id='asg-scroll-style';
    s.textContent='::-webkit-scrollbar{width:6px;height:6px}'+
      '::-webkit-scrollbar-track{background:transparent}'+
      '::-webkit-scrollbar-thumb{background:#d5dce8;border-radius:0}'+
      '::-webkit-scrollbar-thumb:hover{background:#bfc3d3}'+
      '::-webkit-scrollbar-corner{background:transparent}';
    document.head.appendChild(s);
  }catch(e){}
})();"#;

/// WebView2 启动参数。
/// 注意：Tauri 会显式传入 additional_browser_args，环境变量
/// WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS 会被忽略，所以这里显式带上：
/// - Tauri 默认的 disable-features 与 autoplay-policy
/// - use-fake-ui-for-media-stream：麦克风权限自动放行，不弹暴露服务器地址的授权框
/// - ASHENG_DEBUG_PORT：仅排查性能时开启调试端口
fn browser_args() -> String {
    let mut args = String::from(
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion \
--disable-background-timer-throttling --disable-backgrounding-occluded-windows \
--disable-renderer-backgrounding \
--autoplay-policy=no-user-gesture-required --use-fake-ui-for-media-stream",
    );
    if let Ok(port) = std::env::var("ASHENG_DEBUG_PORT") {
        let p = port.trim();
        if !p.is_empty() {
            args.push_str(&format!(" --remote-debugging-port={}", p));
        }
    }
    args
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let active = ASK_HOTKEY.lock().ok().and_then(|g| g.clone());
                    if active.as_ref() == Some(shortcut) {
                        toggle_ask(app);
                        return;
                    }
                    let quit = QUIT_HOTKEY.lock().ok().and_then(|g| g.clone());
                    if quit.as_ref() == Some(shortcut) {
                        quit_app(app);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![toggle_panel, toggle_ball, ball_menu])
        .on_menu_event(|app, event| match event.id.as_ref() {
            "ball_erp" => show_main(app),
            "ball_ask" => toggle_ask(app),
            "ball_hide" => toggle_ball((*app).clone()),
            "ball_quit" => quit_app(app),
            _ => {}
        })
        .setup(|app| {
            if create_main(app.handle()).is_none() {
                log_line(app.handle(), "主窗口创建失败");
                // 主线程里同步弹窗可能死锁，放到单独线程
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    handle
                        .dialog()
                        .message("主窗口创建失败。\n\n请确认系统已安装 WebView2 运行时，或重新安装阿盛桌面端。")
                        .title("阿盛 · 启动失败")
                        .blocking_show();
                });
            }
            if create_ball(app.handle()).is_none() {
                log_line(app.handle(), "悬浮球创建失败");
            } else {
                prewarm_ball(app.handle());
            }
            let ask_item = create_tray(app.handle())?;

            // 逐个尝试快捷键，被别的软件占用就自动换下一个
            let mut picked: Option<(Shortcut, &'static str)> = None;
            for (hk, label) in ask_shortcut_candidates() {
                match app.global_shortcut().register(hk.clone()) {
                    Ok(_) => {
                        log_line(app.handle(), &format!("快捷键可用：{}", label));
                        picked = Some((hk, label));
                        break;
                    }
                    Err(e) => log_line(app.handle(), &format!("快捷键被占用：{} / {}", label, e)),
                }
            }
            match picked {
                Some((hk, label)) => {
                    if let Ok(mut g) = ASK_HOTKEY.lock() {
                        *g = Some(hk);
                    }
                    let _ = ask_item.set_text(format!("问一句（{}）", label));
                }
                None => log_line(app.handle(), "所有快捷键都注册失败，只能用托盘菜单打开"),
            }

            // 退出快捷键：Ctrl+Alt+Shift+Q
            let quit_hk = Shortcut::new(
                Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT),
                Code::KeyQ,
            );
            match app.global_shortcut().register(quit_hk.clone()) {
                Ok(_) => {
                    if let Ok(mut g) = QUIT_HOTKEY.lock() {
                        *g = Some(quit_hk);
                    }
                    log_line(app.handle(), "退出快捷键可用：Ctrl+Alt+Shift+Q");
                }
                Err(e) => log_line(app.handle(), &format!("退出快捷键被占用：{}", e)),
            }

            // 开机自启：默认打开，用户可在系统「启动应用」里关掉
            match enable_autostart(app.handle()) {
                Ok(_) => log_line(app.handle(), "开机自启已开启"),
                Err(e) => log_line(app.handle(), &format!("开机自启开启失败：{}", e)),
            }

            // 启动 6 秒后静默检查一次版本
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(6));
                check_update(&handle, false);
            });

            // 窗口跟随：事件先入队，50ms 合并一次再排布（拖动时不再每个事件都挪窗）
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            if let Ok(mut g) = ALIGN_TX.lock() {
                *g = Some(tx);
            }
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut pending = false;
                loop {
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(_) => pending = true,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if pending {
                                pending = false;
                                align_dock(&handle);
                                ball_sync(&handle);
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                // 对话框关闭只收起，不销毁：销毁会留下停靠/跟随等脏状态，之后再也打不开
                if window.label() == ASK_LABEL {
                    api.prevent_close();
                    let _ = window.hide();
                    ASK_ARMED.store(false, Ordering::SeqCst);
                    ASK_FOLLOW.store(false, Ordering::SeqCst);
                    if DOCKED.swap(false, Ordering::SeqCst) {
                        restore_main_width(window.app_handle());
                    }
                    ball_sync(window.app_handle());
                    return;
                }
                // 主窗口点关闭只收进托盘，不退出
                if window.label() == MAIN_LABEL {
                    api.prevent_close();
                    // ERP 关闭：对话框不受影响；只有它本来就开着，才解绑并贴到屏幕右侧
                    let ask_open = window
                        .app_handle()
                        .get_webview_window(ASK_LABEL)
                        .map(|a| a.is_visible().unwrap_or(false))
                        .unwrap_or(false);
                    ASK_REDOCK.store(ask_open, Ordering::SeqCst);
                    float_ask_right(window.app_handle());
                    ASK_FOLLOW.store(false, Ordering::SeqCst);
                    let _ = window.hide();
                    // ERP 不在：球按面板是否可见决定显隐
                    ball_sync(window.app_handle());
                }
            }
            WindowEvent::Moved(_) => {
                if window.label() == ASK_LABEL {
                    on_ask_moved(window.app_handle());
                } else if window.label() == MAIN_LABEL {
                    schedule_align(window.app_handle());
                }
            }
            WindowEvent::Resized(_) => {
                if window.label() == MAIN_LABEL {
                    // ERP 最小化：对话框不跟着收起；本来就开着才解绑并贴到屏幕右侧
                    if window.is_minimized().unwrap_or(false) {
                        let ask_open = window
                            .app_handle()
                            .get_webview_window(ASK_LABEL)
                            .map(|a| a.is_visible().unwrap_or(false))
                            .unwrap_or(false);
                        ASK_REDOCK.store(ask_open, Ordering::SeqCst);
                        float_ask_right(window.app_handle());
                        ASK_FOLLOW.store(false, Ordering::SeqCst);
                        ball_sync(window.app_handle());
                        return;
                    }
                    // ERP 全屏时右侧没有停靠空间：先把面板收起，退出全屏再放回来
                    if window.is_maximized().unwrap_or(false) && DOCKED.load(Ordering::SeqCst) {
                        if let Some(a) = window.app_handle().get_webview_window(ASK_LABEL) {
                            if a.is_visible().unwrap_or(false) {
                                let _ = a.hide();
                                ASK_FOLLOW.store(true, Ordering::SeqCst);
                            }
                        }
                        ball_sync(window.app_handle());
                        return;
                    }
                    // ERP 关闭/最小化前是停靠的：回来后自动恢复停靠
                    // 只在 ERP 真正可见时消费标记，避免关闭过程中的 Resized 把标记吃掉
                    if window.is_visible().unwrap_or(false)
                        && ASK_REDOCK.swap(false, Ordering::SeqCst)
                    {
                        let app = window.app_handle().clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(150));
                            let up = app
                                .get_webview_window(MAIN_LABEL)
                                .map(|m| m.is_visible().unwrap_or(false) && !m.is_minimized().unwrap_or(false))
                                .unwrap_or(false);
                            if up {
                                dock_layout(&app);
                            }
                        });
                        return;
                    }
                    if ASK_FOLLOW.swap(false, Ordering::SeqCst) {
                        if let Some(a) = window.app_handle().get_webview_window(ASK_LABEL) {
                            let _ = a.show();
                        }
                    }
                    schedule_align(window.app_handle());
                }
            }
            // 兜底：对话框被其他途径销毁时，同样复位状态并让球回来
            WindowEvent::Destroyed => {
                if window.label() == ASK_LABEL {
                    ASK_ARMED.store(false, Ordering::SeqCst);
                    ASK_FOLLOW.store(false, Ordering::SeqCst);
                    if DOCKED.swap(false, Ordering::SeqCst) {
                        restore_main_width(window.app_handle());
                    }
                    ball_sync(window.app_handle());
                }
            }
            WindowEvent::Focused(focused) => {
                // 停靠时 ERP 与面板必须同层：任意一个被激活，另一个也提到最前
                // 加 200ms 节流，避免快速来回点两个窗口时反复调 z 序
                if *focused && DOCKED.load(Ordering::SeqCst) && allow_raise() {
                    if window.label() == MAIN_LABEL {
                        if let Some(a) = window.app_handle().get_webview_window(ASK_LABEL) {
                            if a.is_visible().unwrap_or(false) {
                                raise_no_activate(&a);
                            }
                        }
                    } else if window.label() == ASK_LABEL {
                        if let Some(m) = window.app_handle().get_webview_window(MAIN_LABEL) {
                            raise_no_activate(&m);
                        }
                    }
                }
                if window.label() != ASK_LABEL {
                    return;
                }
                // 停靠状态不自动收起，面板本来就是工作区的一部分
                if DOCKED.load(Ordering::SeqCst) {
                    return;
                }
                if *focused {
                    ASK_ARMED.store(true, Ordering::SeqCst);
                    return;
                }
                // 真正拿到过焦点之后，失焦 4 秒才收起
                if !ASK_ARMED.load(Ordering::SeqCst) {
                    return;
                }
                let w = window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(4000));
                    // 这 4 秒内如果已经停靠，就不再自动收（面板是工作区的一部分）
                    if DOCKED.load(Ordering::SeqCst) {
                        return;
                    }
                    if !w.is_focused().unwrap_or(true) {
                        let _ = w.hide();
                        ASK_ARMED.store(false, Ordering::SeqCst);
                        // 面板收起后，球按 ERP 是否在场重新出现
                        ball_sync(w.app_handle());
                    }
                });
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("阿盛桌面端启动失败");
}

fn ask_shortcut_candidates() -> Vec<(Shortcut, &'static str)> {
    vec![
        (
            Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyA),
            "Ctrl+Alt+A",
        ),
        (
            Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyQ),
            "Ctrl+Alt+Q",
        ),
        (
            Shortcut::new(
                Some(Modifiers::CONTROL | Modifiers::SHIFT | Modifiers::ALT),
                Code::KeyA,
            ),
            "Ctrl+Shift+Alt+A",
        ),
    ]
}

fn log_line(app: &AppHandle, msg: &str) {
    use std::io::Write;
    let Some(dir) = app.path().app_data_dir().ok() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 复用同一个文件句柄，避免每条日志都开关一次文件
    static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
    if let Ok(mut g) = LOG.lock() {
        if g.is_none() {
            *g = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("asheng.log"))
                .ok();
        }
        if let Some(f) = g.as_mut() {
            let _ = writeln!(f, "[{}] {}", ts, msg);
        }
    }
}

/// 开机自启：Windows 写当前用户 Run 键（普通账户也能写，不需要管理员）
#[cfg(windows)]
fn enable_autostart(_app: &AppHandle) -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = hkcu
        .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .map_err(|e| e.to_string())?;
    run.set_value("Asheng", &format!("\"{}\"", exe.display()))
        .map_err(|e| e.to_string())
}

/// 开机自启：macOS/Linux 交给插件处理
#[cfg(not(windows))]
fn enable_autostart(app: &AppHandle) -> Result<(), String> {
    app.autolaunch().enable().map_err(|e| e.to_string())
}

/// 允许在壳内导航的地址：本地页面 + 生产服务器同源；其余一律交给系统浏览器
fn is_internal_url(url: &tauri::Url) -> bool {
    if matches!(url.scheme(), "tauri" | "about" | "blob") {
        return true;
    }
    // Windows 下本地资源页是 http(s)://tauri.localhost
    if url.host_str() == Some("tauri.localhost") {
        return true;
    }
    let Ok(base) = MAIN_URL.parse::<tauri::Url>() else {
        return false;
    };
    url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default()
}

/// 登录页用紧凑窗口；进入 ERP 工作区后放大，退出登录再收回来
fn handle_main_size(app: &AppHandle, path: &str) {
    use tauri::{LogicalSize, Size};

    let is_login = path.starts_with("/erp/login");
    let is_app = path.starts_with("/erp/") && !is_login && path != "/erp/";

    if is_login {
        if MAIN_EXPANDED.swap(false, Ordering::SeqCst) {
            if let Some(w) = app.get_webview_window(MAIN_LABEL) {
                let _ = w.set_min_size(Some(Size::Logical(LogicalSize::new(900.0, 600.0))));
                let _ = w.set_size(Size::Logical(LogicalSize::new(960.0, 640.0)));
                let _ = w.center();
                log_line(app, "主窗口：已切回登录尺寸");
            }
        }
    } else if is_app && !MAIN_EXPANDED.swap(true, Ordering::SeqCst) {
        if let Some(w) = app.get_webview_window(MAIN_LABEL) {
            // 按屏幕工作区取一个舒服的比例：比登录窗口明显大，但不铺满屏幕
            let mut width = 1120.0_f64;
            let mut height = 740.0_f64;
            if let Ok(Some(mon)) = w.current_monitor() {
                let scale = mon.scale_factor();
                let msize = mon.size();
                let mw = msize.width as f64 / scale;
                let mh = msize.height as f64 / scale;
                width = (mw * 0.78).clamp(1020.0, 1320.0).min((mw - 80.0).max(960.0));
                height = (mh * 0.82).clamp(660.0, 860.0).min((mh - 100.0).max(620.0));
            }
            let _ = w.set_min_size(Some(Size::Logical(LogicalSize::new(1000.0, 680.0))));
            let _ = w.set_size(Size::Logical(LogicalSize::new(width, height)));
            let _ = w.center();
            log_line(
                app,
                &format!("主窗口：已切换到工作尺寸 {:.0}x{:.0}", width, height),
            );
        }
    }
}

/// 登录成功进入工作区：球贴到 ERP 右下角，面板按需停靠
fn on_workspace_enter(app: &AppHandle) {
    WORKSPACE.store(true, Ordering::SeqCst);
    ball_sync(app);
}

/* ------------------------- 阿盛面板停靠（B 方案） ------------------------- */

fn logical_rect(w: &WebviewWindow) -> Option<(f64, f64, f64, f64)> {
    let scale = w.scale_factor().ok()?;
    let p = w.outer_position().ok()?;
    let s = w.outer_size().ok()?;
    Some((
        p.x as f64 / scale,
        p.y as f64 / scale,
        s.width as f64 / scale,
        s.height as f64 / scale,
    ))
}

fn screen_w(w: &WebviewWindow) -> f64 {
    w.current_monitor()
        .ok()
        .flatten()
        .map(|m| m.size().width as f64 / m.scale_factor())
        .unwrap_or(1440.0)
}

fn screen_h(w: &WebviewWindow) -> f64 {
    w.current_monitor()
        .ok()
        .flatten()
        .map(|m| m.size().height as f64 / m.scale_factor())
        .unwrap_or(900.0)
}

/// 工作区高度：一屏约 84%，上下留边；避免窗口被拉到顶满屏幕
fn work_height(w: &WebviewWindow) -> f64 {
    let sh = screen_h(w);
    (sh * 0.84).clamp(560.0, 900.0).min(sh - 120.0)
}

/// 窗口外框比内容多出来的单侧宽度（Windows 的不可见缩放边框），拼合时要互相吃掉
fn border_w(w: &WebviewWindow) -> f64 {
    let s = w.scale_factor().unwrap_or(1.0);
    match (w.outer_size(), w.inner_size()) {
        (Ok(o), Ok(i)) => (o.width.saturating_sub(i.width)) as f64 / s / 2.0,
        _ => 0.0,
    }
}

/// 窗口底部看不见的边框（外框底 − 内容底）；停靠时用它把面板内容底边和 ERP 对齐
fn bottom_border(w: &WebviewWindow) -> f64 {
    let s = w.scale_factor().unwrap_or(1.0);
    match (
        w.outer_position(),
        w.outer_size(),
        w.inner_position(),
        w.inner_size(),
    ) {
        (Ok(op), Ok(o), Ok(ip), Ok(i)) => {
            let d = (op.y + o.height as i32) - (ip.y + i.height as i32);
            (d as f64 / s).max(0.0)
        }
        _ => 0.0,
    }
}

/// 工作区宽度（登录后 ERP 单独占用的宽度）
fn work_width(w: &WebviewWindow) -> f64 {
    let sw = screen_w(w);
    (sw * 0.78)
        .clamp(1020.0, 1320.0)
        .min((sw - 80.0).max(960.0))
}

/// 把 ERP 让出右侧宽度，面板贴到 ERP 右边
fn dock_layout(app: &AppHandle) {
    let (Some(main), Some(ask)) = (
        app.get_webview_window(MAIN_LABEL),
        app.get_webview_window(ASK_LABEL),
    ) else {
        return;
    };
    if DOCK_GUARD.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some((mx, my, _mw, mh)) = logical_rect(&main) {
        let scale = main.scale_factor().unwrap_or(1.0);
        let inner_h = main
            .inner_size()
            .ok()
            .map(|s| s.height as f64 / scale)
            .unwrap_or(mh);
        let sw = screen_w(&main);
        let sh = screen_h(&main);
        let main_w = (sw - DOCK_W - 40.0).clamp(880.0, 1320.0);
        // 主窗口被最大化时，直接 set_size 不生效（会一直顶满屏幕）；先还原再排布
        let was_max = main.is_maximized().unwrap_or(false);
        if was_max {
            let _ = main.unmaximize();
        }
        // 高度固定：一屏 84% 左右，上下留边；不跟随最大化窗口拉满
        let wh = work_height(&main);
        let target_h = if was_max { wh } else { inner_h.min(wh) };
        let total = main_w + DOCK_W;
        let x = if mx + total > sw - 8.0 {
            (sw - total - 8.0).max(8.0)
        } else {
            mx
        };
        // 垂直位置：默认沿用 ERP 当前位置；一旦触及上下边缘就回到留边位置
        let mut y0 = my;
        if was_max || y0 < 8.0 || y0 + target_h > sh - 8.0 {
            y0 = (sh - target_h - 72.0).max(8.0);
        }
        let _ = main.set_min_size(Some(tauri::Size::Logical(tauri::LogicalSize::new(720.0, 600.0))));
        let _ = main.set_size(tauri::Size::Logical(tauri::LogicalSize::new(main_w, target_h)));
        let _ = main.set_position(tauri::LogicalPosition::new(x, y0));
        if let Some((mx2, my2, mw2, mh2)) = logical_rect(&main) {
            let _ = ask.set_always_on_top(false);
            // ERP 底部有一段看不见的边框：面板要减掉它，两边内容底边才对得齐
            let dock_h = (mh2 - bottom_border(&main)).max(400.0);
            let _ = ask.set_size(tauri::Size::Logical(tauri::LogicalSize::new(DOCK_W, dock_h)));
            // 面板窗口本身有不可见的边框，按外框补齐，避免比 ERP 高出一截
            if let (Ok(o), Ok(i)) = (ask.outer_size(), ask.inner_size()) {
                let as_ = ask.scale_factor().unwrap_or(1.0);
                let dw = (o.width as f64 - i.width as f64) / as_;
                let dh = (o.height as f64 - i.height as f64) / as_;
                if dw > 0.5 || dh > 0.5 {
                    let _ = ask.set_size(tauri::Size::Logical(tauri::LogicalSize::new(
                        (DOCK_W - dw).max(320.0),
                        (dock_h - dh).max(400.0),
                    )));
                }
            }
            let ask_ow = ask
                .outer_size()
                .map(|s| s.width as f64 / ask.scale_factor().unwrap_or(1.0))
                .unwrap_or(DOCK_W);
            // 两个窗口各有一圈不可见边框，互相吃掉才看不出缝
            let gap = border_w(&main) + border_w(&ask);
            let mut fx = mx2;
            // 整组（ERP + 面板）超出屏幕右边就整体左移
            if fx + mw2 + ask_ow - gap > sw - 8.0 {
                fx = (sw - 8.0 - mw2 - ask_ow + gap).max(8.0);
                let _ = main.set_position(tauri::LogicalPosition::new(fx, my2));
            }
            let _ = ask.set_position(tauri::LogicalPosition::new(fx + mw2 - gap, my2));
            let _ = ask.show();
            log_line(
                app,
                &format!("已停靠到 ERP 右侧：ERP {:.0} + 面板 {:.0}", mw2, DOCK_W),
            );
        }
    }
    DOCK_GUARD.store(false, Ordering::SeqCst);
    ball_sync(app);
}

/// 分离：ERP 恢复宽度，面板回到浮动状态
fn undock_layout(app: &AppHandle) {
    let (Some(main), Some(ask)) = (
        app.get_webview_window(MAIN_LABEL),
        app.get_webview_window(ASK_LABEL),
    ) else {
        return;
    };
    if DOCK_GUARD.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some((mx, my, _mw, mh)) = logical_rect(&main) {
        let scale = main.scale_factor().unwrap_or(1.0);
        let inner_h = main
            .inner_size()
            .ok()
            .map(|s| s.height as f64 / scale)
            .unwrap_or(mh);
        let target = work_width(&main);
        let _ = main.set_size(tauri::Size::Logical(tauri::LogicalSize::new(target, inner_h)));
        let _ = main.set_min_size(Some(tauri::Size::Logical(tauri::LogicalSize::new(1000.0, 680.0))));
        let _ = main.set_position(tauri::LogicalPosition::new(mx, my));
        let _ = ask.set_always_on_top(true);
        log_line(app, "阿盛面板已从停靠分离");
    }
    DOCK_GUARD.store(false, Ordering::SeqCst);
    ball_sync(app);
}

/// 把跟随排布放进合并队列（50ms 最多执行一次），避免拖动/缩放时每个事件都挪窗
fn schedule_align(_app: &AppHandle) {
    if let Ok(g) = ALIGN_TX.lock() {
        if let Some(tx) = g.as_ref() {
            let _ = tx.send(());
        }
    }
}

/// ERP 移动/缩放时，停靠面板跟着走
fn align_dock(app: &AppHandle) {
    if !DOCKED.load(Ordering::SeqCst) || DOCK_GUARD.load(Ordering::SeqCst) {
        return;
    }
    let (Some(main), Some(ask)) = (
        app.get_webview_window(MAIN_LABEL),
        app.get_webview_window(ASK_LABEL),
    ) else {
        return;
    };
    if main.is_minimized().unwrap_or(false) {
        // 面板已独立：ERP 最小化时不再跟着隐藏
        return;
    }
    if !ask.is_visible().unwrap_or(false) {
        return;
    }
    if let Some((mx, my, mw, mh)) = logical_rect(&main) {
        DOCK_GUARD.store(true, Ordering::SeqCst);
        // 与 dock_layout 一致：减掉 ERP 底部不可见边框，两边内容底边对齐
        let dock_h = (mh - bottom_border(&main)).max(400.0);
        let _ = ask.set_size(tauri::Size::Logical(tauri::LogicalSize::new(DOCK_W, dock_h)));
        if let (Ok(o), Ok(i)) = (ask.outer_size(), ask.inner_size()) {
            let as_ = ask.scale_factor().unwrap_or(1.0);
            let dw = (o.width as f64 - i.width as f64) / as_;
            let dh = (o.height as f64 - i.height as f64) / as_;
            if dw > 0.5 || dh > 0.5 {
                let _ = ask.set_size(tauri::Size::Logical(tauri::LogicalSize::new(
                    (DOCK_W - dw).max(320.0),
                    (dock_h - dh).max(400.0),
                )));
            }
        }
        let _ = ask.set_position(tauri::LogicalPosition::new(
            mx + mw - (border_w(&main) + border_w(&ask)),
            my,
        ));
        DOCK_GUARD.store(false, Ordering::SeqCst);
    }
}

/// 面板被拖动时：靠近 ERP 右边缘就吸附停靠，拖远就分离
fn on_ask_moved(app: &AppHandle) {
    if DOCK_GUARD.load(Ordering::SeqCst) {
        return;
    }
    let (Some(main), Some(ask)) = (
        app.get_webview_window(MAIN_LABEL),
        app.get_webview_window(ASK_LABEL),
    ) else {
        return;
    };
    if main.is_minimized().unwrap_or(false) || !main.is_visible().unwrap_or(false)
        || !ask.is_visible().unwrap_or(false)
    {
        return;
    }
    let Some((mx, my, mw, mh)) = logical_rect(&main) else {
        return;
    };
    let Some((ax, ay, _aw, ah)) = logical_rect(&ask) else {
        return;
    };
    let right = mx + mw;
    if !DOCKED.load(Ordering::SeqCst) {
        let near = (ax - right).abs() <= 40.0;
        let overlap = ay < my + mh - 60.0 && ay + ah > my + 60.0;
        if near && overlap {
            DOCKED.store(true, Ordering::SeqCst);
            dock_layout(app);
        }
    } else if (ax - right).abs() > 90.0 {
        DOCKED.store(false, Ordering::SeqCst);
        undock_layout(app);
    }
}

/// ERP 关闭/最小化：面板与 ERP 解绑，保持可见并贴到屏幕最右侧
fn float_ask_right(app: &AppHandle) {
    let (Some(main), Some(ask)) = (
        app.get_webview_window(MAIN_LABEL),
        app.get_webview_window(ASK_LABEL),
    ) else {
        return;
    };
    if DOCKED.swap(false, Ordering::SeqCst) {
        // 停靠中：把宽度还给 ERP；最小化时不碰它，恢复时由 dock_layout 统一排
        if !main.is_minimized().unwrap_or(false) && !DOCK_GUARD.swap(true, Ordering::SeqCst) {
            if let Some((mx, my, _mw, mh)) = logical_rect(&main) {
                let scale = main.scale_factor().unwrap_or(1.0);
                let inner_h = main
                    .inner_size()
                    .ok()
                    .map(|s| s.height as f64 / scale)
                    .unwrap_or(mh);
                let target = work_width(&main);
                let _ = main.set_min_size(Some(tauri::Size::Logical(tauri::LogicalSize::new(1000.0, 680.0))));
                let _ = main.set_size(tauri::Size::Logical(tauri::LogicalSize::new(target, inner_h)));
                let _ = main.set_position(tauri::LogicalPosition::new(mx, my));
            }
            DOCK_GUARD.store(false, Ordering::SeqCst);
        }
        log_line(app, "ERP 不在：面板已解除停靠");
    }
    let _ = ask.set_always_on_top(true);
    // 只在面板本来开着时移动它，绝不因为 ERP 收起而把面板强行弹出来
    if !ask.is_visible().unwrap_or(false) {
        return;
    }
    let sw = screen_w(&ask);
    let sh = screen_h(&ask);
    if let Some((_ax, ay, aw, ah)) = logical_rect(&ask) {
        let x = (sw - aw - 20.0).max(8.0);
        let y = ay.clamp(8.0, (sh - ah - 8.0).max(8.0));
        let _ = ask.set_position(tauri::LogicalPosition::new(x, y));
    }
}

/// 收起停靠面板，把宽度还给 ERP
fn collapse_dock(app: &AppHandle) {
    if !DOCKED.load(Ordering::SeqCst) {
        return;
    }
    let (Some(main), Some(ask)) = (
        app.get_webview_window(MAIN_LABEL),
        app.get_webview_window(ASK_LABEL),
    ) else {
        return;
    };
    let _ = ask.hide();
    if DOCK_GUARD.swap(true, Ordering::SeqCst) {
        return;
    }
    restore_main_width(app);
    DOCK_GUARD.store(false, Ordering::SeqCst);
    log_line(app, "停靠面板已收起，ERP 恢复宽度");
    ball_sync(app);
}

/// 解除停靠后把 ERP 宽度还原（不依赖对话框窗口是否存在）
fn restore_main_width(app: &AppHandle) {
    let Some(main) = app.get_webview_window(MAIN_LABEL) else {
        return;
    };
    if let Some((mx, my, _mw, mh)) = logical_rect(&main) {
        let scale = main.scale_factor().unwrap_or(1.0);
        let inner_h = main
            .inner_size()
            .ok()
            .map(|s| s.height as f64 / scale)
            .unwrap_or(mh);
        let target = work_width(&main);
        let _ = main.set_min_size(Some(tauri::Size::Logical(tauri::LogicalSize::new(1000.0, 680.0))));
        let _ = main.set_size(tauri::Size::Logical(tauri::LogicalSize::new(target, inner_h)));
        let _ = main.set_position(tauri::LogicalPosition::new(mx, my));
    }
}

/* --------------------------- 悬浮球位置（贴 ERP / 回桌面） --------------------------- */

const BALL_SIZE: f64 = 72.0;

fn erp_up(app: &AppHandle) -> bool {
    app.get_webview_window(MAIN_LABEL)
        .map(|m| m.is_visible().unwrap_or(false) && !m.is_minimized().unwrap_or(false))
        .unwrap_or(false)
}

/// ERP 开着：球贴到 ERP 窗口内部右下角，浮在页面内容之上
fn ball_to_erp(app: &AppHandle) -> bool {
    let (Some(ball), Some(main)) = (
        app.get_webview_window(BALL_LABEL),
        app.get_webview_window(MAIN_LABEL),
    ) else {
        return false;
    };
    if !main.is_visible().unwrap_or(false) || main.is_minimized().unwrap_or(false) {
        return false;
    }
    let Some((mx, my, mw, mh)) = logical_rect(&main) else {
        return false;
    };
    // 球窗口比圆形本体宽（两侧有透明留白），按外框换算，让圆贴住右下角
    let s = ball.scale_factor().unwrap_or(1.0);
    let outer = ball
        .outer_size()
        .unwrap_or(tauri::PhysicalSize::new(131, 72));
    let ow = outer.width as f64 / s;
    let oh = outer.height as f64 / s;
    let x = mx + mw - 22.0 - (ow - BALL_SIZE) / 2.0 - BALL_SIZE;
    let y = my + mh - 26.0 - (oh - BALL_SIZE) / 2.0 - BALL_SIZE;
    let _ = ball.set_position(tauri::LogicalPosition::new(x, y));
    // 隐藏再显示后 Windows 可能丢掉置顶，重新压回最上层
    let _ = ball.set_always_on_top(true);
    if !ball.is_visible().unwrap_or(false) {
        let _ = ball.show();
    }
    true
}

/// ERP 不在：球回桌面右下角
fn ball_to_desktop(app: &AppHandle) {
    let Some(ball) = app.get_webview_window(BALL_LABEL) else {
        return;
    };
    if let Ok(Some(mon)) = ball.current_monitor() {
        let scale = mon.scale_factor();
        let msize = mon.size();
        let sw = msize.width as f64 / scale;
        let sh = msize.height as f64 / scale;
        let outer = ball
            .outer_size()
            .unwrap_or(tauri::PhysicalSize::new(BALL_SIZE as u32, BALL_SIZE as u32));
        let ow = outer.width as f64 / scale;
        let oh = outer.height as f64 / scale;
        let _ = ball.set_position(tauri::LogicalPosition::new(
            (sw - ow - 12.0).max(0.0),
            (sh - oh - 120.0).max(0.0),
        ));
    }
    let _ = ball.set_always_on_top(true);
    if !ball.is_visible().unwrap_or(false) {
        let _ = ball.show();
    }
}

/// 预热悬浮球：在屏幕外显示一小会儿，让 WebView2 先把页面和首帧渲染好。
/// 否则第一次显示时要等渲染进程冷启动，看起来就是“点了两三秒才出来”。
fn prewarm_ball(app: &AppHandle) {
    let Some(ball) = app.get_webview_window(BALL_LABEL) else {
        return;
    };
    let _ = ball.set_position(tauri::LogicalPosition::new(80.0, -400.0));
    let _ = ball.show();
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1200));
        if let Some(b) = handle.get_webview_window(BALL_LABEL) {
            let _ = b.hide();
        }
        ball_sync(&handle);
        log_line(&handle, "悬浮球预热完成");
    });
}

/// 按登录态和 ERP 是否在场，把球放到该在的位置
fn ball_sync(app: &AppHandle) {
    if !WORKSPACE.load(Ordering::SeqCst) {
        if let Some(b) = app.get_webview_window(BALL_LABEL) {
            if b.is_visible().unwrap_or(false) {
                let _ = b.hide();
            }
        }
        return;
    }
    // 面板开着时球不出现：停靠中，或 ERP 不在（关闭/最小化）都算
    let ask_visible = app
        .get_webview_window(ASK_LABEL)
        .map(|a| a.is_visible().unwrap_or(false))
        .unwrap_or(false);
    if ask_visible && (DOCKED.load(Ordering::SeqCst) || !erp_up(app)) {
        if let Some(b) = app.get_webview_window(BALL_LABEL) {
            if b.is_visible().unwrap_or(false) {
                let _ = b.hide();
            }
        }
        return;
    }
    if !ball_to_erp(app) {
        ball_to_desktop(app);
    }
}

fn navigation_guard(app: AppHandle) -> impl Fn(&tauri::Url) -> bool + Send + 'static {
    move |url| {
        let path = url.path();
        // 页面通过这个约定路径上报登录/工作状态，只用来切窗口尺寸，不放行导航
        if let Some(state) = path.strip_prefix("/ask/win/") {
            let fake = if state == "app" { "/erp/dashboard" } else { "/erp/login" };
            log_line(&app, &format!("窗口状态：{}", state));
            handle_main_size(&app, fake);
            if state == "app" {
                on_workspace_enter(&app);
            } else {
                // 回到登录页：球收起，停靠面板一并退出
                WORKSPACE.store(false, Ordering::SeqCst);
                if DOCKED.swap(false, Ordering::SeqCst) {
                    undock_layout(&app);
                    if let Some(a) = app.get_webview_window(ASK_LABEL) {
                        let _ = a.hide();
                    }
                }
                if let Some(b) = app.get_webview_window(BALL_LABEL) {
                    let _ = b.hide();
                }
            }
            return false;
        }
        // 阿盛回答里的深链接：切到 ERP 主窗口并打开对应页面
        if path == "/ask/open" {
            let mut target = String::new();
            for (k, v) in url.query_pairs() {
                if k == "p" {
                    target = v.into_owned();
                }
            }
            if target.starts_with("/erp") && !target.starts_with("//") {
                // 从面板跳到 ERP：面板按原逻辑收起，不触发自动回停靠
                ASK_REDOCK.store(false, Ordering::SeqCst);
                if let Some(main) = app.get_webview_window(MAIN_LABEL) {
                    let _ = main.show();
                    let _ = main.unminimize();
                    let _ = main.set_focus();
                    if let Ok(base) = MAIN_URL.parse::<tauri::Url>() {
                        let full = format!("{}{}", base.origin().ascii_serialization(), target);
                        if let Ok(u) = full.parse::<tauri::Url>() {
                            let _ = main.navigate(u);
                            log_line(&app, &format!("阿盛跳转 ERP：{}", target));
                        }
                    }
                }
                if let Some(a) = app.get_webview_window(ASK_LABEL) {
                    let _ = a.hide();
                }
            }
            return false;
        }
        if path.starts_with("/erp") {
            handle_main_size(&app, path);
            if path != "/erp/" && !path.starts_with("/erp/login") {
                on_workspace_enter(&app);
            }
        }
        // 小窗登录态探测：1=已登录，0=未登录
        if url.path() == "/ask/state/1" || url.path() == "/ask/state/0" {
            let q = url.query().unwrap_or("");
            log_line(
                &app,
                &format!("登录探测：{}{}", url.path(), if q.is_empty() { String::new() } else { format!("?{}", q) }),
            );
        }
        if url.path() == "/ask/state/1" {
            if let Some(w) = app.get_webview_window(ASK_LABEL) {
                if DOCKED.load(Ordering::SeqCst) {
                    let _ = w.show();
                    align_dock(&app);
                } else if WORKSPACE.load(Ordering::SeqCst) && erp_up(&app) {
                    // 已登录且 ERP 在场：默认开成左右分栏
                    DOCKED.store(true, Ordering::SeqCst);
                    dock_layout(&app);
                    let _ = w.set_focus();
                } else {
                    show_ask(&w);
                }
            }
            return false;
        }
        if url.path() == "/ask/state/0" {
            if let Some(w) = app.get_webview_window(ASK_LABEL) {
                let _ = w.hide();
            }
            let main_focused = app
                .get_webview_window(MAIN_LABEL)
                .and_then(|w| w.is_focused().ok())
                .unwrap_or(false);
            if !main_focused {
                show_main(&app);
                log_line(&app, "未登录：已切到主窗口");
            }
            return false;
        }
        // 面板拖动：无边框窗口由页面触发，交给系统原生拖动
        if url.path() == "/ask/drag" {
            if let Some(w) = app.get_webview_window(ASK_LABEL) {
                let _ = w.start_dragging();
            }
            return false;
        }
        // 小窗里的“去登录”走这个约定路径：收起小窗，把主窗口切到前台
        if url.path() == "/ask/focus-main" {
            if DOCKED.load(Ordering::SeqCst) {
                // 分栏状态点“ERP”图标：收起面板，球回到 ERP 右下角
                collapse_dock(&app);
                ball_sync(&app);
                return false;
            }
            if let Some(w) = app.get_webview_window(ASK_LABEL) {
                let _ = w.hide();
            }
            let main_focused = app
                .get_webview_window(MAIN_LABEL)
                .and_then(|w| w.is_focused().ok())
                .unwrap_or(false);
            if !main_focused {
                show_main(&app);
                log_line(&app, "小窗请求登录：已收起小窗并切到主窗口");
            }
            return false;
        }
        // 小窗里的关闭按钮走这个约定路径：不加载页面，直接把小窗收起来
        if url.path() == "/ask/close" {
            if DOCKED.load(Ordering::SeqCst) {
                collapse_dock(&app);
            } else if let Some(w) = app.get_webview_window(ASK_LABEL) {
                let _ = w.hide();
                log_line(&app, "快捷小窗已关闭");
            }
            // hide() 是异步生效的，稍等再按真实可见性把悬浮球放出来
            let h = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(80));
                ball_sync(&h);
            });
            return false;
        }
        if is_internal_url(url) {
            return true;
        }
        log_line(&app, &format!("站外链接交给系统浏览器：{}", url));
        let _ = app.opener().open_url(url.as_str(), None::<&str>);
        false
    }
}

/// 建主窗口：先显示本地启动页，由页面探测服务器后再跳转，启动不再被网络阻塞
fn create_main(app: &AppHandle) -> Option<WebviewWindow> {
    let target = format!(
        "window.__ASHENG_TARGET__ = {:?}; window.__ASHENG_CLIENT__ = true; {}",
        MAIN_URL, JS_WIN_STATE
    );
    let w = WebviewWindowBuilder::new(app, MAIN_LABEL, WebviewUrl::App("start.html".into()))
        .title("阿盛 · 盛峰门业")
        .inner_size(960.0, 640.0)
        .min_inner_size(900.0, 600.0)
        .center()
        .additional_browser_args(&browser_args())
        .initialization_script(target)
        .on_navigation(navigation_guard(app.clone()))
        .on_page_load(|webview, payload| {
            let _ = webview.eval(JS_NAV_FIX);
            let _ = webview.eval(JS_SCROLL_STYLE);
            log_line(
                webview.app_handle(),
                &format!("页面加载：{}", payload.url()),
            );
        })
        .build()
        .ok();
    w
}

/// Windows 上把悬浮球窗口裁成圆形：去掉白色方底，圆外点击穿透到桌面
#[cfg(windows)]
fn shape_ball_window(w: &WebviewWindow) {
    use windows_sys::Win32::Graphics::Gdi::{CreateEllipticRgn, SetWindowRgn};

    let Ok(hwnd) = w.hwnd() else { return };
    let Ok(size) = w.inner_size() else { return };
    let cx = (size.width / 2) as i32;
    let cy = (size.height / 2) as i32;
    let r = (size.height / 2) as i32;
    let rgn = unsafe { CreateEllipticRgn(cx - r, cy - r, cx + r, cy + r) };
    if !rgn.is_null() {
        unsafe { SetWindowRgn(hwnd.0, rgn, 1) };
    }
}

#[cfg(not(windows))]
fn shape_ball_window(_w: &WebviewWindow) {}

/// 把窗口提到最前，但不抢焦点（用于 ERP 与对话面板保持同层）
#[cfg(windows)]
fn raise_no_activate(w: &WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };

    if let Ok(hwnd) = w.hwnd() {
        unsafe {
            SetWindowPos(
                hwnd.0,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(not(windows))]
fn raise_no_activate(_w: &WebviewWindow) {}

/// 焦点互顶的节流：200ms 内只提升一次，避免快速切换时反复改 z 序
fn allow_raise() -> bool {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    if let Ok(mut g) = LAST.lock() {
        let now = std::time::Instant::now();
        if let Some(t) = *g {
            if now.duration_since(t) < Duration::from_millis(200) {
                return false;
            }
        }
        *g = Some(now);
        true
    } else {
        false
    }
}

/// 让悬浮球不抢焦点：否则第一次点击只会激活窗口，看起来像“点击无效”
#[cfg(windows)]
fn ball_no_activate(w: &WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
    };

    let Ok(hwnd) = w.hwnd() else { return };
    unsafe {
        let ex = GetWindowLongPtrW(hwnd.0, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd.0, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
    }
}

#[cfg(not(windows))]
fn ball_no_activate(_w: &WebviewWindow) {}

/// 桌面悬浮球：独立窗口，壳关掉也还在
fn create_ball(app: &AppHandle) -> Option<WebviewWindow> {
    let w = WebviewWindowBuilder::new(app, BALL_LABEL, WebviewUrl::App("ball.html".into()))
        .title("阿盛")
        .inner_size(72.0, 72.0)
        .max_inner_size(72.0, 72.0)
        .min_inner_size(72.0, 72.0)
        .additional_browser_args(&browser_args())
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .visible(false)
        .build()
        .ok()?;

    if let Ok(Some(mon)) = w.current_monitor() {
        let scale = mon.scale_factor();
        let msize = mon.size();
        let sw = msize.width as f64 / scale;
        let sh = msize.height as f64 / scale;
        // 用窗口实际外框定位，避免不同 DPI 下算偏跑到屏幕外
        let outer = w.outer_size().unwrap_or(tauri::PhysicalSize::new(72, 72));
        let ow = outer.width as f64 / scale;
        let oh = outer.height as f64 / scale;
        let x = (sw - ow - 12.0).max(0.0);
        let y = (sh - oh - 120.0).max(0.0);
        let _ = w.set_position(tauri::LogicalPosition::new(x, y));
    }
    // 登录页不显示球；进入工作区后由 ball_sync 摆到 ERP 右下角
    shape_ball_window(&w);
    ball_no_activate(&w);
    log_line(app, "悬浮球已创建");
    Some(w)
}

fn create_tray(app: &AppHandle) -> tauri::Result<MenuItem<tauri::Wry>> {
    // 防御：同名托盘已存在时先移除，避免托盘里出现两个图标
    let _ = app.remove_tray_by_id("asheng-tray");
    let open_i = MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
    let ask_i = MenuItem::with_id(app, "ask", "问一句（Ctrl+Alt+A）", true, None::<&str>)?;
    let ball_i = MenuItem::with_id(app, "ball", "显示/隐藏悬浮球", true, None::<&str>)?;
    let screen_i = MenuItem::with_id(app, "screen", "打开生产大屏（浏览器）", true, None::<&str>)?;
    let logout_i = MenuItem::with_id(app, "logout", "退出登录（换人）", true, None::<&str>)?;
    let check_i = MenuItem::with_id(app, "check", "检查更新", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open_i,
            &ask_i,
            &ball_i,
            &screen_i,
            &logout_i,
            &PredefinedMenuItem::separator(app)?,
            &check_i,
            &PredefinedMenuItem::separator(app)?,
            &quit_i,
        ],
    )?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("缺少托盘图标".into()))?;

    TrayIconBuilder::with_id("asheng-tray")
        .icon(icon)
        .tooltip("阿盛 · 盛峰门业")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "ask" => toggle_ask(app),
            "ball" => toggle_ball((*app).clone()),
            "screen" => {
                let _ = app.opener().open_url(SCREEN_URL, None::<&str>);
            }
            "logout" => logout_all(app),
            "check" => {
                let h = app.clone();
                std::thread::spawn(move || check_update(&h, true));
            }
            "quit" => quit_app(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(ask_i)
}

/// 退出应用：先摘掉托盘图标再退出，避免任务栏残留图标
fn quit_app(app: &AppHandle) {
    let _ = app.remove_tray_by_id("asheng-tray");
    app.exit(0);
}

/// 退出登录：清令牌、服务端会话和本机聊天记录，再重新加载（换人用）
fn logout_all(app: &AppHandle) {
    const JS: &str = r#"(function(){
  try{
    var keys=['sf-erp-token','sf_token','sf_user','sf-user','asg_history_v1','asg_alert_off_v1'];
    for(var i=0;i<keys.length;i++){ try{ localStorage.removeItem(keys[i]); }catch(e){} }
    var target=null;
    try{ target=window.__ASHENG_TARGET__?new URL(window.__ASHENG_TARGET__):null; }catch(e){}
    var onErp=!!(target && location.host===target.host);
    var reloaded=false;
    function reload(){ if(reloaded) return; reloaded=true; try{ location.reload(); }catch(e){} }
    if(onErp){
      try{
        fetch('/api/logout',{method:'POST',credentials:'same-origin',cache:'no-store'})
          .then(reload,reload);
        setTimeout(reload,1200);
        return;
      }catch(e){}
    }
    reload();
  }catch(e){}
})();"#;
    for label in [MAIN_LABEL, ASK_LABEL] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.eval(JS);
        }
    }
    log_line(app, "已退出登录：令牌、会话与聊天记录已清理");
    show_main(app);
}

/// 悬浮球点一下：开/关对话面板
#[tauri::command]
fn toggle_panel(app: AppHandle) {
    log_line(&app, "悬浮球点击");
    toggle_ask(&app);
}

/// 悬浮球显隐（托盘菜单用）
#[tauri::command]
fn toggle_ball(app: AppHandle) {
    if let Some(w) = app.get_webview_window(BALL_LABEL) {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            ball_sync(&app);
        }
    } else {
        create_ball(&app);
        ball_sync(&app);
    }
}

/// 悬浮球右键菜单：所有出口都在这儿，避免“退不出”
#[tauri::command]
fn ball_menu(app: AppHandle) {
    use tauri::menu::ContextMenu;

    let Ok(erp) = MenuItem::with_id(&app, "ball_erp", "打开 ERP", true, None::<&str>) else {
        return;
    };
    let Ok(ask) = MenuItem::with_id(&app, "ball_ask", "问一句", true, None::<&str>) else {
        return;
    };
    let Ok(hide) = MenuItem::with_id(&app, "ball_hide", "隐藏悬浮球", true, None::<&str>) else {
        return;
    };
    let Ok(quit) = MenuItem::with_id(&app, "ball_quit", "退出阿盛（完全退出）", true, None::<&str>) else {
        return;
    };

    let menu = Menu::with_items(&app, &[&erp, &ask, &hide, &quit]);
    let Ok(menu) = menu else { return };
    if let Some(w) = app.get_webview_window(BALL_LABEL) {
        let win = w.as_ref().window().clone();
        match menu.popup(win) {
            Ok(_) => log_line(&app, "悬浮球菜单已弹出"),
            Err(e) => log_line(&app, &format!("悬浮球菜单弹出失败：{}", e)),
        }
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN_LABEL) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        // 关闭/最小化前是停靠的：ERP 回来后自动恢复停靠
        if ASK_REDOCK.swap(false, Ordering::SeqCst) {
            let app2 = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                let up = app2
                    .get_webview_window(MAIN_LABEL)
                    .map(|m| m.is_visible().unwrap_or(false) && !m.is_minimized().unwrap_or(false))
                    .unwrap_or(false);
                if up {
                    dock_layout(&app2);
                }
            });
        }
        // ERP 回到前台：球贴回它右下角
        if WORKSPACE.load(Ordering::SeqCst) {
            ball_sync(app);
        }
    }
}

fn toggle_ask(app: &AppHandle) {
    // 脏状态兜底：停靠标记还在但对话框已不存在 → 复位并还原 ERP 宽度，继续走创建流程
    if DOCKED.load(Ordering::SeqCst) && app.get_webview_window(ASK_LABEL).is_none() {
        DOCKED.store(false, Ordering::SeqCst);
        restore_main_width(app);
        log_line(app, "停靠状态残留，已复位");
    }
    // 已停靠：热键就是收起/展开右侧面板，ERP 宽度跟着伸缩
    if DOCKED.load(Ordering::SeqCst) {
        if !erp_up(app) {
            // ERP 已最小化/收起：改回浮窗
            DOCKED.store(false, Ordering::SeqCst);
            if let Some(w) = app.get_webview_window(ASK_LABEL) {
                show_ask(&w);
            }
            return;
        }
        if let Some(w) = app.get_webview_window(ASK_LABEL) {
            if w.is_visible().unwrap_or(false) {
                collapse_dock(app);
                ball_sync(app);
            } else {
                dock_layout(app);
                let _ = w.set_focus();
                log_line(app, "停靠面板已展开");
            }
        }
        return;
    }
    if let Some(w) = app.get_webview_window(ASK_LABEL) {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
            // 面板收起后，球按 ERP 是否在场重新出现
            ball_sync(app);
        } else {
            // 先把登录态探出来：已登录才显示，未登录切主窗口
            let _ = w.eval(JS_ASK_AUTH_NOW);
        }
        return;
    }
    // 同步命令/事件里直接建 WebView 在 Windows 上会死锁，放到后台线程
    if ASK_CREATING.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        log_line(&handle, "创建快捷小窗（后台线程）");
        let created = create_ask(&handle);
        ASK_CREATING.store(false, Ordering::SeqCst);
        match created {
            Some(_) => log_line(&handle, "快捷小窗已创建，等待登录状态"),
            None => log_line(&handle, "快捷小窗创建失败"),
        }
    });
}

fn create_ask(app: &AppHandle) -> Option<WebviewWindow> {
    let w = WebviewWindowBuilder::new(
        app,
        ASK_LABEL,
        WebviewUrl::External(ASK_URL.parse().expect("小窗地址格式错误")),
    )
    .title("问阿盛")
    .inner_size(440.0, 780.0)
    .min_inner_size(360.0, 560.0)
    .max_inner_size(640.0, 1100.0)
    .additional_browser_args(&browser_args())
    .on_navigation(navigation_guard(app.clone()))
    .decorations(false)
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .resizable(true)
    .visible(false)
    .on_page_load(|webview, _payload| {
        // 页面加载完先探测登录态，再决定显示小窗还是切主窗口
        let _ = webview.eval(JS_ASK_AUTH);
    })
    .build()
    .ok()?;
    Some(w)
}

fn show_ask(w: &WebviewWindow) {
    // 已停靠：只显示并按 ERP 对齐，不改变停靠布局
    if DOCKED.load(Ordering::SeqCst) {
        let _ = w.show();
        align_dock(w.app_handle());
        return;
    }
    // 贴当前屏幕右下角，高度占屏幕 84%，比原来更高一截
    let mut width = 440.0_f64;
    let mut height = 780.0_f64;
    let mut x = 20.0_f64;
    let mut y = 20.0_f64;
    if let Ok(Some(mon)) = w.current_monitor() {
        let scale = mon.scale_factor();
        let msize = mon.size();
        let sw = msize.width as f64 / scale;
        let sh = msize.height as f64 / scale;
        width = 440.0_f64.min(sw - 48.0);
        height = (sh * 0.84).clamp(560.0, 900.0).min(sh - 120.0);
        x = (sw - width - 20.0).max(0.0);
        // 面板底部留出悬浮球的位置
        y = (sh - height - 72.0).max(0.0);
    }
    ASK_ARMED.store(false, Ordering::SeqCst);
    let _ = w.show();
    let _ = w.set_size(tauri::LogicalSize::new(width, height));
    let _ = w.set_position(tauri::LogicalPosition::new(x, y));
    let _ = w.set_focus();
    log_line(w.app_handle(), "快捷小窗已显示");
    // 面板开着时球让位：停靠中或 ERP 不在都不显示
    ball_sync(w.app_handle());
}

fn http_get(url: &str) -> Option<String> {
    ureq::get(url)
        .timeout(Duration::from_secs(6))
        .call()
        .ok()?
        .into_string()
        .ok()
}

fn is_newer(latest: &str, current: &str) -> bool {
    fn parse(s: &str) -> Vec<u32> {
        s.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|x| x.parse().unwrap_or(0))
            .collect()
    }
    let (a, b) = (parse(latest), parse(current));
    for i in 0..3 {
        let x = *a.get(i).unwrap_or(&0);
        let y = *b.get(i).unwrap_or(&0);
        if x != y {
            return x > y;
        }
    }
    false
}

const DOWNLOAD_PAGE: &str = "https://43.142.55.26:8668/download/";

fn check_update(app: &AppHandle, manual: bool) {
    #[cfg(target_os = "macos")]
    {
        check_update_mac(app, manual);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            use tauri_plugin_updater::UpdaterExt;
            let updater = match app.updater() {
                Ok(u) => u,
                Err(err) => {
                    if manual {
                        app.dialog()
                            .message(format!("检查更新失败：{}", err))
                            .title("检查更新")
                            .blocking_show();
                    }
                    return;
                }
            };
            match updater.check().await {
                Ok(Some(update)) => {
                    let notes = update.body.clone().unwrap_or_default();
                    let go = app
                        .dialog()
                        .message(format!(
                            "发现新版本 {}（当前 {}）\n\n{}\n\n现在下载并自动安装吗？安装完成后会自动重启。",
                            update.version, update.current_version, notes
                        ))
                        .title("阿盛有更新")
                        .buttons(MessageDialogButtons::OkCancelCustom(
                            "立即更新".into(),
                            "稍后".into(),
                        ))
                        .blocking_show();
                    if !go {
                        return;
                    }
                    let _ = app
                        .dialog()
                        .message("正在下载更新，完成后会自动安装并重启，请稍候…")
                        .title("正在更新")
                        .show(|_| {});
                    if let Err(err) = update.download_and_install(|_, _| {}, || {}).await {
                        app.dialog()
                            .message(format!(
                                "更新失败：{}\n\n可以打开下载页手动安装：{}",
                                err, DOWNLOAD_PAGE
                            ))
                            .title("更新失败")
                            .blocking_show();
                    }
                }
                Ok(None) => {
                    if manual {
                        app.dialog()
                            .message(format!("当前已是最新版本（{}）。", env!("CARGO_PKG_VERSION")))
                            .title("检查更新")
                            .blocking_show();
                    }
                }
                Err(err) => {
                    if manual {
                        app.dialog()
                            .message(format!("检查更新失败：{}", err))
                            .title("检查更新")
                            .blocking_show();
                    }
                }
            }
        });
    }
}

/// macOS：自动下载 dmg 到「下载」文件夹并打开，由用户拖进「应用程序」完成安装。
/// 不做自动替换，避免未公证的包在替换后被系统拦住、导致软件打不开。
#[cfg(target_os = "macos")]
fn check_update_mac(app: &AppHandle, manual: bool) {
    let current = env!("CARGO_PKG_VERSION");
    let body = match http_get(VERSION_URL_MAC) {
        Some(b) => b,
        None => {
            if manual {
                app.dialog()
                    .message("检查更新失败：连不上服务器，请确认网络。")
                    .title("检查更新")
                    .blocking_show();
            }
            return;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return,
    };
    let latest = v.get("version").and_then(|x| x.as_str()).unwrap_or("");
    let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
    let notes = v.get("notes").and_then(|x| x.as_str()).unwrap_or("");

    if latest.is_empty() || !is_newer(latest, current) {
        if manual {
            app.dialog()
                .message(format!("当前已是最新版本（{}）。", current))
                .title("检查更新")
                .blocking_show();
        }
        return;
    }
    let go = app
        .dialog()
        .message(format!(
            "发现新版本 {}（当前 {}）\n\n{}\n\n下载安装包到「下载」文件夹吗？下载完成后会自动打开，按提示拖进「应用程序」即可。",
            latest, current, notes
        ))
        .title("阿盛有更新")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "下载更新".into(),
            "稍后".into(),
        ))
        .blocking_show();
    if !go || url.is_empty() {
        return;
    }

    let url = url.to_string();
    let app2 = app.clone();
    std::thread::spawn(move || {
        let _ = app2
            .dialog()
            .message("正在下载安装包，完成后会自动打开…")
            .title("正在下载更新")
            .show(|_| {});
        let name = url
            .rsplit('/')
            .next()
            .filter(|n| n.ends_with(".dmg"))
            .unwrap_or("Asheng.dmg")
            .to_string();
        let home = std::env::var("HOME").unwrap_or_default();
        let dir = std::path::PathBuf::from(home).join("Downloads");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let result = (|| -> Result<(), String> {
            let resp = ureq::get(&url)
                .timeout(Duration::from_secs(180))
                .call()
                .map_err(|e| e.to_string())?;
            let mut reader = resp.into_reader();
            let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
            std::io::copy(&mut reader, &mut file).map_err(|e| e.to_string())?;
            Ok(())
        })();
        match result {
            Ok(_) => {
                let _ = app2
                    .opener()
                    .open_path(path.to_string_lossy().to_string(), None::<&str>);
                app2.dialog()
                    .message("安装包已下载并打开。\n\n请把 Asheng 拖进「应用程序」，选择“替换”，然后重新打开。\n\n首次打开如果被系统拦截，请到「系统设置 → 隐私与安全性」点“仍要打开”。")
                    .title("更新已下载")
                    .blocking_show();
            }
            Err(err) => {
                app2.dialog()
                    .message(format!(
                        "下载失败：{}\n\n可以打开下载页手动下载：{}",
                        err, DOWNLOAD_PAGE
                    ))
                    .title("更新失败")
                    .blocking_show();
            }
        }
    });
}
