# 阿盛桌面端（Asheng Desktop）

盛峰门业生产数据 + 阿盛智能体的 Windows 客户端。第一版（P0）只做“更好用的入口”，不重写网页。

## 它做什么

- 主窗口直接打开 `https://43.142.55.26:8668/`，大屏、ERP、阿盛对话都在里面
- 点关闭只收进右下角托盘，不退出
- `Ctrl+Alt+A` 随时唤起“问一句”小窗，失焦自动收起
- 开机自启（可在系统“启动应用”里关掉）
- 启动时检查新版本

## 环境要求

- Windows 10 1803+ / Windows 11
- WebView2 运行时（Win11 自带；Win10 装过 Edge 的也有）
- 构建机：Node 18+、Rust stable（MSVC 工具链）、Visual Studio 生成工具（C++ 桌面开发）

## 开发

```powershell
npm install
npm run dev
```

## 打安装包

```powershell
npm run build
```

产物：

- `src-tauri/target/release/asheng-desktop.exe`（免安装，可直接拷走）
- `src-tauri/target/release/bundle/nsis/Asheng_0.1.0_x64-setup.exe`（安装包）

图标源文件是根目录的 `app-icon.png`，改动后执行 `npm run icon` 重新生成全套图标。

## 发版流程

1. 改 `package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json` 三处版本号（保持一致）
2. `npm run build`
3. 把 `Asheng_x.y.z_x64-setup.exe` 上传到服务器：
   `ubuntu@43.142.55.26:/www/wwwroot/sf-dashboard.brandis.dev/dl/desktop/`
4. 更新同目录下的 `latest.json`：

```json
{
  "version": "0.2.0",
  "url": "https://43.142.55.26:8668/dl/desktop/Asheng_0.2.0_x64-setup.exe",
  "notes": "本次更新内容，一行一条"
}
```

5. 客户端下次启动会自动提示，也可以从托盘菜单“检查更新”手动触发

## 代码结构

```
src-tauri/src/main.rs      全部逻辑（窗口、托盘、快捷键、更新检查）
src/start.html             启动页：探测服务器，连不上时留在本页重试
server/ask/index.html      服务端纯对话页（部署到 /ask/，悬浮球小窗加载）
src-tauri/icons/           全套图标（由 app-icon.png 生成）
```

## 注意

- 网页与桌面壳之间**没有**开放任何本地能力通道，页面拿不到文件、命令、注册表。
- 登录状态由 WebView2 自己保存（`%LOCALAPPDATA%\com.shengfeng.asheng`），换机器需要重新输一次密码。
- 安装包未做代码签名，首次运行 Windows 会提示“未知发布者”，点“更多信息 → 仍要运行”即可；采购证书后签一下就没有这个提示。
- macOS 版需要在 macOS 机器或云端 macOS 构建机上打包，本仓库配置可直接复用。
