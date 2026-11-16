"""阿盛语音本地服务（asr-host）

在办公笔记本本地完成唤醒词与语音识别，音频不出本机。
桌面端通过 127.0.0.1 调用，只做「把话变成文字」，对话仍走 /agent-api/chat。

接口：
  GET  /health              服务与模型状态
  POST /wake   {"on":true}  开/关唤醒词「阿胜阿胜」
  POST /ptt    {"on":true}  按住说话：开始
  POST /ptt    {"on":false} 按住说话：结束并返回识别文本
  GET  /events?since=0      拉取唤醒/识别事件（桌面端轮询）
  POST /shutdown            退出服务
"""

import argparse
import base64
import copy
import hashlib
import io
import json
import os
import queue
import random
import re
import sys
import tempfile
import threading
import time
import urllib.request
import uuid
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlencode, urlparse

import numpy as np
import sherpa_onnx
import sounddevice as sd

VERSION = "0.1.0"
SAMPLE_RATE = 16000
BLOCK = 512
PORT = 18095
PTT_MAX_SECONDS = 30.0
# 连续对话：说完一句继续收音，这么久没人开口才退出
CONV_IDLE_SECONDS = 20.0
CONV_MAX_MISSES = 2
# 播报期间听到这些词就立刻停止播报，转去听新问题
INTERRUPT_WORDS = ("阿胜", "停一下", "等一下", "别说了", "打住")
# 明确的打断指令：这些词在回答正文里几乎不会出现
STOP_WORDS = ("停一下", "等一下", "别说了", "打住", "不用了", "暂停")
# 出现这些词说明在问业务，一律不判退出（第 0 层保护）
BIZ_WORDS = (
    "产线", "产量", "良率", "订单", "库存", "压型", "筷子", "热狗棒", "多少",
    "几个", "几号", "今天", "昨天", "这个月", "上周", "进度", "交期", "次品",
    "返工", "排产", "报表", "数据", "统计", "查一下", "看一下",
)
# 必须认得的核心退出词（原来那 12 个一个不少）
EXIT_WORDS = (
    "退出", "停吧", "不聊了", "结束吧", "再见", "先这样", "走了",
    "退下", "退朝", "朕乏了", "跪安", "散了吧",
)
# 固定告别句式，命中即退出（第 2 层）
EXIT_PHRASES = (
    "就这样吧", "先到这", "先到这里", "回头再说", "有事再找你", "有事再叫我",
    "我去忙了", "去忙了", "先忙去了", "改天聊", "不打扰你了", "先不聊了", "先这样吧",
    "那就这样", "先挂了", "拜拜",
)
# 第 3 层：短句 + 告别动词 + 收尾语气
EXIT_TAILS = ("吧", "了", "啦", "啊")
EXIT_VERBS = ("退下", "退朝", "跪安", "散", "走", "停", "结束", "不聊", "再见", "歇")
QUESTION_WORDS = ("吗", "多少", "怎么", "什么", "几", "哪", "为什么", "是不是")
MAX_EXIT_LEN = 12
# 距上一轮对话超过这么久，唤醒时才加角色称呼，免得每句都喊称呼显得假
ROLE_IDLE_SECONDS = 1800
# 用户近场说话的能量门槛；播报从扬声器绕回麦克风，能量达不到这个量级
INTERRUPT_RMS = 0.05

# 以上都是兜底默认值；在线时以服务端 /agent-api/voice/config 下发的为准
DEFAULT_RULES = {
    "version": "",
    "ack": ["在", "我在", "来了", "您说", "听着呢", "嗯，您讲", "请讲", "好，我在听",
            "来了，您说", "嗯", "我在呢", "您吩咐"],
    "bye": ["好嘞，臣告退", "遵旨，有事您再传我", "那您歇着，随叫随到", "退下啦，需要时喊我一声",
            "得嘞，我候着", "您忙，我在边上候着", "好，先告退", "有事随时叫我",
            "臣告退，您保重", "明白，我先退下"],
    "roles": {"chairman": "董事长", "director": "总监", "manager": "经理", "staff": "同事"},
    # 时段问候：每个时段每天第一次唤醒说一次
    "greetings": [
        {"start": 5, "end": 11, "texts": ["早上好", "早上好呀"]},
        {"start": 11, "end": 13, "texts": ["中午好", "中午好"]},
        {"start": 13, "end": 18, "texts": ["下午好", "下午好"]},
        {"start": 18, "end": 23, "texts": ["晚上好", "晚上好"]},
        {"start": 23, "end": 5, "texts": ["夜深了", "这么晚还没休息"]},
    ],
    "exit": {
        "biz": list(BIZ_WORDS),
        "words": list(EXIT_WORDS),
        "phrases": list(EXIT_PHRASES),
        "verbs": list(EXIT_VERBS),
        "tails": list(EXIT_TAILS),
        "questions": list(QUESTION_WORDS),
        "max_len": MAX_EXIT_LEN,
        "short_len": 8,
    },
    "interrupt": {
        "words": list(INTERRUPT_WORDS),
        "stop": list(STOP_WORDS),
        "rms": INTERRUPT_RMS,
    },
    "conversation": {
        "idle": CONV_IDLE_SECONDS,
        "misses": CONV_MAX_MISSES,
        "role_idle": ROLE_IDLE_SECONDS,
    },
    "tts": {"voice": "zh-CN-YunxiaNeural", "rate": "+0%"},
}


def exit_decision(text: str, rules: dict | None = None) -> tuple[bool, str]:
    """分层判断并返回命中层，埋点里用来统计是哪一层在生效。"""
    r = (rules or DEFAULT_RULES)["exit"]
    t = text.strip().strip("，,。.！!？?、~ ")
    if not t:
        return False, "空输入"
    if any(w in t for w in r["biz"]):
        return False, "业务保护词"
    if len(t) > r["max_len"]:
        return False, "超出长度"
    if any(p in t for p in r["phrases"]):
        return True, "告别句式"
    if any(w in t for w in r["words"]):
        return True, "核心退出词"
    if any(q in t for q in r["questions"]):
        return False, "疑问词保护"
    if len(t) <= r["short_len"] and any(v in t for v in r["verbs"]) and t[-1] in r["tails"]:
        return True, "短句判定"
    return False, "未命中"


def is_exit_intent(text: str, rules: dict | None = None) -> bool:
    """分层判断用户是不是在结束对话；业务问句优先，宁可不退也不误退。"""
    return exit_decision(text, rules)[0]


# 识别由服务端中转（腾讯云 / 阿里云），本地只做录音和静音检测
DEFAULT_ASR_ENDPOINT = "https://43.142.55.26:8668/agent-api/asr"
# 规则多久拉一次；后台改完最多这么久生效
RULES_REFRESH_SECONDS = 60.0


def to_wav(samples: np.ndarray, rate: int = SAMPLE_RATE) -> bytes:
    """把采集到的浮点音频封装成 16k 单声道 PCM wav。"""
    pcm = (np.clip(samples, -1.0, 1.0) * 32767).astype(np.int16)
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(pcm.tobytes())
    return buf.getvalue()


_SSL_CONTEXT = None


def ssl_context():
    """macOS 的 Python 默认找不到系统 CA，优先用随包携带的 certifi。"""
    global _SSL_CONTEXT
    if _SSL_CONTEXT is not None:
        return _SSL_CONTEXT
    import ssl

    try:
        import certifi

        _SSL_CONTEXT = ssl.create_default_context(cafile=certifi.where())
    except Exception:
        _SSL_CONTEXT = ssl.create_default_context()
    return _SSL_CONTEXT


# ---------- 端侧埋点 ----------
# 只记时延、结果、命中层；不上传音频，也不上传对话原文。
METRIC_FLUSH_SECONDS = 30.0
METRIC_BATCH = 100
APP_VERSION = os.environ.get("ASHENG_APP_VERSION", "").strip()


def _metric_dir() -> Path:
    """埋点队列与设备标识的落盘目录：优先用桌面端传进来的数据目录。"""
    for env in ("ASHENG_METRIC_DIR", "ASHENG_DATA_DIR"):
        raw = os.environ.get(env, "").strip()
        if not raw:
            continue
        try:
            path = Path(raw)
            path.mkdir(parents=True, exist_ok=True)
            return path
        except Exception:
            continue
    base = os.environ.get("LOCALAPPDATA") or os.environ.get("APPDATA") or ""
    if base:
        try:
            path = Path(base) / "asheng-desktop"
            path.mkdir(parents=True, exist_ok=True)
            return path
        except Exception:
            pass
    try:
        path = Path(sys.executable).resolve().parent / "data"
        path.mkdir(parents=True, exist_ok=True)
        return path
    except Exception:
        return Path(tempfile.gettempdir())


class Metrics:
    """先落本地文件再批量上报：断网时留在本地，恢复后自动补发。"""

    def __init__(self) -> None:
        self.dir = _metric_dir()
        self.queue_file = self.dir / "voice-metrics.jsonl"
        self.id_file = self.dir / "client-id"
        self.lock = threading.RLock()
        self.client_id = self._load_id()
        self.sent = 0
        self.failed = 0
        self.last_error = ""
        self.last_ok_at = 0.0

    def _load_id(self) -> str:
        try:
            raw = self.id_file.read_text(encoding="utf-8").strip()
            if raw:
                return raw
        except Exception:
            pass
        value = uuid.uuid4().hex[:16]
        try:
            self.id_file.write_text(value, encoding="utf-8")
        except Exception:
            pass
        return value

    def add(self, event: str, **fields) -> None:
        item = {"event": event, "ts": round(time.time(), 3)}
        for key, value in fields.items():
            if value is None or value == "":
                continue
            item[key] = value
        line = json.dumps(item, ensure_ascii=False)
        with self.lock:
            try:
                with self.queue_file.open("a", encoding="utf-8") as fh:
                    fh.write(line + "\n")
            except Exception as exc:
                self.last_error = f"写入失败 {exc}"

    def pending(self) -> int:
        try:
            with self.queue_file.open("r", encoding="utf-8") as fh:
                return sum(1 for line in fh if line.strip())
        except Exception:
            return 0

    def flush(self, endpoint: str, token: str) -> bool:
        if not endpoint or not token:
            return False
        with self.lock:
            try:
                lines = [line for line in self.queue_file.read_text(encoding="utf-8").splitlines()
                         if line.strip()]
            except Exception:
                return False
            if not lines:
                return True
            # 长期离线兜底：只保留最近的 2000 条，避免队列无限增长
            if len(lines) > 5000:
                self._drop(len(lines) - 2000)
                lines = lines[-2000:]
            batch = lines[:METRIC_BATCH]
        events = []
        for line in batch:
            try:
                events.append(json.loads(line))
            except Exception:
                continue
        if not events:
            self._drop(len(batch))
            return True
        body = json.dumps({
            "client_id": self.client_id,
            "app_version": APP_VERSION,
            "events": events,
        }, ensure_ascii=False).encode("utf-8")
        req = urllib.request.Request(
            endpoint,
            data=body,
            method="POST",
            headers={"Content-Type": "application/json", "Authorization": "Bearer " + token},
        )
        try:
            with urllib.request.urlopen(req, timeout=15, context=ssl_context()) as resp:
                resp.read()
        except Exception as exc:
            with self.lock:
                self.failed += 1
                self.last_error = f"{type(exc).__name__}: {exc}"
            return False
        self._drop(len(batch))
        with self.lock:
            self.sent += len(events)
            self.last_error = ""
            self.last_ok_at = time.time()
        return True

    def _drop(self, count: int) -> None:
        with self.lock:
            try:
                lines = self.queue_file.read_text(encoding="utf-8").splitlines()
            except Exception:
                return
            keep = lines[count:]
            tmp = self.queue_file.with_suffix(".tmp")
            try:
                tmp.write_text("\n".join(keep) + ("\n" if keep else ""), encoding="utf-8")
                os.replace(tmp, self.queue_file)
            except Exception as exc:
                self.last_error = f"清理失败 {exc}"


# 固定业务词的常见同音误识，统一改回正确写法
FIXES = (
    ("压行线", "压型线"),
    ("鸭行线", "压型线"),
    ("压形线", "压型线"),
    ("鸭形线", "压型线"),
    ("亚型线", "压型线"),
    ("热狗邦", "热狗棒"),
    ("筷子现", "筷子线"),
    ("筷子县", "筷子线"),
)


def normalize(text: str) -> str:
    text = text.strip()
    for wrong, right in FIXES:
        text = text.replace(wrong, right)
    return text


class AsrEngine:
    def __init__(self, models: Path, threads: int = 2):
        self.models = models
        self.threads = threads
        self.lock = threading.Lock()
        self.events: list[dict] = []
        self.seq = 0
        self.wake_on = False
        self.ptt_active = False
        self.capturing = False
        self.ptt_buffer: list[np.ndarray] = []
        self.capture_buffer: list[np.ndarray] = []
        self.capture_started = 0.0
        self.conv_misses = 0
        self.queue: queue.Queue = queue.Queue()
        self.stream: sd.InputStream | None = None
        self.running = True
        self.last_error = ""
        self.on_shutdown = None
        # 阿盛正在播报回答时不再直接丢弃音频，而是只认打断词，避免把自己的声音当成提问
        self.muted = False
        self.interrupt_vad = None
        self.last_heard = ""
        self.last_rms = 0.0
        self.last_asr_error = ""

        # 识别改由服务端中转，本地只留录音和静音检测，启动更快、内存更省
        self.token = ""
        self.endpoint = DEFAULT_ASR_ENDPOINT
        self.online = False
        # 交互规则（词库/话术/阈值）由服务端下发，本地只做兜底
        self.rules = copy.deepcopy(DEFAULT_RULES)
        self.rules_error = ""
        self.rules_at = 0.0

        self.vad = self._new_vad()
        self.kws = None
        self._kws_stream = None
        self.wake_vad = None
        # 唤醒后的应答音，让用户明确知道可以接着说了
        self._init_voice()
        # 埋点：只统计时延与命中结果，音频和对话原文都不外传
        self.metrics = Metrics()
        self.session_id = ""
        self.session_started = 0.0
        self.session_turns = 0
        self.session_empty = 0
        self.last_interrupt_at = 0.0
        self.text_since_interrupt = False
        self.muted_at = 0.0
        self.ack_started_at = 0.0
        self.last_asr_ms = 0
        threading.Thread(target=self._rules_loop, daemon=True).start()
        threading.Thread(target=self._metrics_loop, daemon=True).start()

    def _init_voice(self) -> None:
        base = self.models / "voice"
        self.ack_files = sorted((base / "ack").glob("*.wav"))
        self.role_files = {p.stem: p for p in (base / "role").glob("*.wav")}
        self.bye_files = sorted((base / "bye").glob("*.wav"))
        # 后台改了话术又不想发版时，现场合成的小音频缓存到这里
        self.tts_dir = base / "tts"
        try:
            self.tts_dir.mkdir(parents=True, exist_ok=True)
        except Exception as exc:
            print(f"[asr-host] 创建语音缓存目录失败 {exc}", flush=True)
        self.role_key = "staff"
        self.last_active = 0.0
        # 记已问候过的时段（日期+时段），同一时段每天只问候一次
        self.last_greeting_key = ""
        print(
            f"[asr-host] 语音素材 应答{len(self.ack_files)} 称呼{len(self.role_files)} 告别{len(self.bye_files)}",
            flush=True,
        )

    def _play_clips(self, clips) -> None:
        for i, p in enumerate(clips):
            try:
                if str(p).lower().endswith(".mp3"):
                    self._play_mp3(Path(p))
                else:
                    with wave.open(str(p), "rb") as w:
                        rate = w.getframerate()
                        channels = w.getnchannels()
                        raw = w.readframes(w.getnframes())
                    data = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
                    if channels > 1:
                        data = data.reshape(-1, channels).mean(axis=1)
                    sd.play(data, rate)
                    sd.wait()
            except Exception as exc:
                print(f"[asr-host] 播放失败 {p.name}: {exc}", flush=True)
            if i < len(clips) - 1:
                time.sleep(0.12)

    @staticmethod
    def _play_mp3(path: Path) -> None:
        """播放服务端合成的话术缓存（mp3）：Windows 用 MCI，macOS 用系统 afplay。"""
        if sys.platform == "darwin":
            import subprocess

            subprocess.run(["afplay", str(path)], check=False)
            return
        import ctypes

        mci = ctypes.windll.winmm.mciSendStringW
        buf = ctypes.create_unicode_buffer(512)
        alias = "asheng%d" % int(time.time() * 1000)
        if mci(f'open "{path}" type mpegvideo alias {alias}', buf, 512, None) != 0:
            raise RuntimeError("MCI 打开失败")
        try:
            mci(f"play {alias} wait", buf, 512, None)
        finally:
            mci(f"close {alias}", buf, 512, None)

    # ---------- 云端规则 ----------

    def _api_base(self) -> str:
        base = self.endpoint.strip().rstrip("/")
        return base[:-4] if base.endswith("/asr") else base

    def _http_get(self, url: str, timeout: float = 10.0) -> bytes:
        req = urllib.request.Request(url, headers={"Authorization": "Bearer " + self.token})
        with urllib.request.urlopen(req, timeout=timeout, context=ssl_context()) as resp:
            return resp.read()

    def _rules_loop(self) -> None:
        while self.running:
            if self.token:
                self.safe_refresh()
            time.sleep(RULES_REFRESH_SECONDS)

    def safe_refresh(self) -> None:
        try:
            self.refresh_rules()
        except Exception as exc:
            self.rules_error = str(exc)
            print(f"[asr-host] 规则刷新失败 {exc}", flush=True)

    def refresh_rules(self) -> str:
        """拉一次服务端规则；失败保留旧规则，绝不因为后台不可用影响说话。"""
        raw = self._http_get(self._api_base() + "/voice/config")
        data = json.loads(raw.decode("utf-8"))
        merged = copy.deepcopy(DEFAULT_RULES)

        def pick_list(key: str) -> None:
            if isinstance(data.get(key), list) and data[key]:
                merged[key] = [str(x).strip() for x in data[key] if str(x).strip()]

        pick_list("ack")
        pick_list("bye")
        if isinstance(data.get("roles"), dict) and data["roles"]:
            merged["roles"] = {str(k): str(v) for k, v in data["roles"].items()}
        if isinstance(data.get("greetings"), list) and data["greetings"]:
            items = []
            for item in data["greetings"]:
                if not isinstance(item, dict):
                    continue
                try:
                    start, end = int(item.get("start")), int(item.get("end"))
                except (TypeError, ValueError):
                    continue
                texts = [str(t).strip() for t in item.get("texts") or [] if str(t).strip()]
                if texts:
                    items.append({"start": start, "end": end, "texts": texts})
            if items:
                merged["greetings"] = items
        for section in ("exit", "interrupt", "conversation", "tts"):
            if isinstance(data.get(section), dict):
                merged[section].update({k: v for k, v in data[section].items() if v is not None})
        for section, keys in (
            ("exit", ("max_len", "short_len")),
            ("conversation", ("idle", "misses", "role_idle")),
            ("interrupt", ("rms",)),
        ):
            for key in keys:
                try:
                    merged[section][key] = float(merged[section][key])
                except (TypeError, ValueError):
                    merged[section][key] = DEFAULT_RULES[section][key]

        merged["version"] = str(data.get("version") or "")
        changed = merged["version"] != self.rules.get("version")
        self.rules = merged
        self.rules_at = time.time()
        self.rules_error = ""
        if changed:
            print(
                f"[asr-host] 规则已更新 v{merged['version']} "
                f"应答{len(merged['ack'])} 告别{len(merged['bye'])} "
                f"退出词{len(merged['exit']['words'])}",
                flush=True,
            )
            threading.Thread(target=self.prewarm_voice, daemon=True).start()
        return merged["version"]

    def clip_path(self, text: str) -> Path:
        tts = self.rules.get("tts") or DEFAULT_RULES["tts"]
        key = f"{tts.get('voice', '')}|{tts.get('rate', '')}|{text}"
        return self.tts_dir / (hashlib.sha1(key.encode("utf-8")).hexdigest()[:20] + ".mp3")

    def fetch_clip(self, text: str) -> Path | None:
        """按服务端配置的音色现场合成一句，缓存到本地，下回直接用。"""
        if not self.token or not text:
            return None
        path = self.clip_path(text)
        try:
            tts = self.rules.get("tts") or {}
            query = urlencode({
                "text": text,
                "voice": tts.get("voice", ""),
                "rate": tts.get("rate", ""),
            })
            data = self._http_get(self._api_base() + "/tts?" + query, timeout=15)
            if not data:
                return None
            tmp = path.with_suffix(".part")
            tmp.write_bytes(data)
            tmp.replace(path)
            return path
        except Exception as exc:
            print(f"[asr-host] 合成语音失败 {text!r}: {exc}", flush=True)
            return None

    def prewarm_voice(self) -> None:
        """后台把下发的问候/应答/告别/称呼都合成好，唤醒时直接播本地缓存。"""
        rules = self.rules
        texts = list(rules["ack"]) + list(rules["bye"]) + list(rules["roles"].values())
        for item in rules.get("greetings") or []:
            texts.extend(item.get("texts") or [])
        for text in texts:
            if not self.running:
                return
            if not text or self.clip_path(text).exists():
                continue
            if not self.fetch_clip(text):
                return
            time.sleep(0.15)
        print("[asr-host] 语音缓存预热完成", flush=True)

    def _current_greeting(self) -> tuple[list[str], str]:
        """按本机当前时间找时段问候，返回 (候选话术, 当天该时段的唯一键)。"""
        hour = time.localtime().tm_hour
        for item in self.rules.get("greetings") or []:
            try:
                start, end = int(item["start"]), int(item["end"])
            except (KeyError, TypeError, ValueError):
                continue
            hit = (start <= hour < end) if start < end else (hour >= start or hour < end)
            if not hit:
                continue
            texts = [t for t in item.get("texts") or [] if t]
            if texts:
                return texts, f"{time.strftime('%Y-%m-%d')}-{start}-{end}"
        return [], ""

    def play_ack(self) -> None:
        """唤醒应答：时段问候 + 隔久了带称呼 + 随机应答词。"""
        self.ack_started_at = time.perf_counter()
        rules = self.rules
        clips = []
        greeting, greet_key = self._current_greeting()
        if greeting and greet_key != self.last_greeting_key:
            clip = self.clip_path(random.choice(greeting))
            if clip.exists():
                clips.append(clip)
                self.last_greeting_key = greet_key
        if time.time() - self.last_active > rules["conversation"]["role_idle"]:
            title = rules["roles"].get(self.role_key)
            role = self.clip_path(title) if title else None
            if role is not None and role.exists():
                clips.append(role)
            elif self.role_files.get(self.role_key) is not None:
                clips.append(self.role_files[self.role_key])
        phrase = random.choice(rules["ack"]) if rules["ack"] else ""
        ack = self.clip_path(phrase) if phrase else None
        if ack is not None and ack.exists():
            clips.append(ack)
        elif self.ack_files:
            clips.append(random.choice(self.ack_files))
        print(f"[asr-host] 应答 {[Path(p).name for p in clips]}", flush=True)
        self._play_clips(clips)
        self.last_active = time.time()
        while True:
            try:
                self.queue.get_nowait()
            except queue.Empty:
                break

    def play_bye(self) -> None:
        phrase = random.choice(self.rules["bye"]) if self.rules["bye"] else ""
        pick = self.clip_path(phrase) if phrase else None
        if pick is None or not pick.exists():
            pick = random.choice(self.bye_files) if self.bye_files else None
        if pick is not None:
            print(f"[asr-host] 告别 {Path(pick).name}", flush=True)
            self._play_clips([pick])
        self.last_active = time.time()

    def _new_wake_vad(self) -> sherpa_onnx.VoiceActivityDetector:
        cfg = sherpa_onnx.VadModelConfig()
        cfg.silero_vad.model = str(self.models / "vad" / "silero_vad.onnx")
        cfg.silero_vad.threshold = 0.5
        # 唤醒只关心一句话，断句快一点
        cfg.silero_vad.min_silence_duration = 0.55
        cfg.silero_vad.min_speech_duration = 0.35
        cfg.silero_vad.max_speech_duration = 8
        cfg.sample_rate = SAMPLE_RATE
        return sherpa_onnx.VoiceActivityDetector(cfg, buffer_size_in_seconds=12)

    def _new_interrupt_vad(self) -> sherpa_onnx.VoiceActivityDetector:
        """播报期间用的断句器：窗口短，用户一插话很快就能判出来。"""
        cfg = sherpa_onnx.VadModelConfig()
        cfg.silero_vad.model = str(self.models / "vad" / "silero_vad.onnx")
        cfg.silero_vad.threshold = 0.5
        cfg.silero_vad.min_silence_duration = 0.45
        cfg.silero_vad.min_speech_duration = 0.35
        cfg.silero_vad.max_speech_duration = 2.5
        cfg.sample_rate = SAMPLE_RATE
        return sherpa_onnx.VoiceActivityDetector(cfg, buffer_size_in_seconds=6)

    def _new_vad(self) -> sherpa_onnx.VoiceActivityDetector:
        cfg = sherpa_onnx.VadModelConfig()
        cfg.silero_vad.model = str(self.models / "vad" / "silero_vad.onnx")
        cfg.silero_vad.threshold = 0.5
        # 说话中间的自然停顿不要断句
        cfg.silero_vad.min_silence_duration = 0.8
        cfg.silero_vad.min_speech_duration = 0.25
        cfg.silero_vad.max_speech_duration = 20
        cfg.sample_rate = SAMPLE_RATE
        return sherpa_onnx.VoiceActivityDetector(cfg, buffer_size_in_seconds=30)

    def _new_kws(self) -> sherpa_onnx.KeywordSpotter:
        kws_dir = self.models / "kws"
        return sherpa_onnx.KeywordSpotter(
            tokens=str(kws_dir / "tokens.txt"),
            encoder=str(kws_dir / "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
            decoder=str(kws_dir / "decoder-epoch-12-avg-2-chunk-16-left-64.onnx"),
            joiner=str(kws_dir / "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx"),
            keywords_file=str(kws_dir / "keywords.txt"),
            num_threads=1,
            provider="cpu",
            # 默认 0.25 对远处的说话偏严，放宽到 0.15；配合下面的输入增益
            keywords_score=1.0,
            keywords_threshold=0.15,
        )

    # ---------- 事件 ----------
    def emit(self, kind: str, text: str = "", **extra) -> None:
        with self.lock:
            self.seq += 1
            self.events.append({"id": self.seq, "type": kind, "text": text, **extra})
            if len(self.events) > 200:
                del self.events[:-100]

    def events_since(self, since: int) -> tuple[list[dict], int]:
        with self.lock:
            return [e for e in self.events if e["id"] > since], self.seq

    # ---------- 埋点 ----------
    def track(self, event: str, **fields) -> None:
        try:
            fields.setdefault("session", self.session_id)
            self.metrics.add(event, **fields)
        except Exception:
            pass

    def _metrics_loop(self) -> None:
        while self.running:
            time.sleep(METRIC_FLUSH_SECONDS)
            if not self.token:
                continue
            try:
                self.metrics.flush(self._api_base() + "/voice/metrics", self.token)
            except Exception as exc:
                print(f"[asr-host] 埋点上报失败 {exc}", flush=True)

    def _open_session(self) -> None:
        if self.session_id:
            return
        self.session_id = uuid.uuid4().hex[:12]
        self.session_started = time.time()
        self.session_turns = 0
        self.session_empty = 0
        self.last_interrupt_at = 0.0
        self.text_since_interrupt = False

    def _close_session(self, reason: str) -> None:
        if not self.session_id:
            return
        duration = time.time() - self.session_started if self.session_started else 0.0
        # 打断后没等到有效提问，多半是把环境音当成了插话
        if self.last_interrupt_at and not self.text_since_interrupt:
            self.track("interrupt_miss", layer="打断后无有效提问")
        self.track(
            "conv_end",
            reason=reason,
            turns=self.session_turns,
            detail={"dur_s": round(duration, 1), "empty": self.session_empty},
        )
        self.session_id = ""
        self.session_started = 0.0
        self.session_turns = 0
        self.session_empty = 0
        self.last_interrupt_at = 0.0
        self.text_since_interrupt = False

    # ---------- 音频 ----------
    def ensure_stream(self) -> None:
        if self.stream is not None:
            return
        self.stream = sd.InputStream(
            samplerate=SAMPLE_RATE,
            blocksize=BLOCK,
            channels=1,
            dtype="float32",
            callback=self._on_audio,
        )
        self.stream.start()
        print("[asr-host] 麦克风已打开", flush=True)

    def close_stream(self) -> None:
        if self.stream is None:
            return
        try:
            self.stream.stop()
            self.stream.close()
        except Exception:
            pass
        self.stream = None
        print("[asr-host] 麦克风已关闭", flush=True)

    def _on_audio(self, indata, frames, time_info, status) -> None:
        if status:
            self.last_error = str(status)
        chunk = indata.reshape(-1).copy()
        # 笔记本内置麦克风离人通常有半米以上，电平偏低；这里抬一档再交给模型
        peak = float(np.abs(chunk).max())
        if 0 < peak < 0.4:
            chunk = np.clip(chunk * 2.0, -1.0, 1.0)
        self.queue.put(chunk)

    def audio_loop(self) -> None:
        while self.running:
            try:
                chunk = self.queue.get(timeout=0.2)
            except queue.Empty:
                continue
            with self.lock:
                wake = self.wake_on
                capturing = self.capturing
                ptt = self.ptt_active
                muted = self.muted
            if muted and not ptt:
                self._interrupt_feed(chunk)
                continue
            if ptt:
                self.ptt_buffer.append(chunk)
                continue
            if capturing:
                self.capture_buffer.append(chunk)
                self._capture_feed(chunk)
            elif wake:
                self._wake_feed(chunk)

    def _wake_feed(self, chunk: np.ndarray) -> None:
        # 一段话说完（静音 0.8s）交给服务端识别，句子里提到「阿胜」就算唤醒
        vad = self.wake_vad
        if vad is None:
            return
        vad.accept_waveform(chunk)
        while not vad.empty():
            seg = np.array(vad.front.samples, dtype=np.float32)
            vad.pop()
            if self._too_quiet(seg):
                continue
            try:
                text = self.transcribe(seg, source="wake")
            except Exception as exc:
                print(f"[asr-host] 唤醒识别失败 {exc}", flush=True)
                self.track("wake_miss", ok=False, reason="error")
                continue
            if "阿胜" in text:
                self._on_wake_text(text)
                return
            self.track(
                "wake_miss", ok=False,
                reason="no_keyword" if text else "empty",
                ms=self.last_asr_ms, text_len=len(text),
            )

    def _interrupt_feed(self, chunk: np.ndarray) -> None:
        """播报期间继续听：回放的声音也会被识别，但只有出现打断词才真正动作。"""
        vad = self.interrupt_vad
        if vad is None:
            return
        vad.accept_waveform(chunk)
        while not vad.empty():
            seg = np.array(vad.front.samples, dtype=np.float32)
            vad.pop()
            if self._too_quiet(seg):
                continue
            text = self.transcribe(seg, source="interrupt")
            if not text:
                continue
            self.emit("heard", text, source="interrupt")
            # 明确说「退出」：停止播报，直接收工
            if is_exit_intent(text, self.rules):
                print(f"[asr-host] 用户结束对话：{text}", flush=True)
                with self.lock:
                    self.muted = False
                    self.interrupt_vad = None
                self.track("exit", ok=True, layer=exit_decision(text, self.rules)[1],
                           text_len=len(text), detail={"during": "playing"})
                self.emit("bye", text)
                self.play_bye()
                self.end_conversation("bye")
                return
            # 播报里本身可能说到「阿胜」，所以只认三种情况：
            # 1) 明确的打断词；2) 句首就是「阿胜」；3) 用户近场说话能量明显更高
            rms = float(np.sqrt(np.mean(np.square(seg))))
            self.last_rms = rms
            head = text.strip()[:4]
            strong = rms >= self.rules["interrupt"]["rms"]
            head_hit = head.startswith("阿胜")
            stop_hit = any(w in text for w in self.rules["interrupt"]["stop"])
            if strong or head_hit or stop_hit:
                print(f"[asr-host] 打断播报 rms={rms:.3f} 文本={text}", flush=True)
                layer = "stop" if stop_hit else ("head" if head_hit else "rms")
                self.track(
                    "interrupt", ok=True, layer=layer,
                    ms=int((time.time() - self.muted_at) * 1000) if self.muted_at else None,
                    detail={"rms": round(rms, 3)},
                )
                self.last_interrupt_at = time.time()
                self.text_since_interrupt = False
                with self.lock:
                    self.muted = False
                    self.interrupt_vad = None
                self.emit("interrupt", text)
                # 只丢掉打断瞬间的缓冲，用户紧接着说的话要保留下来
                while not self.queue.empty():
                    try:
                        self.queue.get_nowait()
                    except queue.Empty:
                        break
                self.begin_capture()
                return

    def _on_wake_text(self, text: str) -> None:
        """唤醒词后面还带了问题就一并当指令，省得用户说两遍。"""
        cmd = text
        for token in ("阿胜阿胜", "阿胜"):
            cmd = cmd.replace(token, "")
        cmd = cmd.strip(" ，。,.!?！？、")
        print(f"[asr-host] 唤醒识别 {text} -> 指令 {cmd!r}", flush=True)
        # 同一轮对话里重新唤醒，先把上一段会话收尾再开新会话
        self._close_session("rewake")
        self._open_session()
        self.track("wake", ok=True, ms=self.last_asr_ms, text_len=len(cmd),
                   detail={"cmd": bool(len(cmd) >= 2)})
        self.emit("wake", "阿胜阿胜")
        wake_at = time.perf_counter()
        self.play_ack()
        ack_at = self.ack_started_at or wake_at
        self.track("ack", ok=True, ms=int((ack_at - wake_at) * 1000),
                   detail={"play_ms": int((time.perf_counter() - ack_at) * 1000)})
        if len(cmd) >= 2:
            self.session_turns += 1
            self.text_since_interrupt = True
            self.emit("text", cmd, source="wake")
        # 不管这句里有没有带问题，都进入收音，方便接着说
        self.begin_capture()

    def begin_capture(self) -> None:
        with self.lock:
            self.capturing = True
            self.capture_buffer = []
            self.vad = self._new_vad()
            self.wake_vad = self._new_wake_vad()
        self.capture_started = time.time()
        self.conv_misses = 0
        self.emit("listening")

    def _capture_feed(self, chunk: np.ndarray) -> None:
        with self.lock:
            vad = self.vad
        vad.accept_waveform(chunk)
        if not vad.empty():
            self.finish_capture()
        elif time.time() - self.capture_started > self.rules["conversation"]["idle"]:
            # 一段时间没人说话，结束这轮对话，回到待机
            self.end_conversation("idle")

    @staticmethod
    def _too_quiet(samples: np.ndarray) -> bool:
        """太短或能量太低的一律当噪音，避免模型对着空调声编句子。"""
        if len(samples) < SAMPLE_RATE * 0.35:
            return True
        rms = float(np.sqrt(np.mean(np.square(samples.astype(np.float32)))))
        return rms < 0.012

    def finish_capture(self) -> None:
        with self.lock:
            if not self.capturing:
                return
            self.capturing = False
            vad = self.vad
            buffered = self.capture_buffer
            self.capture_buffer = []
        text = ""
        try:
            vad.flush()
            samples = []
            while not vad.empty():
                samples.extend(vad.front.samples)
                vad.pop()
            if not samples and buffered:
                samples = np.concatenate(buffered).tolist()
            if samples:
                arr = np.array(samples, dtype=np.float32)
                if not self._too_quiet(arr):
                    text = self.transcribe(arr, source="capture")
        except Exception as exc:  # 识别失败要让桌面端知道，不能静默
            self.emit("error", str(exc))

        clean = text.strip(" ，。,.!?！？、")
        if len(clean) >= 2:
            # 用户明确要结束，就别再回答，直接回待机
            if is_exit_intent(text, self.rules):
                print(f"[asr-host] 用户结束对话：{text}", flush=True)
                self.track("exit", ok=True, layer=exit_decision(text, self.rules)[1],
                           text_len=len(text), detail={"during": "listening"})
                self.emit("bye", text)
                self.play_bye()
                self.end_conversation("bye")
                return
            # 对话中又喊了唤醒词：重新应答，不把它当成问题发出去
            if "阿胜" in text:
                self._on_wake_text(text)
                return
            self.session_turns += 1
            self.text_since_interrupt = True
            self.emit("text", text, source="wake")
            self.conv_misses = 0
            self.resume_capture()
            return

        # 有声音但没识别出内容，连续两次就认为用户走开了
        self.conv_misses += 1
        self.session_empty += 1
        self.emit("empty")
        if self.conv_misses >= self.rules["conversation"]["misses"]:
            self.end_conversation("misses")
        else:
            self.resume_capture()

    def resume_capture(self) -> None:
        """一句话结束后继续收音，用户可以接着追问，不用重新唤醒。"""
        with self.lock:
            self.capturing = True
            self.capture_buffer = []
            self.vad = self._new_vad()
            self.wake_vad = self._new_wake_vad()
        self.capture_started = time.time()

    def end_conversation(self, reason: str = "manual") -> None:
        self._close_session(reason)
        with self.lock:
            self.capturing = False
            self.wake_vad = self._new_wake_vad()
        self.conv_misses = 0
        print("[asr-host] 对话结束，回到待机", flush=True)
        self.emit("sleep")

    def trim_speech(self, samples: np.ndarray) -> np.ndarray | None:
        """用 VAD 去掉首尾静音；整段没人说话就返回 None，避免拿噪音硬识别。"""
        if len(samples) < SAMPLE_RATE * 0.3:
            return None
        vad = self._new_vad()
        vad.accept_waveform(samples)
        vad.flush()
        parts = []
        while not vad.empty():
            parts.append(np.array(vad.front.samples, dtype=np.float32))
            vad.pop()
        if not parts:
            return None
        return np.concatenate(parts)

    def transcribe(self, samples: np.ndarray, source: str = "unknown") -> str:
        """上传到服务端中转识别，用哪家由管理后台切换。"""
        if not self.token:
            print("[asr-host] 还没有登录令牌，跳过识别", flush=True)
            return ""
        body = json.dumps({"audio": base64.b64encode(to_wav(samples)).decode()})
        req = urllib.request.Request(
            self.endpoint,
            data=body.encode("utf-8"),
            method="POST",
            headers={
                "Content-Type": "application/json",
                "Authorization": "Bearer " + self.token,
            },
        )
        audio_s = round(len(samples) / SAMPLE_RATE, 2)
        t0 = time.perf_counter()
        try:
            with urllib.request.urlopen(req, timeout=25, context=ssl_context()) as resp:
                data = json.loads(resp.read().decode("utf-8"))
        except Exception as exc:
            self.last_asr_error = f"{type(exc).__name__}: {exc}"
            print(f"[asr-host] 识别请求失败 {exc}", flush=True)
            self.online = False
            self.last_asr_ms = int((time.perf_counter() - t0) * 1000)
            self.track("asr_error", reason=type(exc).__name__, ms=self.last_asr_ms, layer=source)
            return ""
        self.online = True
        self.last_asr_error = ""
        self.last_asr_ms = int((time.perf_counter() - t0) * 1000)
        text = normalize(str(data.get("text") or "").strip())
        self.last_heard = text
        self.track("asr", ok=bool(text), ms=self.last_asr_ms, text_len=len(text),
                   detail={"source": source, "audio_s": audio_s})
        print(
            f"[asr-host] {len(samples) / SAMPLE_RATE:.2f}s -> {text} "
            f"({(time.perf_counter() - t0) * 1000:.0f}ms)",
            flush=True,
        )
        return text

    # ---------- 对外动作 ----------
    def set_wake(self, on: bool) -> dict:
        if on:
            if self.wake_vad is None:
                self.wake_vad = self._new_wake_vad()
            self.ensure_stream()
            self.wake_on = True
            self.emit("wake_on")
        else:
            self._close_session("wake_off")
            self.wake_on = False
            with self.lock:
                self.capturing = False
                self.wake_vad = None
            self.emit("wake_off")
            if not self.ptt_active:
                self.close_stream()
        return {"wake": self.wake_on}

    def ptt(self, on: bool) -> dict:
        if on:
            self.ensure_stream()
            with self.lock:
                self.ptt_active = True
                self.ptt_buffer = []
            self.emit("ptt_start")
            return {"ok": True}
        with self.lock:
            had = self.ptt_active
            self.ptt_active = False
            buffered = self.ptt_buffer
            self.ptt_buffer = []
        text = ""
        if had and buffered:
            samples = np.concatenate(buffered)
            if len(samples) > SAMPLE_RATE * PTT_MAX_SECONDS:
                samples = samples[-int(SAMPLE_RATE * PTT_MAX_SECONDS):]
            speech = self.trim_speech(samples)
            if speech is not None:
                text = self.transcribe(speech, source="ptt")
        if not self.wake_on:
            self.close_stream()
        return {"text": text}

    def shutdown(self) -> None:
        self.running = False
        self.close_stream()
        if self.on_shutdown is not None:
            self.on_shutdown()


class Handler(BaseHTTPRequestHandler):
    engine: AsrEngine = None  # type: ignore[assignment]

    def log_message(self, fmt, *args) -> None:  # 静音默认访问日志
        pass

    def _send(self, code: int, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        # 页面（https）需要能调本地 127.0.0.1，这里放开跨域
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(body)

    def do_OPTIONS(self) -> None:
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.end_headers()

    def do_GET(self) -> None:
        url = urlparse(self.path)
        if url.path == "/health":
            e = self.engine
            self._send(200, {
                "ok": True,
                "version": VERSION,
                "wake": e.wake_on,
                "ptt": e.ptt_active,
                "mic": e.stream is not None,
                "auth": bool(e.token),
                "online": e.online,
                "muted": e.muted,
                "heard": e.last_heard,
                "rms": round(e.last_rms, 3),
                "error": e.last_error,
                "asr_error": e.last_asr_error,
                "rules": {
                    "version": e.rules.get("version") or "default",
                    "at": round(e.rules_at, 0),
                    "error": e.rules_error,
                    "ack": len(e.rules["ack"]),
                    "bye": len(e.rules["bye"]),
                    "greetings": len(e.rules.get("greetings") or []),
                    "greeted": e.last_greeting_key,
                },
                "metrics": {
                    "client": e.metrics.client_id,
                    "app": APP_VERSION or "-",
                    "sent": e.metrics.sent,
                    "failed": e.metrics.failed,
                    "pending": e.metrics.pending(),
                    "error": e.metrics.last_error,
                },
            })
            return
        if url.path == "/test/exit":
            text = parse_qs(url.query).get("text", [""])[0]
            self._send(200, {"text": text, "exit": is_exit_intent(text, self.engine.rules)})
            return
        if url.path == "/events":
            since = int(parse_qs(url.query).get("since", ["0"])[0] or 0)
            items, next_id = self.engine.events_since(since)
            self._send(200, {"events": items, "next": next_id})
            return
        self._send(404, {"ok": False, "error": "not found"})

    def do_POST(self) -> None:
        try:
            self._post()
        except Exception as exc:
            self._send(500, {"ok": False, "error": f"{type(exc).__name__}: {exc}"})

    def _post(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw.decode("utf-8") or "{}")
        except Exception:
            body = {}
        url = urlparse(self.path)
        if url.path == "/wake":
            self._send(200, self.engine.set_wake(bool(body.get("on"))))
            return
        if url.path == "/ptt":
            self._send(200, self.engine.ptt(bool(body.get("on"))))
            return
        if url.path == "/mute":
            on = bool(body.get("on"))
            with self.engine.lock:
                self.engine.muted = on
                # 播报期间另起一个断句器，专门盯打断词
                self.engine.interrupt_vad = self.engine._new_interrupt_vad() if on else None
            self.engine.muted_at = time.time() if on else 0.0
            self._send(200, {"muted": on})
            return
        if url.path == "/config":
            token = str(body.get("token") or "").strip()
            endpoint = str(body.get("endpoint") or "").strip()
            if token:
                self.engine.token = token
            if endpoint:
                self.engine.endpoint = endpoint
            role = str(body.get("role") or "").strip()
            if role:
                # 角色决定唤醒时用哪个称呼；不认识的一律当普通同事
                roles = self.engine.rules["roles"]
                self.engine.role_key = role if role in roles else ("staff" if "staff" in roles else next(iter(roles)))
            if self.engine.token:
                # 刚拿到令牌就立刻拉一次规则，不用等下一轮轮询
                threading.Thread(target=self.engine.safe_refresh, daemon=True).start()
            self._send(200, {"ok": True, "hasToken": bool(self.engine.token)})
            return
        if url.path == "/rules/refresh":
            threading.Thread(target=self.engine.safe_refresh, daemon=True).start()
            self._send(200, {"ok": True})
            return
        if url.path == "/shutdown":
            self._send(200, {"ok": True})
            threading.Thread(target=self.engine.shutdown, daemon=True).start()
            return
        if url.path == "/test/wav":
            path = str(body.get("path") or "")
            try:
                import wave

                with wave.open(path, "rb") as f:
                    data = np.frombuffer(f.readframes(f.getnframes()), dtype=np.int16)
                text = self.engine.transcribe(data.astype(np.float32) / 32768.0)
                self._send(200, {"text": text})
            except Exception as exc:
                self._send(500, {"ok": False, "error": str(exc)})
            return
        self._send(404, {"ok": False, "error": "not found"})


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--models", default=os.environ.get("ASHENG_ASR_MODELS", ""))
    parser.add_argument("--port", type=int, default=PORT)
    parser.add_argument("--threads", type=int, default=2)
    args = parser.parse_args()

    if not args.models:
        base = Path(sys.executable).resolve().parent if getattr(sys, "frozen", False) else Path(__file__).resolve().parent
        args.models = str(base / "models")
    models = Path(args.models)
    if not (models / "vad" / "silero_vad.onnx").exists():
        print(f"[asr-host] 找不到模型目录：{models}", flush=True)
        sys.exit(2)

    engine = AsrEngine(models, args.threads)
    Handler.engine = engine
    threading.Thread(target=engine.audio_loop, daemon=True).start()

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    engine.on_shutdown = server.shutdown
    print(f"[asr-host] 服务已启动 http://127.0.0.1:{args.port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        engine.shutdown()


if __name__ == "__main__":
    main()
