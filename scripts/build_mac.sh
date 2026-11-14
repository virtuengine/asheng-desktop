#!/bin/sh
# 在 macOS 上构建阿盛桌面端通用安装包（Intel + Apple Silicon）
#
# 环境要求：macOS 12+、Xcode Command Line Tools、Rust、Node.js 18+
# 签名策略：ad-hoc 签名（tauri.conf.json 里 signingIdentity = "-"），不做公证。
#          用户首次打开需到「系统设置 → 隐私与安全性 → 仍要打开」。
#
# 用法：把整个 asheng-desktop 目录拷到 Mac，执行
#   sh scripts/build_mac.sh
# 产物：src-tauri/target/universal-apple-darwin/release/bundle/dmg/*.dmg

set -e
cd "$(dirname "$0")/.."

echo "== 环境检查 =="
command -v xcode-select >/dev/null 2>&1 || { echo "缺少 Xcode Command Line Tools：xcode-select --install"; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "缺少 Rust：https://rustup.rs"; exit 1; }
command -v npm >/dev/null 2>&1 || { echo "缺少 Node.js 18+"; exit 1; }

echo "== 安装 Rust 双架构目标 =="
rustup target add aarch64-apple-darwin x86_64-apple-darwin

echo "== 安装前端依赖 =="
if [ -f package-lock.json ]; then
  npm ci
else
  npm install
fi

echo "== 构建通用 dmg =="
npx tauri build --target universal-apple-darwin --bundles dmg

DMG=$(find src-tauri/target/universal-apple-darwin/release/bundle/dmg -name '*.dmg' 2>/dev/null | head -1)
if [ -z "$DMG" ]; then
  echo "构建完成但没找到 dmg，请检查上面的输出"
  exit 1
fi

echo
echo "== 产物 =="
ls -lh "$DMG"
echo "SHA256:"
shasum -a 256 "$DMG"
echo
echo "下一步：把 dmg 上传到服务器 /www/wwwroot/sf-dashboard.brandis.dev/dl/desktop/"
echo "并更新 latest-mac.json（version / url / sha256 / size / date / notes）。"
