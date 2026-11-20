#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""AgentFind —— 本机多 Agent 对话全局检索。

线索进（关键词 / 产物文件名 / 项目名 / 日期），出「什么时候 · 哪个产品 · 哪个项目 · 哪个对话」。

用法:
    agentfind                     刷新索引，起本地网页并打开浏览器
    agentfind 锂电池 RUL           同上，搜索框预填关键词
    agentfind --status            打印各数据源会话数与索引状态
    agentfind --rebuild           全量重建索引
    agentfind --cli 关键词         终端里直接搜，不起服务
    agentfind --port 9000 --no-open

只读扫描各 Agent 的本地记录，不修改任何原始文件。索引落在 ~/.agentfind/index.sqlite。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sqlite3
import subprocess
import sys
import threading
import time
import webbrowser
from dataclasses import dataclass, field
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, quote, unquote, urlparse
from urllib.request import build_opener, ProxyHandler

# 自己调自己的 127.0.0.1 接口必须绕过系统代理：Windows 上 urllib 不认 ProxyOverride 里的
# "127.*"/"<local>"，开着 Clash 这类本地代理时请求会被丢给代理，直接挂住或误判"服务已在运行"。
LOCAL_OPENER = build_opener(ProxyHandler({}))


def local_open(url, timeout=60):
    return LOCAL_OPENER.open(url, timeout=timeout)

HOME = Path.home()
APP_DIR = Path(os.environ.get("AGENTFIND_HOME") or (HOME / ".agentfind"))
DB_PATH = APP_DIR / "index.sqlite"
LOG_PATH = APP_DIR / "agentfind.log"
PORT_FILE = APP_DIR / "running.port"
# 8765 已经让出去了：qwenwork-bridge（驾驶舱的千问办公反代）默认端口就是它，
# 两边都写死 8765 时谁先起谁赢，输的那个还察觉不到。
DEFAULT_PORT = int(os.environ.get("AGENTFIND_PORT", 8790))
PORT_SPAN = 10            # 首选口被影子绑定时往后试几个
VERIFY_TRIES = 3          # 自证探活：每个候选端口探几次
VERIFY_TIMEOUT = 6        # 秒，单次探活的超时（首次 /api/meta 冷启动实测 2.9s）

MAX_MSG_CHARS = 20000   # 单条消息入库上限
TRIGRAM_MIN = 3         # fts5 trigram 分词器要求的最短子串长度
IDX_LOCK = threading.RLock()


class _NoLock:
    """读路径用的空锁。SQLite 开了 WAL，"一写多读"本来就允许并发，
    让查询排在首轮约 20s 的扫描后面，是"每次刚打开都卡半天"的真正原因。
    写路径（refresh / rebuild）仍然用 IDX_LOCK 独占。"""

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


NO_LOCK = _NoLock()


def _script_fingerprint():
    """FNV-1a over this file's bytes. The desktop launcher compares it before
    reusing a live service, so a leftover older build cannot keep serving a
    stale UI. Must stay byte-identical to script_fingerprint() in agentfind.rs."""
    h = 0xcbf29ce484222325
    for b in Path(__file__).read_bytes():
        h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return f"{h:016x}"


SCRIPT_FINGERPRINT = _script_fingerprint()


# --------------------------------------------------------------------------
# 运行环境
# --------------------------------------------------------------------------

def attach_log():
    """pythonw / 快捷方式启动时没有控制台，sys.stdout 是 None，print 会直接崩。"""
    if getattr(sys, "frozen", False) or sys.stdout is None or sys.stderr is None:
        try:
            APP_DIR.mkdir(parents=True, exist_ok=True)
            stream = open(LOG_PATH, "a", encoding="utf-8", buffering=1)
        except OSError:
            stream = open(os.devnull, "w", encoding="utf-8")
        sys.stdout = sys.stderr = stream


def _meta_at(port, timeout=3.0):
    try:
        with local_open(f"http://127.0.0.1:{port}/api/meta", timeout=timeout) as r:
            meta = json.loads(r.read().decode("utf-8"))
        return meta if isinstance(meta, dict) else None
    except Exception:
        return None


def already_running(port):
    """本机端口上已有 AgentFind 在跑就返回 True（避免重复启动去抢同一个端口）。"""
    for _ in range(2):
        meta = _meta_at(port)
        if meta and "total" in meta:
            return True
    return False


def is_me_running(port):
    """端口上应答的确实是**本进程**。

    Windows 上 allow_reuse_address 允许两个进程绑同一个端口，先绑的那个才收得到请求，
    所以 bind() 成功不等于抢到端口。2026-10-29 实录：qwenwork-bridge 先占了 8765，
    agentfind 影子绑在同一个口上，进程活着、端口"在听"、一个请求都收不到，
    桌面启动器探到的是桥的 404，于是弹"等了 12 秒服务端口还是没响应"。

    要重试而不是单发一次：/api/meta 首次得现算一遍 overview()，实测冷启动 2.9s，
    探得太急会把本来就归我们的好端口误判成"没抢到"，白往后漂。
    """
    for attempt in range(VERIFY_TRIES):
        meta = _meta_at(port, timeout=VERIFY_TIMEOUT)
        if meta and meta.get("pid") == os.getpid():
            return True
        if attempt < VERIFY_TRIES - 1:
            time.sleep(0.3)
    return False


def read_port_file():
    try:
        p = int(PORT_FILE.read_text(encoding="ascii").strip())
    except (OSError, ValueError):
        return None
    return p if 1024 <= p <= 65535 else None


def write_port_file(port):
    try:
        PORT_FILE.write_text(f"{port}\n", encoding="ascii")
    except OSError:
        pass


# --------------------------------------------------------------------------
# 时间 / 文本
# --------------------------------------------------------------------------

def to_epoch(v):
    """各种时间戳表示统一成 epoch 秒。"""
    if v is None or v == "":
        return None
    if isinstance(v, (int, float)):
        return v / 1000.0 if v > 1e11 else float(v)
    if isinstance(v, str):
        try:
            return datetime.fromisoformat(v.strip().replace("Z", "+00:00")).timestamp()
        except ValueError:
            return None
    return None


def day_of(ts):
    return datetime.fromtimestamp(ts).strftime("%Y-%m-%d") if ts else ""


def fmt_time(ts):
    if not ts:
        return "时间未知"
    return datetime.fromtimestamp(ts).strftime("%Y-%m-%d %H:%M")


BLOCK_TAG = re.compile(
    r"<(system-reminder|command-name|command-message|command-args|local-command-stdout|"
    r"local-command-stderr|user-prompt-submit-hook|task-notification|environment_context"
    r"|available-skills)(?:\s[^>]*)?>"
    r".*?</\1>", re.S)

PATH_RE = re.compile(
    r"[A-Za-z]:[\\/][^\s\"'`<>|]{2,200}"
    r"|/(?:Users|home|mnt|opt|var|srv|workspace|Claude|data|D)/[^\s\"'`<>|]{2,200}")

PATH_START = re.compile(r"^(?:[A-Za-z]:[\\/]|/(?:Users|home|mnt|opt|var|srv|workspace|Claude|data)/)")


KEEP_INNER = re.compile(r"</?(user_query|local-command-caveat|user_prompt)(?:\s[^>]*)?>")


def clean_text(s):
    if not s:
        return ""
    s = BLOCK_TAG.sub(" ", str(s))
    s = KEEP_INNER.sub("", s)
    s = s.replace("\r\n", "\n")
    s = re.sub(r"[ \t]+", " ", s)
    s = re.sub(r"\n{3,}", "\n\n", s)
    return s.strip()


def norm_path(p):
    """规整成可信路径，不像路径或太短就丢弃。"""
    if not isinstance(p, str):
        return None
    p = p.strip().strip("\"'<>(),;，。；、").replace("\\\\", "\\")
    p = re.sub(r"[\\/]$", "", p)
    if len(p) < 5 or "\n" in p or not PATH_START.match(p):
        return None
    return p


def collect_paths(obj, sink):
    """从工具调用参数里递归抓文件路径。"""
    if isinstance(obj, dict):
        for k, v in obj.items():
            if isinstance(v, str):
                if re.search(r"path|file|filename|directory|target|output", k, re.I):
                    np = norm_path(v)
                    if np:
                        sink.add(np)
            elif isinstance(v, (dict, list)):
                collect_paths(v, sink)
    elif isinstance(obj, list):
        for it in obj:
            if isinstance(it, str):
                np = norm_path(it)
                if np:
                    sink.add(np)
            else:
                collect_paths(it, sink)


def paths_in_text(text, sink):
    for m in PATH_RE.finditer(text or ""):
        np = norm_path(m.group(0))
        if np:
            sink.add(np)


NOISE_PREFIX = ("Caveat:", "This request must be fulfilled using",
                "Target file this round:",              # QwenWork CN 每轮注入的记忆上下文
                "The following is the Codex agent history",  # Codex 审批提示
                "User explicitly selected MCP server")

# 取标题时要跳过的包装行：Codex 转写边界（>>> TRANSCRIPT START）、分隔线、成对标签行。
TITLE_SKIP = re.compile(r"^\s*(>{3}|<\s*/?\w|[-=*_~]{3,}\s*$)")

# 2026-11-03：同一工作区的所有 Codex 会话都被取成 "AGENTS.md instructions for ..."。
# 病根：Codex 每场把工作区 AGENTS.md 包在第一条 user 消息里（<INSTRUCTIONS> 块），
# 老逻辑只跳"行"不跳"块"，块的正文行照样被当标题。改成块级跳过 + 标记行黑名单。
TITLE_BLOCK_TAGS = {
    "instructions", "user_instructions", "environment_context", "turn_context",
    "user_action", "user_actions", "system_warning", "permissions_input",
    "persisted-file", "persisted_file", "ide_opened_file", "ide_selection",
    "task_notification", "approval_request", "recommended_plugins",
}
TITLE_NOISE = (
    "agents.md instructions",                # Codex 注入的 AGENTS.md 抬头（有/无路径变体都盖住）
    "files mentioned by the user",           # 附件清单抬头（含 ## 变体，先剥 # 再比对）
    "my request:",                           # Codex 标记行本身（取标记后面的内容）
    "distinguish instructions in attached documents",
    "image attachment:",                     # 附件元数据行
    "the user opened the file",              # IDE 状态注入
)
# 老式转写把每轮标成 "[1] user:"，这种行也不配做标题
TITLE_ROLE_PREFIX = re.compile(r"^\[\d*\]\s*(user|assistant)\s*[:：]", re.I)
# 注意：这里必须全小写——比对处是 plain.lower().startswith(TITLE_NOISE)。
# 2026-11-03 首次上线时第一条写成 "AGENTS.md..."（大写），永远匹配不上，
# 130 场 Codex 会话的标题没换掉，就是它。
TITLE_PATHY = re.compile(
    r"^([A-Za-z]:[\\/][^\s]*|/{1,2}[\w.-]+(/\S*)*|\S*clipboard[-_]\S*)$", re.I)
_TITLE_TAG = re.compile(r"^<\s*(\w[\w.-]*)[^<>]*>$")


def _title_tag_close(tag):
    return re.compile(rf"^</\s*{re.escape(tag)}\s*>$", re.I)


def _title_from_user(text):
    """从一条 user 消息里挑第一行"真话"做标题候选。

    <INSTRUCTIONS> 这类成对标签块整段跳过；"## My request:" 后面的内容才是
    Codex 用户亲手打的话，优先取那里；文件路径行、附件元数据行都不配做标题。
    """
    body = text
    marker = "## My request:"
    i = body.find(marker)
    if i >= 0:
        body = body[i + len(marker):]
    in_block = None
    for line in body.split("\n"):
        s = line.strip()
        if in_block is not None:
            if _title_tag_close(in_block).match(s):
                in_block = None
            continue
        m = _TITLE_TAG.match(s)
        if m:
            tag = m.group(1).lower()
            if tag in TITLE_BLOCK_TAGS and not _title_tag_close(tag).match(s):
                in_block = tag
            continue
        plain = re.sub(r"^#+\s*", "", s)
        if not plain or TITLE_SKIP.match(plain):
            continue
        low = plain.lower()
        if low.startswith(TITLE_NOISE) or TITLE_PATHY.match(plain) \
                or TITLE_ROLE_PREFIX.match(plain):
            continue
        return plain
    return ""


def is_system_prompt(raw):
    """整段产品自带 system prompt，不是用户内容。"""
    return len(raw) > 4000 and raw.lstrip().startswith(("You are", "Your task",
                                                        "You will", "## Rules", "# Personality"))


# --------------------------------------------------------------------------
# 解析结果
# --------------------------------------------------------------------------

@dataclass
class Parsed:
    sid: str
    source: str
    label: str
    src_file: str
    project: str = ""
    title: str = ""
    started: float | None = None
    ended: float | None = None
    branch: str = ""
    models: set = field(default_factory=set)
    messages: list = field(default_factory=list)   # (ts, role, text)
    artifacts: set = field(default_factory=set)
    auto: int = 0      # 1 = 产品自己的自动任务（一场里连一条 user/summary 都没有）

    signal: tuple | None = None  # (timestamp, kind, summary, tool call id)

    def add(self, ts, role, text):
        text = clean_text(text)
        if not text:
            return
        if ts:
            if self.started is None or ts < self.started:
                self.started = ts
            if self.ended is None or ts > self.ended:
                self.ended = ts
        self.messages.append((ts, role, text[:MAX_MSG_CHARS]))

    def finish(self):
        if not self.title:
            for _ts, role, text in self.messages:
                if role != "user":
                    continue
                pick = _title_from_user(text)
                if pick:
                    self.title = pick[:160]
                    break
        if not any(role in ("user", "summary") for _ts, role, _text in self.messages):
            self.auto = 1
        return self


def read_lines(path: Path):
    try:
        with open(path, "r", encoding="utf-8", errors="ignore") as fh:
            for line in fh:
                if line.strip():
                    yield line
    except OSError:
        return


# --------------------------------------------------------------------------
# 数据源
# --------------------------------------------------------------------------

TRAE_SOURCE = "trae-cn"
TRAE_LABEL = "TraeWork CN"

NAMED_CLAUDE = [
    (".qoder-cn", "Qoder CN"),
    (".qoder", "Qoder 国际版"),
    (".claude", "Claude Code"),
    (".catpaw", "CatPaw"),
    (".meituan-catpaw", "美团 CatPaw"),
    (".openclaw", "OpenClaw"),
    (".workbuddy", "WorkBuddy"),
    (".qwenworkcn", "QwenWork CN"),
]

# 本机装了但记录格式还没解析的：页面上要如实告诉用户，免得以为没聊过
UNCOVERED = ["Cursor（正文存服务器端，本机只有空壳）", "Qoder IDE 侧边栏对话"]

# 已确认不是 Agent 对话记录的目录：Chromium 浏览器 profile / 控制台数据，没有可检索的
# 对话正文。列出来不是为了提示「没解析器」，而是让用户知道这些目录已经被看过并确认无对话。
KNOWN_NO_CHAT_DIRS = ["ClientSettings", "agent-browser", "agent-cockpit", "ichat"]
KNOWN_NO_CHAT = {"clientsettings", "agentbrowser", "agentcockpit", "ichat"}

# AgentCockpit src-tauri/src/registry.rs 里 curated 的全部 Agent（2026-10-31 对齐，共 32 个）。
# ① discover_sources() 拿它给有数据的 Agent 挂数据根目录；② overview() 拿它把
# 没装/没聊过的产品也列出来（显示 0 场）——用户一眼能分辨「装了但没记录」和「没装」。
AGENT_REGISTRY = [
    ("qoder", "Qoder"), ("qoder-cn", "Qoder CN"), ("qoder-cn-ide", "Qoder CN IDE"),
    ("workbuddy", "WorkBuddy"), ("workbuddy-intl", "WorkBuddy 国际版"),
    ("astudio", "AStudio"), ("minimax", "MiniMax Code"), ("traework-cn", "TRAE CN"),
    ("zcode", "ZCode"), ("qwenwork", "千问办公"), ("catpaw", "妙手"),
    ("dsh", "DeepSeek Harness"), ("marvis", "Marvis"), ("xiaomi-mimo", "Xiaomi MiMo"),
    ("opencode", "OpenCode"), ("codex", "Codex CLI"), ("codex-desktop", "ChatGPT"),
    ("claude", "Claude Code CLI"), ("claude-desktop", "Claude 桌面版"),
    ("antigravity", "Antigravity"), ("coze", "Coze"), ("cursor", "Cursor"),
    ("kiro", "Kiro"), ("windsurf", "Windsurf"), ("github-copilot", "GitHub Copilot"),
    ("zed", "Zed"), ("grok", "Grok Bot"), ("codebuddy", "CodeBuddy"),
    ("codebuddy-cn", "CodeBuddy CN"), ("trae", "Trae"), ("trae-solo", "TRAE SOLO"),
    ("trae-solo-cn", "TRAE SOLO CN"),
]

# registry id → AgentFind 的 source id。已有本地数据的沿用现成 source（含 DB 里的计数），
# 没有的用 registry id 新建。
REGISTRY_MAP = {
    "qoder": ".qoder", "qoder-cn": ".qoder-cn", "qoder-cn-ide": "qoder-cn-ide",
    "workbuddy": ".workbuddy", "workbuddy-intl": ".workbuddy-ai",
    "astudio": "astudio", "minimax": "minimax", "traework-cn": TRAE_SOURCE,
    "zcode": "zcode", "qwenwork": ".qwenworkcn", "catpaw": ".catpaw",
    "dsh": "dsh", "marvis": "marvis", "xiaomi-mimo": "xiaomi-mimo",
    "opencode": "opencode", "codex": "codex", "codex-desktop": "codex-desktop",
    "claude": ".claude", "claude-desktop": "claude-desktop",
    "antigravity": "antigravity", "coze": "coze", "cursor": "cursor",
    "kiro": "kiro", "windsurf": "windsurf", "github-copilot": "vscode-copilot",
    "zed": "zed", "grok": "grok", "codebuddy": ".codebuddy",
    "codebuddy-cn": "codebuddy-cn", "trae": "trae", "trae-solo": "trae-solo",
    "trae-solo-cn": "trae-solo-cn",
}

SKIP_DIRS = {"node_modules", ".git", "vendor_imports", "backups_state", "skills", "plugins"}
SCAN_SKIP = {".agents", ".cache", ".config", ".local", ".npm", ".cargo", ".conda", ".android"}


def pretty(dirname: str) -> str:
    name = dirname.lstrip(".") or "unknown"
    return re.sub(r"[-_]+", " ", name).strip().title() or name


def discover_sources():
    """[(source_id, label, kind, [root, ...])]，kind ∈ claude_jsonl | codex_jsonl | opencode_db | trae_memory | copilot_db"""
    out, known = [], set()

    def add(sid, label, kind, roots):
        roots = [r for r in roots if Path(r).exists()]
        if roots and sid not in known:
            known.add(sid)
            out.append((sid, label, kind, roots))

    for dname, label in NAMED_CLAUDE:
        root = HOME / dname
        add(dname, label, "claude_jsonl", [root / "projects", *sorted(root.glob("*/projects"))])

    # 新装 Agent 只要沿用 ~/.xxx/projects/<项目>/<会话>.jsonl 就自动纳入
    try:
        children = sorted(c for c in HOME.iterdir()
                          if c.is_dir() and c.name.startswith(".") and c.name not in SCAN_SKIP)
    except OSError:
        children = []
    for child in children:
        if child.name in known:
            continue
        prj = child / "projects"
        if prj.is_dir() and any(prj.glob("*/*.jsonl")):
            add(child.name, pretty(child.name), "claude_jsonl", [prj])

    codex = HOME / ".codex"
    add("codex", "Codex", "codex_jsonl", [codex / "sessions", codex / "archived_sessions"])
    add("opencode", "opencode", "opencode_db", [HOME / ".local/share/opencode/opencode.db"])
    add("zcode", "ZCode", "opencode_db", [HOME / ".zcode/cli/db/db.sqlite"])
    add(TRAE_SOURCE, TRAE_LABEL, "trae_memory", [HOME / ".trae-cn/memory/projects"])
    add("vscode-copilot", "VS Code Copilot", "copilot_db",
        [HOME / "AppData/Roaming/Code/User/globalStorage/github.copilot-chat"])
    add("dsh", "DeepSeek DSH", "dsh_jsonl_zst", [HOME / ".dsh/sessions"])

    # AStudio（Acode / 科大讯飞星辰）：rollout JSONL 在 acode-home-overlay/sessions/
    astudio_home = os.environ.get("AGENTFIND_ASTUDIO_HOME")
    if not astudio_home:
        local = os.environ.get("LOCALAPPDATA", "")
        astudio_home = str(Path(local) / "Programs/AStudio Data/acode-home-overlay/sessions") if local else ""
    if astudio_home:
        add("astudio", "AStudio", "astudio_rollout", [Path(astudio_home)])

    # ---- AgentCockpit registry 的 Agent：有数据根目录的照常扫，没数据的留给 overview 补 0 场 ----
    APPDATA = Path(os.environ.get("APPDATA", ""))
    agent_roots = {
        "qoder": [HOME / ".qoder/projects"],
        "qoder-cn": [HOME / ".qoder-cn/projects"],
        "qoder-cn-ide": [HOME / ".qoder-cn-ide/projects"],
        "workbuddy": [HOME / ".workbuddy/projects"],
        "workbuddy-intl": [HOME / ".workbuddy-ai/projects"],
        "astudio": [astudio_sessions],
        "minimax": [HOME / ".minimax/sessions"],
        "traework-cn": [HOME / ".trae-cn/memory/projects"],
        "zcode": [HOME / ".zcode/cli/db/db.sqlite"],
        "qwenwork": [HOME / ".qwenworkcn/projects"],
        "catpaw": [HOME / ".catpaw/projects", HOME / ".meituan-catpaw/projects"],
        "dsh": [HOME / ".dsh/sessions"],
        "marvis": [HOME / ".marvis"],
        "xiaomi-mimo": [APPDATA / "Xiaomi MiMo"],
        "opencode": [HOME / ".local/share/opencode/opencode.db"],
        "codex": [HOME / ".codex/sessions", HOME / ".codex/archived_sessions"],
        "codex-desktop": [APPDATA / "Codex/web/Codex"],
        "claude": [HOME / ".claude/projects"],
        "claude-desktop": [],
        "antigravity": [HOME / ".gemini/antigravity/conversation_summaries.db"],
        "coze": [HOME / ".coze/sessions", APPDATA / "Coze"],
        "cursor": [HOME / ".cursor/projects", APPDATA / "Cursor"],
        "kiro": [APPDATA / "Kiro"],
        "windsurf": [APPDATA / "Windsurf"],
        "github-copilot": [HOME / "AppData/Roaming/Code/User/globalStorage/github.copilot-chat"],
        "zed": [APPDATA / "Zed"],
        "grok": [HOME / ".grok", HOME / ".grokbot", APPDATA / "Grok Bot"],
        "codebuddy": [HOME / ".codebuddy/projects"],
        "codebuddy-cn": [HOME / ".codebuddy-cn/projects"],
        "trae": [APPDATA / "Trae"],
        "trae-solo": [APPDATA / "TRAE SOLO"],
        "trae-solo-cn": [APPDATA / "TRAE SOLO CN"],
    }
    agent_kind = {
        "astudio": "astudio_rollout",
        "antigravity": "gemini_db",
        "zcode": "opencode_db", "opencode": "opencode_db",
        "codex": "codex_jsonl", "dsh": "dsh_jsonl_zst",
        "traework-cn": "trae_memory", "github-copilot": "copilot_db",
    }
    for rid, rlabel in AGENT_REGISTRY:
        add(REGISTRY_MAP.get(rid, rid), rlabel, agent_kind.get(rid, "claude_jsonl"),
            agent_roots.get(rid, []))
    root_paths = [str(r) for _s, _l, _k, rs in out for r in rs]
    known = set()
    for s, label, _k, _rs in out:
        known.update(w.lower() for w in re.split(r"[^A-Za-z0-9]+", s + " " + label) if len(w) >= 4)
    anchors = (HOME, Path(os.environ.get("APPDATA", "")))
    for rp in root_paths:
        for anc in anchors:
            try:
                known.add(_token(Path(rp).relative_to(anc).parts[0]))
                break
            except (ValueError, IndexError):
                continue
    global UNRECOGNIZED
    extra, UNRECOGNIZED = _autodiscover(root_paths, known)
    for sid, label, kind, roots in extra:
        add(sid, label, kind, roots)
    return out


KIND_PATTERN = {"opencode_db": "*.db", "copilot_db": "session-store.db",
                "dsh_jsonl_zst": "*", "trae_memory": "*",
                "astudio_rollout": "*.jsonl", "gemini_db": "*.db"}

# 像 Agent 数据目录的名字特征；命中才去嗅探，避免把 Adobe/Ansys 那些也扫一遍
AGENTISH = re.compile(
    r"chat|agent|cli|code|copilot|llm|gpt|claude|codex|gemini|grok|marvis|mimo|dsh|qoder"
    r"|qwen|trae|workbuddy|catpaw|deepseek|kimi|minimax|doubao|cursor|windsurf|aider|claw"
    r"|buddy|opencode|zcode|genie|solob|session|conversation", re.I)
UNRECOGNIZED = []                       # 每次 discover_sources() 重建
_AUTO = {"t": 0.0, "sources": [], "unknown": []}
AUTO_TTL = 600                          # 秒：装软件不是高频事件，10 分钟内复用嗅探结果
AUTO_BUDGET = 6.0                       # 秒：一次发现最多花这么久，绝不拖住启动


def sniff_db_kind(f: Path):
    """看 sqlite 里的表集合判断它属于哪种格式家族，完全不认产品名。

    必须 immutable + timeout=0：候选库里常有别的产品正锁着的，默认超时会让一次嗅探
    卡好几秒；这里只要表名，读不到就跳过，不影响正确性。
    """
    try:
        con = sqlite3.connect(f.resolve().as_uri() + "?mode=ro&immutable=1",
                              uri=True, timeout=0)
        names = {r[0] for r in con.execute(
            "select name from sqlite_master where type='table'")}
        con.close()
    except sqlite3.Error:
        return ""
    if {"session", "message", "part"} <= names:
        return "opencode_db"
    if {"sessions", "turns"} <= names:
        return "copilot_db"
    return ""


DB_GLOBS = ("*.db", "*/db/*.db", "db/*.db", "*/*/db/*.db",
            "User/globalStorage/*/*.db", "User/workspaceStorage/*/*.db",
            "globalStorage/*/*.db", "data/*.db", "store/*.db")
CHAT_GLOBS = ("*/*.jsonl", "*/*/*.jsonl", "*/*.jsonl.zst", "*/*.jsonl.zstd",
              "*/*/*.jsonl.zst", "*/*/*.jsonl.zstd")


def _bounded(root: Path, patterns, limit=40):
    """只按固定深度几种写法找文件，绝不整树 rglob（APPDATA 下一个 Electron 目录能有几万个文件）。"""
    got, n = [], 0
    for pat in patterns:
        try:
            for f in root.glob(pat):
                if f.is_file():
                    got.append(f)
                    n += 1
                    if n >= limit:
                        return got
        except OSError:
            continue
    return got


def _token(name: str) -> str:
    return re.sub(r"[^a-z0-9]", "", name.lower())


def _autodiscover(known_paths, known_tokens):
    """按形状发现新装 Agent，返回 (sources, 还没解析器的可疑目录)。"""
    if time.time() - _AUTO["t"] < AUTO_TTL:
        return _AUTO["sources"], _AUTO["unknown"]
    found, unknown = [], []
    deadline = time.time() + AUTO_BUDGET
    sniffed, timed_out = 0, False
    for base in (HOME, Path(os.environ.get("APPDATA", ""))):
        try:
            entries = sorted(e for e in base.iterdir() if e.is_dir())
        except OSError:
            continue
        for e in entries:
            if time.time() > deadline or sniffed > 80:
                timed_out = True
                break
            if e.name in SCAN_SKIP or e.name in (".", ".."):
                continue
            if _token(e.name) in KNOWN_NO_CHAT:
                continue                          # 已确认无对话记录，别列进「没解析器」提示
            if any(len(t) >= 4 and (t in _token(e.name) or _token(e.name) in t)
                   for t in known_tokens):      # 已经用另一个根目录收录过了
                continue
            if any(p.lower().startswith(str(e).lower()) for p in known_paths):
                continue                          # 已收录的根目录就在这个候选目录里面
            looks_agent = bool(AGENTISH.search(e.name)) or any(
                (e / sub).is_dir() for sub in ("sessions", "projects", "chats", "db"))
            if not looks_agent:
                continue
            hit = ""
            prj = e / "projects"
            if prj.is_dir() and _bounded(prj, ("*/*.jsonl",), 1):
                found.append((e.name, pretty(e.name), "claude_jsonl", [prj]))
                hit = "claude_jsonl"
            sess = e / "sessions"
            if not hit and sess.is_dir() and _bounded(sess, CHAT_GLOBS, 1):
                found.append((e.name, pretty(e.name), "dsh_jsonl_zst", [sess]))
                hit = "dsh_jsonl_zst"
            if not hit:
                for db in _bounded(e, DB_GLOBS, 24):
                    sniffed += 1
                    kind = sniff_db_kind(db)
                    if kind:
                        found.append((e.name, pretty(e.name), kind, [db]))
                        hit = kind
                        break
                    if sniffed > 80 or time.time() > deadline:
                        timed_out = True
                        break
            if hit:
                known_tokens.add(_token(e.name))
                continue
            # 没命中格式，但有"存过聊天"的痕迹 → 如实报出来，别静默漏掉
            if _bounded(e, ("*.db", "*/*.db", "db/*.db"), 1) or _bounded(e, CHAT_GLOBS, 1) \
                    or (e / "IndexedDB").is_dir() or (e / "Local Storage" / "leveldb").is_dir():
                unknown.append(e.name.lstrip("."))
    _AUTO.update(sources=found, unknown=sorted(set(unknown))[:10],
                 t=time.time() - (AUTO_TTL - 60) if timed_out else time.time())
    return _AUTO["sources"], _AUTO["unknown"]


def iter_files(roots, pattern):
    for root in roots:
        root = Path(root)
        if root.is_file():
            yield root
            continue
        try:
            for p in root.rglob(pattern):
                if p.is_file() and not (SKIP_DIRS & set(p.parts)):
                    yield p
        except OSError:
            continue


# --------------------------------------------------------------------------
# 解析器
# --------------------------------------------------------------------------

SKIP_TYPES = {
    "reasoning", "thinking", "tool_result", "function_call_result", "local_command_output",
    "file-history-snapshot", "snapshot", "last-prompt", "active-leaf", "attachment",
    "token_count", "step-start", "step-finish", "compact_boundary", "progress", "summary",
}


def ingest(s: Parsed, rec: dict, ts):
    """把一条记录并入会话。按字段形状分派，兼容 Qoder/Claude 嵌套式、
    Codex payload 包裹式、WorkBuddy 扁平式三种记录布局。"""
    typ = rec.get("type")
    payload = rec.get("payload")
    if ts is None:
        ts = to_epoch(rec.get("timestamp"))
    if isinstance(payload, dict) and typ in ("session_meta", "turn_context",
                                             "response_item", "event_msg"):
        ingest(s, payload, ts or to_epoch(rec.get("timestamp")))
        return
    if typ in SKIP_TYPES:
        return
    if rec.get("cwd"):
        s.project = rec["cwd"]
    if rec.get("gitBranch"):
        s.branch = rec["gitBranch"]
    if rec.get("model"):
        s.models.add(str(rec["model"]))

    if typ in ("session_meta", "turn_context"):
        if rec.get("cwd"):
            s.project = rec["cwd"]
        return
    if typ == "workspace-directories":
        dirs = rec.get("directories") or []
        if dirs and not s.project:
            s.project = dirs[0]
        return
    if typ == "runtime-config":
        if rec.get("model"):
            s.models.add(str(rec["model"]))
        return
    if typ in ("user_message", "agent_message"):
        return      # Codex 会把同一轮对话再记一份事件，response_item 已收录
    if typ == "function_call":
        name = rec.get("name") or "tool"
        args = rec.get("arguments")
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except json.JSONDecodeError:
                args = {"cmd": args}
        collect_paths(args, s.artifacts)
        cmd = ""
        if isinstance(args, dict):
            cmd = str(args.get("cmd") or args.get("command") or args.get("file_path")
                      or args.get("path") or args.get("pattern") or "")[:500]
        s.add(ts, "tool", f"[调用 {name}] {cmd}".strip())
        return

    msg = rec.get("message") if isinstance(rec.get("message"), dict) else {}
    role = msg.get("role") or rec.get("role") or (typ if typ in ("user", "assistant") else None)
    content = rec.get("content") if rec.get("content") is not None else msg.get("content")
    if role not in ("user", "assistant") or content is None or rec.get("isMeta"):
        return
    blocks = content if isinstance(content, list) else [{"type": "text", "text": content}]
    for b in blocks:
        if not isinstance(b, dict):
            continue
        bt = b.get("type")
        if bt in ("text", "input_text", "output_text"):
            text = b.get("text") or ""
            if is_system_prompt(text) or text.lstrip().startswith(NOISE_PREFIX):
                continue
            paths_in_text(text, s.artifacts)
            s.add(ts, role, text)
        elif bt == "tool_use":
            name = b.get("name") or "tool"
            inp = b.get("input") if isinstance(b.get("input"), dict) else {}
            collect_paths(inp, s.artifacts)
            bits = [str(inp[k]).strip()[:500] for k in
                    ("command", "cmd", "pattern", "query", "prompt", "description", "url")
                    if isinstance(inp.get(k), str) and inp.get(k, "").strip()]
            s.add(ts, "tool", f"[调用 {name}] " + " | ".join(bits))


def desktop_signal(previous, rec, ts):
    """Only protocol-level boundaries trigger voice or focus, never assistant prose."""
    if not ts or rec.get("isSidechain") or (previous and previous[0] > ts):
        return previous
    kind = rec.get("type")
    if kind in ("event_msg", "response_item") and isinstance(rec.get("payload"), dict):
        rec = rec["payload"]
        kind = rec.get("type")
    message = rec.get("message") if isinstance(rec.get("message"), dict) else rec
    content = message.get("content")
    blocks = [b for b in content if isinstance(b, dict)] if isinstance(content, list) else []
    calls = [b for b in blocks if b.get("type") == "tool_use"]
    if kind == "function_call":
        calls.append(rec)
    for call in calls:
        name = str(call.get("name", "")).split(".")[-1].lower()
        if name not in ("askuserquestion", "ask_user_question", "request_user_input"):
            continue
        args = call.get("input", call.get("arguments", {}))
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except ValueError:
                args = {}
        questions = args.get("questions") if isinstance(args, dict) else None
        first = questions[0] if isinstance(questions, list) and questions else {}
        question = (first.get("question") or first.get("title")) if isinstance(first, dict) else ""
        return ts, "choice", str(question or "有一个选项等你确认")[:240], str(call.get("id") or call.get("call_id") or call.get("callId") or "")
    if kind == "task_complete" or (message.get("role") == "assistant" and message.get("stop_reason") == "end_turn"):
        summary = rec.get("last_agent_message") or " ".join(str(b.get("text") or "") for b in blocks if b.get("type") == "text")
        return ts, "complete", str(summary or "本轮已结束")[:240], ""
    if kind in ("turn_aborted", "task_started", "user_message"):
        return ts, "running", "", ""
    if kind in ("function_call_result", "function_call_output") and previous and previous[3]:
        if str(rec.get("call_id") or rec.get("callId") or "") == previous[3]:
            if not (isinstance(rec.get("output"), str) and "Request queued" in rec["output"]):
                return ts, "running", "", ""
    if message.get("role") == "user":
        results = [b for b in blocks if b.get("type") == "tool_result"]
        if not results or (previous and any(b.get("tool_use_id") == previous[3] for b in results)):
            return ts, "running", "", ""
    return previous


def parse_transcript_file(path: Path, source: str, label: str):
    """一个 .jsonl 会话记录文件 → 若干会话（同一文件可能含 sidechain）。"""
    sessions: dict[str, Parsed] = {}

    def get(sid):
        if sid not in sessions:
            sessions[sid] = Parsed(sid=sid, source=source, label=label, src_file=str(path))
        return sessions[sid]

    records = []
    for raw in read_lines(path):
        try:
            records.append(json.loads(raw))
        except json.JSONDecodeError:
            continue
    # Codex 把会话 id 写在 session_meta 里，优先用它，否则退回文件名
    real_id = next((str(r["payload"].get("id")) for r in records
                    if r.get("type") == "session_meta"
                    and isinstance(r.get("payload"), dict) and r["payload"].get("id")), None)
    fallback = real_id or path.stem
    for d in records:
        payload = d.get("payload") if isinstance(d.get("payload"), dict) else {}
        sid = str(d.get("sessionId") or payload.get("sessionId") or "") or fallback
        session = get(sid)
        ts = to_epoch(d.get("timestamp"))
        session.signal = desktop_signal(session.signal, d, ts)
        ingest(session, d, ts)
    return [s.finish() for s in sessions.values() if s.messages]


def parse_astudio_rollout(path: Path, sid="astudio", label="AStudio"):
    """AStudio（Acode / 科大讯飞星辰）：rollout JSONL 文件在
    acode-home-overlay/sessions/YYYY/MM/DD/rollout-*.jsonl。

    格式：每行 {"type": "...", "timestamp": "...", "payload": {...}}
    - 一个 rollout 文件 = 一场会话：session_meta.payload.session_id（UUID）是会话 id，
      其余记录的 payload.id 是 message/call/reasoning 的 id，**不是**会话 id。
    - session_meta: payload.session_id / cwd / timestamp
    - turn_context: payload.cwd / workspace_roots
    - event_msg: payload.type ∈ {user_message, agent_message, agent_reasoning, ...}
      payload.message 是**纯字符串**（不是带 role/content 的 dict）。
    - response_item: payload.type ∈ {message, function_call, function_call_output, reasoning}
      message 型 record 的 payload.message 为空，正文只在 event_msg 里。
    """
    current_id = path.stem
    s = None

    def cur():
        nonlocal s
        if s is None:
            s = Parsed(sid=current_id, source=sid, label=label, src_file=str(path))
        return s

    for raw in read_lines(path):
        try:
            rec = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if not isinstance(rec, dict):
            continue
        typ = rec.get("type") or ""
        payload = rec.get("payload") if isinstance(rec.get("payload"), dict) else {}
        ts = to_epoch(rec.get("timestamp"))

        if typ == "session_meta":
            current_id = str(payload.get("session_id") or "") or path.stem
            s = Parsed(sid=current_id, source=sid, label=label, src_file=str(path))
            if payload.get("cwd"):
                s.project = str(payload["cwd"])
            if ts:
                s.started = ts if s.started is None else min(s.started, ts)
        elif typ == "turn_context":
            sc = cur()
            if payload.get("cwd") and not sc.project:
                sc.project = str(payload["cwd"])
        elif typ == "event_msg":
            sc = cur()
            sc.signal = desktop_signal(sc.signal, rec, ts)
            ptype = payload.get("type") or ""
            msg = payload.get("message")
            if ptype == "user_message" and isinstance(msg, str) and msg:
                paths_in_text(msg, sc.artifacts)
                sc.add(ts, "user", msg)
            elif ptype == "agent_message" and isinstance(msg, str) and msg:
                paths_in_text(msg, sc.artifacts)
                sc.add(ts, "assistant", msg)
            elif ptype == "agent_reasoning":
                text = payload.get("text")
                if isinstance(text, str) and text:
                    sc.add(ts, "assistant", text)
        elif typ == "response_item":
            sc = cur()
            sc.signal = desktop_signal(sc.signal, rec, ts)
            ptype = payload.get("type") or ""
            if ptype == "function_call":
                name = payload.get("name") or "tool"
                args = payload.get("arguments")
                if isinstance(args, str):
                    try:
                        args = json.loads(args)
                    except json.JSONDecodeError:
                        args = {"cmd": args}
                collect_paths(args, sc.artifacts)
                cmd = ""
                if isinstance(args, dict):
                    cmd = str(args.get("cmd") or args.get("command") or args.get("file_path")
                              or args.get("path") or args.get("pattern") or "")[:500]
                sc.add(ts, "tool", f"[调用 {name}] {cmd}".strip())

    return [sc.finish() for sc in ([s] if s else []) if sc.messages]


def _gemini_project(workspace_uris):
    """从 Antigravity 的 workspace_uris JSON 里取项目名（目录最后一段）。"""
    if not workspace_uris:
        return ""
    try:
        arr = json.loads(workspace_uris)
    except (ValueError, TypeError):
        return ""
    if not isinstance(arr, list) or not arr:
        return ""
    p = str(arr[0])
    if p.startswith("file://"):
        p = p[len("file://"):]
        p = unquote(p).lstrip("/")
    return Path(p).name or p


def parse_gemini_antigravity(path: Path, sid="antigravity", label="Antigravity"):
    """Google Gemini / Antigravity 会话库：~/.gemini/antigravity/conversation_summaries.db
    （~/.gemini 与 Antigravity 整机共享，见 AgentCockpit registry.rs 注释）。

    conversation_summaries 表：每行一场会话（conversation_id / title / preview /
    step_count / last_modified_time / workspace_uris）。
    conversations_fts 表：每场按 chunk 存 user_body / agent_body 正文，chunk_index>=0
    才是正文块（chunk_index=-1 是标题/项目行，正文为空）。
    """
    try:
        con = sqlite3.connect(path.resolve().as_uri() + "?mode=ro&immutable=1", uri=True)
        con.row_factory = sqlite3.Row
    except sqlite3.Error:
        return []
    sessions = {}
    try:
        try:
            summaries = list(con.execute(
                "select conversation_id,title,preview,last_modified_time,workspace_uris "
                "from conversation_summaries"))
        except sqlite3.Error:
            summaries = []
        for r in summaries:
            cid = str(r["conversation_id"])
            p = Parsed(sid=cid, source=sid, label=label, src_file=str(path))
            p.title = (r["title"] or r["preview"] or "").strip() or "(无标题)"
            p.started = to_epoch(r["last_modified_time"])
            p.ended = p.started
            p.project = _gemini_project(r["workspace_uris"])
            sessions[cid] = p
        try:
            chunks = list(con.execute(
                "select conversation_id,chunk_index,user_body,agent_body "
                "from conversations_fts where chunk_index>=0 order by chunk_index"))
        except sqlite3.Error:
            chunks = []
        for r in chunks:
            cid = str(r["conversation_id"])
            p = sessions.get(cid)
            if p is None:
                continue
            for role, body in (("user", r["user_body"]), ("assistant", r["agent_body"])):
                text = (body or "").strip()
                if text:
                    paths_in_text(text, p.artifacts)
                    p.add(p.started, role, text)
    except sqlite3.Error:
        return []
    finally:
        con.close()
    return [p.finish() for p in sessions.values() if p.messages]


def parse_opencode_db(path: Path, sid="opencode", label="opencode"):
    """opencode / ZCode：session / message / part 三张表，正文在 part.data。"""
    con = sqlite3.connect(path.resolve().as_uri() + "?mode=ro&immutable=1", uri=True)
    con.row_factory = sqlite3.Row
    out = []
    try:
        sess = list(con.execute("select id,title,directory,time_created,time_updated "
                                "from session order by time_created"))
        parts, msgs = {}, {}
        for r in con.execute("select message_id,data from part"):
            parts.setdefault(r["message_id"], []).append(r["data"])
        for r in con.execute("select id,session_id,time_created,data from message order by time_created,id"):
            msgs.setdefault(r["session_id"], []).append(r)
    except sqlite3.Error:
        con.close()
        return out
    con.close()

    for sr in sess:
        sid_ = str(sr["id"])
        p = Parsed(sid=sid_, source=sid, label=label, src_file=str(path))
        p.project = sr["directory"] or ""
        p.title = (sr["title"] or "").strip()
        p.started = to_epoch(sr["time_created"])
        p.ended = to_epoch(sr["time_updated"]) or p.started
        for mr in msgs.get(sid_, []):
            try:
                md = json.loads(mr["data"])
            except (json.JSONDecodeError, TypeError):
                continue
            role = md.get("role")
            if role not in ("user", "assistant"):
                continue
            mm = md.get("model")
            model = md.get("modelID") or (mm.get("modelID") if isinstance(mm, dict) else mm)
            if model:
                p.models.add(str(model))
            ts = to_epoch(mr["time_created"])
            if role == "user" and ts:
                p.signal = (ts, "running", "", "")
            reply = []
            for raw in parts.get(mr["id"], []):
                try:
                    b = json.loads(raw)
                except (json.JSONDecodeError, TypeError):
                    continue
                bt = b.get("type")
                if bt == "text" and b.get("text"):
                    paths_in_text(b["text"], p.artifacts)
                    p.add(ts, role, b["text"])
                    if role == "assistant":
                        reply.append(str(b["text"])[:240])
                elif bt == "file":
                    np = norm_path(b.get("url") or b.get("filename") or "")
                    if np:
                        p.artifacts.add(np)
                elif bt == "tool":
                    st = b.get("state") if isinstance(b.get("state"), dict) else {}
                    inp = st.get("input") if isinstance(st.get("input"), dict) else {}
                    if b.get("tool") == "question" and st.get("status") == "running":
                        p.signal = desktop_signal(p.signal, {"type":"function_call", "name":"ask_user_question",
                                                             "input":inp, "id":b.get("callID")}, ts)
                    elif b.get("tool") == "question" and st.get("status") in ("completed", "error") and p.signal and p.signal[1] == "choice":
                        p.signal = (ts, "running", "", "")
                    collect_paths(inp, p.artifacts)
                    cmd = str(inp.get("command") or inp.get("filePath") or inp.get("path") or "")[:500]
                    p.add(ts, "tool", f"[调用 {b.get('tool') or 'tool'}] {cmd}".strip())
            if role == "assistant" and md.get("finish") == "stop" and ts and not (p.signal and p.signal[1] == "choice"):
                completed = md.get("time") or {}
                done_at = to_epoch(completed.get("completed")) if isinstance(completed, dict) else None
                p.signal = (done_at or ts, "complete", " ".join(reply)[:240] or "本轮已结束", "")
        if p.messages:
            out.append(p.finish())
    return out


def _zstd_decompress(raw: bytes):
    """DSH 的会话是 zstd 压缩的 JSONL；3.14+ 用标准库，否则退回可选的 zstandard。"""
    try:
        from compression import zstd
        return zstd.decompress(raw)
    except ImportError:
        pass
    try:
        import zstandard
        return zstandard.ZstdDecompressor().decompress(raw, max_output_size=64 << 20)
    except Exception:
        return b""


def parse_dsh_file(path: Path, sid="dsh", label="DeepSeek DSH"):
    """DeepSeek DSH：~/.dsh/sessions/<工作区>/session-<uuid>/session.vN.jsonl.zstd。"""
    raw = _zstd_decompress(path.read_bytes())
    if not raw:
        return []
    p = None
    for line in raw.decode("utf-8", "ignore").splitlines():
        if not line.strip():
            continue
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(r, dict):
            continue
        kind = r.get("type")
        if kind == "session":
            p = Parsed(sid=str(r.get("id") or path.parent.name), source=sid, label=label,
                       src_file=str(path))
            p.project = str(r.get("cwd") or "")
            p.started = to_epoch(r.get("createdAt"))
            continue
        if p is None or not isinstance(r.get("data"), dict):
            continue
        ts = to_epoch(r.get("time")) or p.started
        d = r["data"]
        # system/assistant 把内容嵌在 data.message 里，user/message 直接放 data 上
        msg = d.get("message") if isinstance(d.get("message"), dict) else d
        role = msg.get("role")
        if isinstance(role, str) and role in ("user", "assistant"):
            content = msg.get("content")
            blocks = [{"type": "text", "text": content}] if isinstance(content, str) \
                else (content or [])
            for b in blocks:
                if isinstance(b, dict) and b.get("type") == "text" and b.get("text"):
                    paths_in_text(b["text"], p.artifacts)
                    p.add(ts, role, b["text"])
        elif kind == "tool/call":
            inp = d.get("args") if isinstance(d.get("args"), dict) else d.get("input")
            inp = inp if isinstance(inp, dict) else {}
            collect_paths(inp, p.artifacts)
            bits = [str(inp[k]).strip()[:400] for k in ("command", "path", "file_path", "pattern")
                    if isinstance(inp.get(k), str) and inp.get(k, "").strip()]
            p.add(ts, "tool", f"[调用 {d.get('name') or d.get('tool') or 'tool'}] "
                              + " | ".join(bits))
        p.ended = max(p.ended or ts or 0, ts or 0)
    return [p.finish()] if p and p.messages else []


def parse_copilot_db(path: Path, sid="vscode-copilot", label="VS Code Copilot"):
    """VS Code Copilot：session-store.db 的 sessions + turns。

    turns 一行是一个来回（user_message / assistant_response 两列），不是 role 字段。
    只读打开**不能**带 immutable=1：这库常年挂着 -wal，immutable 会读不到 WAL 里的新行。
    """
    out = []
    try:
        con = sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)
        con.row_factory = sqlite3.Row
        sess = list(con.execute(
            "select id,cwd,summary,created_at,updated_at from sessions"))
        turns = {}
        for t in con.execute("select session_id,turn_index,user_message,"
                             "assistant_response,timestamp from turns "
                             "order by session_id,turn_index"):
            turns.setdefault(t["session_id"], []).append(t)
        con.close()
    except sqlite3.Error:
        return out
    for sr in sess:
        rows = turns.get(str(sr["id"]), [])
        if not rows and not (sr["summary"] or "").strip():
            continue
        p = Parsed(sid=str(sr["id"]), source=sid, label=label, src_file=str(path))
        p.project = sr["cwd"] or ""
        p.title = (sr["summary"] or "").strip()
        p.started = to_epoch(sr["created_at"])
        p.ended = to_epoch(sr["updated_at"]) or p.started
        for t in rows:
            ts = to_epoch(t["timestamp"])
            if (t["user_message"] or "").strip():
                paths_in_text(t["user_message"], p.artifacts)
                p.add(ts, "user", t["user_message"])
            if (t["assistant_response"] or "").strip():
                paths_in_text(t["assistant_response"], p.artifacts)
                p.add(ts, "assistant", t["assistant_response"])
        if p.messages or p.title:
            out.append(p.finish())
    return out


TRAE_SUM_FILE = re.compile(r"^session_memory_(.+)\.jsonl$")
DSH_FILE = re.compile(r"^session\..*\.jsonl\.(zst|zstd)$")
TRAE_TOPIC = re.compile(r"^##\s+\[session:\s*([^\]]+)\]\s*(.+)$")
# 从明文里抽绝对路径：盘符 + 至少 3 个非分隔字符，遇空白/引号/括号停
PATH_IN_TEXT = re.compile(r"[A-Za-z]:[\\/][^\s\"'`,;()<>|]{2,120}")
_TRAE_PROJECT = {}
_TRAE_CANDIDATES = None


def _trae_norm(p: str) -> str:
    """Trae 生成 slug 前的路径规范化：反斜杠→/、去盘符冒号、盘符小写、去尾 /。"""
    p = p.strip().replace("\\", "/")
    m = re.match(r"^([A-Za-z]):", p)
    if m:
        p = "/" + m.group(1).lower() + p[2:]
    return p.rstrip("/")


def _trae_slug_match(slug: str, path: str) -> bool:
    """复现 Trae 的 slug 算法做校验：slug 里中文全被换成 '-' 不可逆，只能正向算再比对。

    主体 = 规范化路径(带尾 /)的每个非字母数字字符换成 '-'；
    尾段 = -p2- + sha256(规范化路径)[:20]。普通项目 = 主体-p2-hash；
    内部 work-mode 项目 = 主体<随机6位>--p2-hash（随机段插在 pid 和双横线之间）。
    """
    norm = _trae_norm(path)
    h = hashlib.sha256(norm.encode("utf-8")).hexdigest()[:20]
    bodies = [re.sub(r"[^A-Za-z0-9]", "-", norm + "/")]
    parts = norm.split("/")     # /c/Users/<谁>/… 的内部项目，slug 主体会省掉前两段
    if len(parts) > 4:
        bodies.append(re.sub(r"[^A-Za-z0-9]", "-", "/".join(parts[3:]) + "/"))
    for body in bodies:
        if slug == body + "-p2-" + h:
            return True
        if re.fullmatch(re.escape(body) + r"[a-z0-9]{4,8}--p2-" + h, slug):
            return True
    return False


def _trae_candidates():
    """Trae 打开过哪些项目：TRAE 自己的 workspace.json + work-mode 目录 +
    ~/.trae-cn 明文里出现过的绝对路径（展开到祖先目录）。

    本机实测 workspace.json 只有 2 个，光靠它几乎什么都还原不出来，
    真正有货的是 memory 明文里提到的路径。
    """
    global _TRAE_CANDIDATES
    if _TRAE_CANDIDATES is not None:
        return _TRAE_CANDIDATES
    raw, out = set(), set()

    appdata = Path(os.environ.get("APPDATA", ""))
    trae = appdata / "TRAE SOLO CN"
    try:
        wjs = [*trae.glob("*/*/workspace.json"), *trae.glob("*/workspace.json")]
        for wj in wjs:
            raw.update(re.findall(PATH_IN_TEXT, unquote(
                wj.read_text(encoding="utf-8", errors="ignore"))))
    except OSError:
        pass
    # state.vscdb 的 ItemTable.value 是 TEXT JSON，SQLite 页里能直接按字节搜出 file:/// URI，
    # 不用打开数据库。这里藏着 Trae 打开过的项目目录。
    for db in trae.glob("User/**/state.vscdb"):
        try:
            data = db.read_bytes()
        except OSError:
            continue
        for m in re.findall(rb"file:///([^\s\"'<>\\|`]+)", data):
            raw.add(unquote(m.decode("utf-8", "ignore")).replace("/", "\\"))
    for wm in (trae / "ModularData" / "ai-agent" / "work-mode-projects",
               trae / "ModularData" / "ai-agent" / "work-mode" / "projects"):
        try:
            raw.update(str(x) for x in wm.iterdir() if x.is_dir())
        except OSError:
            pass
    mem = HOME / ".trae-cn"
    try:
        files = [f for f in mem.rglob("*") if f.is_file() and f.suffix in (".md", ".json", ".jsonl")]
    except OSError:
        files = []
    for f in files[:2500]:
        try:
            if f.stat().st_size > 2_000_000:
                continue
            raw.update(re.findall(PATH_IN_TEXT, f.read_text(encoding="utf-8", errors="ignore")))
        except OSError:
            continue

    for p in raw:
        q = Path(p.strip().rstrip("\"'.,:;"))
        try:
            if not q.is_dir():
                q = q.parent
        except (OSError, ValueError):
            continue
        while len(str(q)) > 3 and str(q) not in out:
            out.add(str(q))
            if q.parent == q:
                break
            q = q.parent

    # 最后兜底：有 sha256 校验，候选多一点无所谓——直接枚举 home 与 D:\ 下 ≤3 层的真实目录。
    # 这条路不依赖 Trae 在哪记录过路径，目录在盘上就能被 hash 验明正身。
    skip = {".git", ".vscode", ".idea", "__pycache__", "node_modules", "appdata",
            "windows", "program files", "program files (x86)", "perflogs", "recovery",
            "system volume information", "documents and settings"}
    stack = [(r, 0) for r in (HOME, Path("D:/")) if r.is_dir()]
    while stack and len(out) < 30000:
        d, depth = stack.pop()
        out.add(str(d))
        if depth >= 3:
            continue
        try:
            for e in os.scandir(d):
                if e.is_dir(follow_symlinks=False) and not e.name.startswith((".", "$")) \
                        and e.name.lower() not in skip:
                    stack.append((Path(e.path), depth + 1))
        except OSError:
            continue
    _TRAE_CANDIDATES = sorted(out)
    return _TRAE_CANDIDATES


def _trae_project_path(mem_root: Path, slug: str):
    """还原 slug 对应的真实项目路径：先同目录 project_memory.md，再拿候选路径复算 slug 比对。"""
    if slug not in _TRAE_PROJECT:
        real = ""
        try:
            head = (mem_root / slug / "project_memory.md").read_text(
                encoding="utf-8", errors="ignore")[:2000]
            m = re.search(r"项目路径[：:]\s*(\S.*\S)", head)
            if m:
                real = m.group(1).strip().strip('`"')
        except OSError:
            pass
        if not real:
            for cand in _trae_candidates():
                if _trae_slug_match(slug, cand):
                    real = cand
                    break
        _TRAE_PROJECT[slug] = real
    return _TRAE_PROJECT[slug]


def _trae_parts(path: Path):
    """~/.trae-cn/memory/projects/<slug>/<YYYYMMDD>/file → (项目路径, day, day_epoch)"""
    day = path.parent.name
    slug = path.parent.parent.name if path.parent.parent else ""
    mem_root = path.parent.parent.parent if path.parent.parent else path.parent
    project = _trae_project_path(mem_root, slug) or slug
    try:
        return project, day, datetime.strptime(day, "%Y%m%d").timestamp()
    except ValueError:
        return project, "", None


def _trae_new(path: Path, sid: str, project: str, title: str):
    p = Parsed(sid="trae:" + sid, source=TRAE_SOURCE, label=TRAE_LABEL, src_file=str(path))
    p.project = project
    p.title = title[:160]
    return p


def parse_trae_memory(path: Path):
    """TraeWork CN：正文库加密，只能索引它自己落盘的会话摘要与每日 topics.md。"""
    project, day, day_ts = _trae_parts(path)
    out = []

    if TRAE_SUM_FILE.match(path.name):
        for line in read_lines(path):
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            intent = str(rec.get("intent") or "").strip()
            if not intent:
                continue
            ts = to_epoch(rec.get("message_summary_time")) or day_ts
            p = _trae_new(path, str(rec.get("message_id") or f"{day}-{intent[:12]}"), project, intent)
            bits = [f"【意图】{intent}"]
            for key, tag in (("actions", "【动作】"), ("outcome", "【结论】"), ("learned", "【学到】")):
                val = rec.get(key)
                if val:
                    bits.append(tag + ("；".join(str(x) for x in val) if isinstance(val, list) else str(val)))
            body = "\n".join(bits)
            paths_in_text(body, p.artifacts)
            p.add(ts, "summary", body)
            if p.messages:
                out.append(p.finish())
        return out

    if path.name == "topics.md":
        try:
            text = path.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            return out
        m = re.search(r"^#\s*(\d{4}-\d{2}-\d{2})", text, re.M)
        ts = to_epoch(m.group(1) + " 12:00:00") if m else day_ts
        blocks, cur = [], None
        for ln in text.splitlines():
            b = TRAE_TOPIC.match(ln.strip())
            if b:
                cur = {"key": b.group(1).strip(), "title": b.group(2).strip(), "body": []}
                blocks.append(cur)
            elif cur is not None and ln.strip():
                cur["body"].append(ln.strip().lstrip("- ").strip())
        for i, b in enumerate(blocks):
            body = "\n".join(b["body"])
            p = _trae_new(path, f"topic-{day}-{b['key']}-{i}", project, b["title"])
            paths_in_text(body, p.artifacts)
            p.add(ts, "summary", f"【{b['title']}】\n{body}" if body else b["title"])
            if p.messages:
                out.append(p.finish())
        return out

    return []


# --------------------------------------------------------------------------
# 索引
# --------------------------------------------------------------------------

SCHEMA = """
create table if not exists meta(k text primary key, v text);
create table if not exists file(path text primary key, sig text, source text);
create table if not exists session(
  sid text primary key, source text, label text, title text, project text,
  project_name text, started real, ended real, day text, n_msg integer,
  models text, branch text, src_file text, artifacts text,
  auto integer not null default 0);
create table if not exists desktop_signal(sid text primary key, ts real not null, kind text not null, summary text, call_id text);
create index if not exists desktop_signal_ts on desktop_signal(ts);
create index if not exists session_day on session(day);
create virtual table if not exists msg_fts using fts5(
  body, sid unindexed, role unindexed, ts unindexed, day unindexed, seq unindexed,
  tokenize='trigram');
"""


def connect():
    APP_DIR.mkdir(parents=True, exist_ok=True)
    # 所有读写都在 IDX_LOCK 内串行，因此可以让工作线程共用这一个连接
    con = sqlite3.connect(str(DB_PATH), timeout=60, isolation_level=None,
                          check_same_thread=False)
    con.row_factory = sqlite3.Row
    con.executescript(SCHEMA)
    try:    # 老库补列：ALTER ADD COLUMN 是瞬时操作，列已存在则报错吞掉
        con.execute("alter table session add column auto integer not null default 0")
    except sqlite3.OperationalError:
        pass
    con.execute("pragma journal_mode=WAL")
    con.execute("pragma synchronous=NORMAL")
    return con


_RCON = threading.local()


def read_con():
    """读路径专用的只读连接（每线程一条）。WAL 允许"一写多读"并发，所以后台首轮扫描
    跑十几秒期间，页面照样能立刻出结果——不然所有查询都得排队等那把应用级锁，
    表现就是"每次刚打开都卡半天"。"""
    c = getattr(_RCON, "c", None)
    if c is not None:
        return c
    try:
        c = sqlite3.connect(f"file:{DB_PATH}?mode=ro", uri=True, timeout=30,
                            check_same_thread=False)
    except sqlite3.Error:
        # 库刚建好还没落盘、或 WAL 需要恢复：退回普通连接，别把页面打死
        c = connect()
    c.row_factory = sqlite3.Row
    _RCON.c = c
    return c


def file_sig(path: Path):
    st = path.stat()
    return f"{int(st.st_mtime)}:{st.st_size}"


def project_name(p):
    if not p:
        return "(未知项目)"
    return str(Path(str(p).rstrip("/\\")).name) or p


def drop_session(con, sid):
    con.execute("delete from msg_fts where sid=?", (sid,))
    con.execute("delete from session where sid=?", (sid,))
    con.execute("delete from desktop_signal where sid=?", (sid,))


def store_session(con, p: Parsed):
    sid = f"{p.source}|{p.sid}"
    drop_session(con, sid)
    con.execute(
        "insert into session(sid,source,label,title,project,project_name,started,ended,"
        "day,n_msg,models,branch,src_file,artifacts,auto) values(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        (sid, p.source, p.label, p.title or "(无标题)", p.project, project_name(p.project),
         p.started, p.ended, day_of(p.ended or p.started), len(p.messages),
         ",".join(sorted(p.models))[:200], p.branch, p.src_file,
         "\n".join(sorted(p.artifacts)), p.auto))
    if p.signal:
        con.execute("insert or replace into desktop_signal values(?,?,?,?,?)", (sid, *p.signal))
    con.executemany(
        "insert into msg_fts(body,sid,role,ts,day,seq) values(?,?,?,?,?,?)",
        [(text, sid, role, ts, day_of(ts), seq)
         for seq, (ts, role, text) in enumerate(p.messages, 1)])


def refresh(con, sources, force=False, log=None):
    """增量刷新：按 mtime+size 只重读变过的会话文件。"""
    say = log or (lambda *_: None)
    t0 = time.time()
    with IDX_LOCK:
        seen, n_new, n_upd, n_skip = set(), 0, 0, 0
        existing = {} if force else {r["path"]: r["sig"]
                                    for r in con.execute("select path,sig from file")}
        if force:
            con.execute("delete from file")

        def handle(path, parsed_list, source):
            nonlocal n_new, n_upd, n_skip
            sp = str(path)
            sig = file_sig(path)
            if not force and existing.get(sp) == sig:
                n_skip += 1
                return
            if sp in existing:
                n_upd += 1
            else:
                n_new += 1
            keep = {f"{source}|{p.sid}" for p in parsed_list}
            old = {r["sid"] for r in con.execute(
                "select sid from session where src_file=?", (sp,))}
            for gone in old - keep:
                drop_session(con, gone)
            for p in parsed_list:
                store_session(con, p)
            con.execute("insert or replace into file(path,sig,source) values(?,?,?)",
                        (sp, sig, source))

        con.execute("begin")
        try:
            for sid, label, kind, roots in sources:
                b_new, b_upd = n_new, n_upd
                for f in iter_files(roots, KIND_PATTERN.get(kind, "*.jsonl")):
                    seen.add(str(f))
                    try:
                        if kind == "opencode_db":
                            handle(f, parse_opencode_db(f, sid, label), sid)
                        elif kind == "copilot_db":
                            handle(f, parse_copilot_db(f, sid, label), sid)
                        elif kind == "dsh_jsonl_zst":
                            if DSH_FILE.match(f.name):
                                handle(f, parse_dsh_file(f, sid, label), sid)
                        elif kind == "trae_memory":
                            handle(f, parse_trae_memory(f), sid)
                        elif kind == "astudio_rollout":
                            handle(f, parse_astudio_rollout(f, sid, label), sid)
                        elif kind == "gemini_db":
                            handle(f, parse_gemini_antigravity(f, sid, label), sid)
                        else:
                            handle(f, parse_transcript_file(f, sid, label), sid)
                    except Exception as e:
                        say(f"  ! {f.name}: {type(e).__name__}: {e}")
                touched = (n_new - b_new) + (n_upd - b_upd)
                say(f"  {label}: " + (f"索引 {touched} 个记录文件" if touched else "无变化"))

            for r in list(con.execute("select path from file")):
                if r["path"] not in seen and not Path(r["path"]).exists():
                    for s in list(con.execute("select sid from session where src_file=?",
                                              (r["path"],))):
                        drop_session(con, s["sid"])
                    con.execute("delete from file where path=?", (r["path"],))
            con.execute("insert or replace into meta(k,v) values('last_refresh',?)",
                        (str(time.time()),))
            con.execute("commit")
        except Exception:
            con.execute("rollback")
            raise
        stats = {"new": n_new, "updated": n_upd, "skipped": n_skip, "secs": time.time() - t0}
    say(f"刷新完成：新增 {n_new}、更新 {n_upd}、未变 {n_skip}，耗时 {stats['secs']:.1f}s")
    return stats


def rebuild(con, sources, log=None):
    with IDX_LOCK:
        con.execute("delete from session")
        con.execute("delete from msg_fts")
        con.execute("delete from file")
    return refresh(con, sources, force=True, log=log)


# 标题规则升级后的一次性回填：老索引里 Codex 系会话的标题全是注入包装的抬头
# （"AGENTS.md instructions for ..."），按现行规则从 user 消息重算。
# 只动"标题由解析器推导"的来源；Trae 这类产品自带的标题不碰。
DERIVED_TITLE_SOURCES = ("codex", "codex-desktop")
TITLE_SCHEMA_VERSION = 4


def retitles(con):
    """按现行标题规则重算 Codex 系会话的 title。靠 PRAGMA user_version 记账，
    跑过就不再跑。msg_fts 按 rowid 顺序整表扫一遍（每场内部即 seq 顺序）。"""
    with IDX_LOCK:
        ver = con.execute("pragma user_version").fetchone()[0]
        if ver >= TITLE_SCHEMA_VERSION:
            return 0
        picks = {}
        for row in con.execute("select sid, body from msg_fts where role='user'"):
            sid = row["sid"]
            if picks.get(sid):
                continue
            pick = _title_from_user(row["body"])
            if pick:
                picks[sid] = pick[:160]
        marks = ",".join("?" * len(DERIVED_TITLE_SOURCES))
        rows = con.execute(
            f"select sid,title from session where source in ({marks})",
            DERIVED_TITLE_SOURCES).fetchall()
        n = 0
        for r in rows:
            t = picks.get(r["sid"])
            if t and t != r["title"]:
                con.execute("update session set title=? where sid=?", (t, r["sid"]))
                n += 1
        con.execute(f"pragma user_version={TITLE_SCHEMA_VERSION}")
        return n


# --------------------------------------------------------------------------
# 检索
# --------------------------------------------------------------------------

RUN_RE = re.compile(r"[A-Za-z0-9]+(?:[._\-][A-Za-z0-9]+)*|[\u3400-\u4dbf\u4e00-\u9fff]+")


def split_terms(q):
    """先按空白切，再在 ASCII↔中文 交界处切：「ChatGPT打不开」→ 两个线索。

    只有当切出的每块都 ≥2 字、且拼回去恰好等于原串时才切；
    文件名（带点）、带标点的串、含单字的串都保持原样，免得 AND 语义把它们查死。
    """
    out = []
    for tok in re.split(r"\s+", (q or "").strip()):
        if not tok:
            continue
        parts = RUN_RE.findall(tok)
        if len(parts) > 1 and "".join(parts) == tok and all(len(p) >= 2 for p in parts):
            out += parts
        else:
            out.append(tok)
    return out[:8]


def cjk_blocks(term, size=3):
    return [term[i:i + size] for i in range(0, len(term) - size + 1, size)]


def scope_sql(sources, day_from, day_to, alias):
    conds, params = [], []
    if sources:
        conds.append(f"{alias}.source in ({','.join('?' * len(sources))})")
        params += list(sources)
    if day_from:
        conds.append(f"{alias}.day >= ?")
        params.append(day_from)
    if day_to:
        conds.append(f"{alias}.day <= ?")
        params.append(day_to)
    return (" and " + " and ".join(conds)) if conds else "", params


def fts_match(term):
    return '"' + term.replace('"', '""') + '"'


def term_hits(con, term, sources, day_from, day_to):
    """单个线索 → ({sid: 命中消息数}, 只在标题/项目/产物里命中的 sid 集合)。"""
    cond, params = scope_sql(sources, day_from, day_to, "s")
    counts = {}
    if len(term) >= TRIGRAM_MIN:
        sql = ("select msg_fts.sid sid, count(*) c from msg_fts "
               "join session s on s.sid = msg_fts.sid "
               f"where msg_fts match ?{cond} group by msg_fts.sid")
        try:
            rows = con.execute(sql, (fts_match(term), *params))
        except sqlite3.OperationalError:
            rows = []
    else:
        sql = ("select msg_fts.sid sid, count(*) c from msg_fts "
               "join session s on s.sid = msg_fts.sid "
               f"where instr(lower(msg_fts.body),?)>0{cond} group by msg_fts.sid")
        rows = con.execute(sql, (term.lower(), *params))
    for r in rows:
        counts[r["sid"]] = r["c"]

    like = f"%{term.lower()}%"
    meta = {r["sid"] for r in con.execute(
        "select sid from session where lower(title) like ? or lower(project) like ? "
        "or lower(project_name) like ? or lower(artifacts) like ? or lower(src_file) like ?",
        (like,) * 5)}
    for sid in meta:
        counts.setdefault(sid, 0)
    return counts, meta


def session_json(meta, hits=0, score=0, snips=None, is_auto=None):
    """session 行 → 前端卡片要的字段；检索和浏览两条路共用这一份。"""
    return {
        "sid": meta["sid"], "label": meta["label"], "source": meta["source"],
        "title": meta["title"], "project": meta["project"],
        "project_name": meta["project_name"], "started": meta["started"],
        "ended": meta["ended"], "day": meta["day"], "n_msg": meta["n_msg"],
        "models": [m for m in (meta["models"] or "").split(",") if m],
        "branch": meta["branch"], "src_file": meta["src_file"],
        "artifacts": [a for a in (meta["artifacts"] or "").split("\n") if a],
        "hits": hits, "score": score, "snippets": snips or [],
        "open_level": opener_for(meta["source"])["level"],
        "auto": int(bool(meta["auto"] if is_auto is None else is_auto)),
    }


def session_index(con):
    """整份会话元数据（不含消息正文）给前端做即时筛选与排序。
    本机回环传 1MB 只要十几毫秒，比每次点标签都跑一趟数据库快得多。"""
    with NO_LOCK:
        rows = con.execute(
            "select * from session order by coalesce(ended,started) desc").fetchall()
    out = []
    for m in rows:
        d = session_json(m)
        d["artifacts"] = d["artifacts"][:14]
        out.append(d)
    return out


def recent_sessions(con, sources=None, day_from=None, day_to=None, limit=60,
                    hide_auto=False, project=None, project_query=None):
    """没输关键词时的浏览：按时间倒序列出筛选范围内的会话，返回 (结果, 符合筛选的总数)。
    产品标签因此能单独当筛选器用——想只看某个 Agent 的历史，点标签就行，不必先想一个词。"""
    limit = max(1, min(limit, 300))
    conds, params = [], []
    if sources:
        conds.append("source in (%s)" % ",".join("?" * len(sources)))
        params += list(sources)
    if day_from:
        conds.append("day>=?")
        params.append(day_from)
    if day_to:
        conds.append("day<=?")
        params.append(day_to)
    if hide_auto:
        conds.append("auto=0")
    if project:
        conds.append("project_name=?")
        params.append(project)
    w = (" where " + " and ".join(conds)) if conds else ""
    with NO_LOCK:
        total = con.execute("select count(*) c from session" + w, params).fetchone()["c"]
        rows = con.execute("select * from session" + w
                           + " order by coalesce(ended,started) desc", params).fetchall()
        if project_query:
            needle = project_query.casefold().replace("\\", "/")
            rows = [row for row in rows if needle in (row["project"] or "").casefold().replace("\\", "/")
                    or needle in (row["project_name"] or "").casefold()]
            total = len(rows)
        rows = rows[:limit]
    return [session_json(r) for r in rows], total


def search(con, q, sources=None, day_from=None, day_to=None, limit=60, sort="relevance",
           hide_auto=False, project=None, project_query=None):
    terms = split_terms(q)
    if not terms:
        return []
    limit = max(1, min(limit, 300))
    with NO_LOCK:
        per_term = [term_hits(con, t, sources, day_from, day_to) for t in terms]
        cand = set(per_term[0][0])
        for counts, _ in per_term[1:]:
            cand &= set(counts)
        if not cand and len(terms) == 1 and len(terms[0]) >= 6 \
                and re.fullmatch(r"[\u3400-\u4dbf\u4e00-\u9fff]+", terms[0]):
            # 整串查不到：多半是把几个关键词连着粘在一起了。退化成 3 字块 OR，
            # 但要求至少两块同时命中，避免单块把无关会话捞上来。
            blocks = [b for b in cjk_blocks(terms[0]) if len(b) >= TRIGRAM_MIN]
            if len(blocks) >= 2:
                per_term = [term_hits(con, b, sources, day_from, day_to) for b in blocks]
                seen = {}
                for counts, _ in per_term:
                    for sid in counts:
                        seen[sid] = seen.get(sid, 0) + 1
                cand = {sid for sid, k in seen.items() if k >= 2}
        if project:
            cand &= {r["sid"] for r in con.execute(
                "select sid from session where project_name=?", (project,))}
        scope, params = scope_sql(sources, day_from, day_to, "s")
        if scope:
            cand &= {r["sid"] for r in con.execute("select s.sid from session s where 1=1" + scope, params)}
        if project_query:
            needle = project_query.casefold().replace("\\", "/")
            cand &= {r["sid"] for r in con.execute("select sid,project,project_name from session")
                     if needle in (r["project"] or "").casefold().replace("\\", "/")
                     or needle in (r["project_name"] or "").casefold()}
        if not cand:
            return []
        auto = {r["sid"] for r in con.execute(
            f"select sid from session where auto=1 and sid in ({','.join('?' * len(cand))})",
            tuple(cand))}
        if hide_auto:
            cand -= auto
            if not cand:
                return []

        scored = []
        for sid in cand:
            score = hits = 0
            for counts, meta_only in per_term:
                n = counts.get(sid, 0)
                hits += n
                score += min(n, 20) + (6 if sid in meta_only else 0)
            if sid in auto:
                score *= 0.35      # 产品自动任务排在真人对话后面，但不消失
            scored.append((score, hits, sid))
        if sort in ("time", "time_asc"):
            times = {r["sid"]: r["ended"] or r["started"] or 0 for r in con.execute(
                "select sid,ended,started from session")}
            scored.sort(key=lambda t: (times.get(t[2], 0), t[2]), reverse=sort == "time")
        else:
            scored.sort(key=lambda t: -t[0])
        top = scored[:limit]
        ids = [t[2] for t in top]

        infos = {r["sid"]: dict(r) for r in con.execute(
            f"select * from session where sid in ({','.join('?' * len(ids))})", ids)}
        snips = fetch_snippets(con, terms, ids)
        results = []
        for score, hits, sid in top:
            meta = infos.get(sid)
            if not meta:
                continue
            snips_ = snips.get(sid, [])
            if not snips_:
                html = meta_snippet(meta, terms)
                if html:
                    snips_ = [{"role": "meta", "ts": meta["ended"] or meta["started"],
                               "day": meta["day"], "html": html}]
            results.append(session_json(meta, hits, score, snips_, sid in auto))
    if sort == "time":
        results.sort(key=lambda r: -(r["ended"] or r["started"] or 0))
    elif sort == "time_asc":
        results.sort(key=lambda r: (r["ended"] or r["started"] or 0))
    return results


def fetch_snippets(con, terms, sids):
    """给最靠前的若干结果取带高亮的上下文片段。

    每个会话单独取最近 6 条（row_number 分区），只回传命中位置附近的小窗口。
    不能用一个全局 limit：SQLite 按 rowid 截断，消息量最大的数据源会把后面的
    数据源全部挤光，表现就是它们明明命中几十次却一条片段都没有。
    """
    out = {s: [] for s in sids}
    if not sids:
        return out
    marks = ",".join("?" * len(sids))
    rn = "row_number() over (partition by msg_fts.sid order by msg_fts.ts desc) rn from msg_fts "
    cols = "select msg_fts.sid sid, msg_fts.role role, msg_fts.ts ts, msg_fts.day day, "
    for term in terms:
        picked = []
        if len(term) >= TRIGRAM_MIN:
            # snippet() 不能和窗口函数同层使用，也不允许在 join 之后再取（会退化成文档开头且无高亮），
            # 所以先按会话挑出 rowid，再对这批 rowid 单独取高亮片段。
            sel = (cols + "msg_fts.rowid rid, " + rn
                   + f"where msg_fts match ? and msg_fts.sid in ({marks})")
            try:
                picked = [(r["sid"], r["role"], r["ts"], r["day"], r["rid"])
                          for r in con.execute(f"select * from ({sel}) where rn <= 6",
                                               (fts_match(term), *sids))]
            except sqlite3.OperationalError:
                picked = []
            rids = [p[4] for p in picked]
            snips = {}
            if rids:
                sql = ("select msg_fts.rowid rid, snippet(msg_fts,0,'<b>','</b>',' … ',20) txt "
                       f"from msg_fts where msg_fts match ? and msg_fts.rowid in "
                       f"({','.join('?' * len(rids))})")
                try:
                    snips = {r["rid"]: r["txt"] for r in con.execute(sql, (fts_match(term), *rids))}
                except sqlite3.OperationalError:
                    snips = {}
            for sid, role, ts, day, rid in picked:
                if rid in snips:
                    out[sid].append({"role": role, "ts": ts, "day": day, "html": snips[rid]})
        else:
            low = term.lower()
            sel = (cols + "substr(msg_fts.body, max(1, instr(lower(msg_fts.body), ?) - 70), 280) txt, "
                   + rn + f"where instr(lower(msg_fts.body),?)>0 and msg_fts.sid in ({marks})")
            try:
                for r in con.execute(f"select sid,role,ts,day,txt from ({sel}) where rn <= 6",
                                     (low, low, *sids)):
                    out[r["sid"]].append({"role": r["role"], "ts": r["ts"], "day": r["day"],
                                          "html": make_snippet(r["txt"], [term])})
            except sqlite3.OperationalError:
                pass
    for sid, lst in out.items():
        seen, dedup = set(), []
        for s in sorted(lst, key=lambda x: -(x["ts"] or 0)):
            key = re.sub(r"<[^>]+>", "", s["html"])[:70]
            if key not in seen:
                seen.add(key)
                dedup.append(s)
        out[sid] = dedup[:4]
    return out


def meta_snippet(meta, terms):
    """正文里没有片段时，指出线索命中的其实是标题 / 项目 / 产物名。"""
    low = [t.lower() for t in terms]
    cands = [meta.get("title"), meta.get("project")]
    cands += [a for a in (meta.get("artifacts") or "").split("\n") if a]
    for val in cands:
        v = (val or "").split("\n")[0]
        if v and any(t in v.lower() for t in low):
            return make_snippet(v, terms)
    return ""


def make_snippet(text, terms):
    text = text or ""
    low = text.lower()
    for t in terms:
        i = low.find(t.lower())
        if i >= 0:
            a = max(0, i - 70)
            return ("…" if a else "") + esc(text[a:i]) + "<b>" + esc(text[i:i + len(t)]) \
                + "</b>" + esc(text[i + len(t):i + len(t) + 150])
    return esc(text[:180]) + ("…" if len(text) > 180 else "")


def esc(s):
    return str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def session_detail(con, sid):
    with NO_LOCK:
        meta = con.execute("select * from session where sid=?", (sid,)).fetchone()
        if not meta:
            return None
        msgs = con.execute(
            "select seq,role,ts,day,body as text from msg_fts where sid=? order by seq",
            (sid,)).fetchall()
    out = dict(meta)
    out["open_level"] = opener_for(meta["source"])["level"]
    return {"session": out, "messages": [dict(m) for m in msgs]}


# --------------------------------------------------------------------------
# 跳回原对话
# --------------------------------------------------------------------------

# level：chat = 直接进那场对话；app = 只能把应用唤到前台（顺带复制会话 ID 供应用内搜索）；
#         file = 这个产品没有对外跳转入口，退而在资源管理器里定位原始记录。
# 只有实测或包内代码确认过的才标 chat，其余宁可低报，别让按钮骗人。
# 2026-09-12 实测：workbuddy://chat/<id> 真能打开那场对话（截图确认）；codex resume <id> 能定位到
# 对应 rollout 文件；qoder-cn://chat/<id> 只会把应用唤到前台、不切会话，所以停在 app 档。
# 同族的 qwenwork-cn / qoder 同理未验证，谁实测通了再把 level 改成 chat。
OPENERS = {
    ".workbuddy": {"level": "chat", "kind": "url", "tpl": "workbuddy://chat/{sid}"},
    ".qoder-cn": {"level": "app", "kind": "url", "tpl": "qoder-cn://chat/{sid}"},
    ".qwenworkcn": {"level": "app", "kind": "url", "tpl": "qwenwork-cn://chat/{sid}"},
    ".qoder": {"level": "app", "kind": "url", "tpl": "qoder://chat/{sid}"},
    "codex": {"level": "chat", "kind": "console", "argv": ("codex", "resume", "{sid}")},
    "zcode": {"level": "app", "kind": "url", "tpl": "zcode://"},
    "opencode": {"level": "app", "kind": "url", "tpl": "opencode://open-project?directory={dir}"},
    TRAE_SOURCE: {"level": "file", "kind": "file"},
}
DEFAULT_OPENER = {"level": "file", "kind": "file"}
OPEN_TEXT = {"chat": "已打开那场对话", "app": "已唤起应用", "file": "已定位原始记录"}


def opener_for(source):
    return OPENERS.get(source, DEFAULT_OPENER)


def reveal_file(path):
    """在资源管理器里选中这个文件；文件已删则退回打开它所在目录。"""
    f = Path(path or "")
    if f.exists():
        subprocess.Popen(["explorer", "/select," + str(f)])
        return True
    if f.parent.is_dir():
        subprocess.Popen(["explorer", str(f.parent)])
        return True
    return False


def open_session(con, sid):
    """跳回原对话。只接受索引里已存在的 sid，参数一律按列表交给 CreateProcess，不拼 shell。"""
    with NO_LOCK:
        row = con.execute("select source, project, src_file from session where sid=?",
                          (sid,)).fetchone()
    if row is None:
        return {"error": "这场会话不在索引里，刷新一次再试"}
    op = opener_for(row["source"])
    raw = sid.split("|", 1)[1] if "|" in sid else sid
    err = ""
    try:
        if op["kind"] == "url":
            os.startfile(op["tpl"].format(sid=quote(raw, safe=""),
                                          dir=quote(row["project"] or "", safe="")))
        elif op["kind"] == "console":
            if not re.fullmatch(r"[\w.:\-]{1,80}", raw):
                return {"error": "会话 ID 格式异常，不启动外部命令"}
            argv = [a.replace("{sid}", raw) for a in op["argv"]]
            proj = (row["project"] or "").strip()
            cwd = Path(proj) if proj else None
            # .cmd 包装不能直接 CreateProcess，只能借 cmd 走一遍 PATH + PATHEXT；
            # 用 /k 而不是 /c，这样 resume 失败时窗口还留着能看到报错。
            subprocess.Popen(["cmd", "/d", "/k"] + argv,
                             cwd=str(cwd) if cwd and cwd.is_dir() else None,
                             creationflags=getattr(subprocess, "CREATE_NEW_CONSOLE", 0))
        else:
            if not reveal_file(row["src_file"]):
                return {"error": "原始记录文件已经不在了"}
            return {"ok": True, "level": "file", "text": OPEN_TEXT["file"], "copy": ""}
        return {"ok": True, "level": op["level"], "text": OPEN_TEXT[op["level"]],
                "copy": raw if op["level"] == "app" else ""}
    except (OSError, ValueError) as e:
        err = f"{type(e).__name__}: {e}"
    if reveal_file(row["src_file"]):
        return {"ok": True, "level": "file", "text": f"跳不过去（{err}），已改为定位原始记录",
                "copy": ""}
    return {"error": err}


def example_terms(con):
    """首页示例线索：从索引里挑真实存在的项目名与产物文件名。

    不能把话题写死在代码里——那等于把使用者的私人上下文印在源码上。
    """
    with NO_LOCK:
        projs = [r["p"].strip() for r in con.execute(
            "select project_name p from session where project_name<>'' "
            "and project_name<>'(未知项目)' group by p "
            "order by count(*) desc, max(ended) desc limit 6").fetchall()]
        arts = [r["a"] for r in con.execute(
            "select artifacts a from session where artifacts<>'' "
            "order by ended desc limit 60").fetchall()]
    out, seen = [], set()

    def push(t):
        t = (t or "").strip().split("\n")[0]
        key = t.lower()
        if 2 <= len(t) <= 24 and key not in seen:
            seen.add(key)
            out.append(t)

    for p in projs:
        if len(out) < 3:
            push(p)
    for blob in arts:
        for line in blob.split("\n"):
            base = line.strip().replace("\\", "/").split("/")[-1]
            if base.split(".")[-1].lower() in ("docx", "pptx", "xlsx", "md", "py", "pdf"):
                push(base)
        if len(out) >= 6:
            break
    for fb in ("报错", "总结", "README"):
        if len(out) >= 6:
            break
        if fb not in seen:
            push(fb)
    return out[:6]


def overview(con):
    """汇总索引概况。sources 里包含所有已发现的数据源 + AgentCockpit registry 的全部 Agent
    （包括 0 场的），让用户能看到全部产品，而不是只看到有数据的那几个。"""
    with NO_LOCK:
        # 有会话的数据源计数
        counts = {r["source"]: r for r in con.execute(
            "select source,count(*) n,max(ended) b from session "
            "group by source")}
        # 合并所有已发现的数据源（discover_sources 自带 10 分钟缓存，不会每次都重扫）
        all_srcs = []
        seen = set()
        for sid, label, _kind, _roots in discover_sources():
            seen.add(sid)
            c = counts.get(sid)
            all_srcs.append({
                "source": sid, "label": label,
                "n": c["n"] if c else 0,
                "b": c["b"] if c else None,
            })
        # AgentCockpit registry 里有、但此刻没探到数据目录的：仍然按索引里的真实场数报。
        # 客户端升级会把 SQLite 文件短暂挪走，写死 0 会让用户以为历史没了。
        for rid, rlabel in AGENT_REGISTRY:
            sid = REGISTRY_MAP.get(rid, rid)
            if sid in seen:
                continue
            seen.add(sid)
            c = counts.get(sid)
            all_srcs.append({"source": sid, "label": rlabel,
                             "n": c["n"] if c else 0, "b": c["b"] if c else None})
        # 有数据的排前面，没数据的排后面；各自按数量/名称排
        all_srcs.sort(key=lambda s: (-s["n"], s["label"]))
        tot = con.execute("select count(*) c from session").fetchone()["c"]
        days = con.execute("select count(distinct day) c from session where day<>''").fetchone()["c"]
        msgs = con.execute("select count(*) c from msg_fts").fetchone()["c"]
        last = con.execute("select v from meta where k='last_refresh'").fetchone()
    return {"sources": all_srcs, "total": tot, "days": days, "messages": msgs,
            "uncovered": UNCOVERED, "unrecognized": UNRECOGNIZED,
            "no_chat": KNOWN_NO_CHAT_DIRS,
            "examples": example_terms(con),
            "last_refresh": float(last["v"]) if last else None}


_LIVE_TAIL = {}
_LIVE_LOCK = threading.Lock()


def desktop_feed(con):
    # The regular index discovers new sessions. Between scans, read only the tail of
    # active JSONL files so an unanswered question can surface while the agent waits.
    now = time.time()
    select = """select s.sid,s.label,s.title,s.project_name,s.ended,s.source,s.src_file,
                 d.ts,d.kind,d.summary,d.call_id from session s left join desktop_signal d on d.sid=s.sid """
    recent = [dict(r) for r in con.execute(select + "where s.auto=0 order by s.ended desc limit 16")]
    active = {r["sid"]:dict(r) for r in con.execute(select + "where s.auto=0 and (s.ended>? or d.ts>?)", (now-86400,now-86400))}
    # An old indexed session may be resumed today. Its indexed timestamp does not
    # change until the next scan, so check the file's modification time as well.
    for row in con.execute(select + "where s.auto=0 and coalesce(s.ended,0)<=? and coalesce(d.ts,0)<=?", (now-86400, now-86400)):
        path = Path(row["src_file"])
        if path.suffix == ".jsonl":
            try:
                if path.stat().st_mtime > now-300:
                    active[row["sid"]] = dict(row)
            except OSError:
                pass
    notifications = []
    with _LIVE_LOCK:
        for sid, row in active.items():
            previous = (row["ts"],row["kind"],row["summary"],row["call_id"]) if row["ts"] else None
            path = Path(row["src_file"])
            if path.suffix == ".jsonl":
                try:
                    st = path.stat()
                    if st.st_mtime > now-300:
                        sig = (st.st_mtime_ns,st.st_size,previous)
                        cached = _LIVE_TAIL.get(sid)
                        if not cached or cached[0] != sig:
                            signal = previous
                            with path.open("rb") as fp:
                                start = max(0, st.st_size-262144)
                                fp.seek(start)
                                if start:
                                    fp.readline()
                                for line in fp.read(262144).splitlines():
                                    try:
                                        rec = json.loads(line)
                                    except (ValueError, UnicodeError):
                                        continue
                                    if isinstance(rec, dict) and (not rec.get("sessionId") or str(rec["sessionId"]) == sid.split("|",1)[-1]):
                                        signal = desktop_signal(signal,rec,to_epoch(rec.get("timestamp")))
                            _LIVE_TAIL[sid] = sig, signal
                        previous = _LIVE_TAIL[sid][1]
                except OSError:
                    pass
            if previous and previous[1] in ("choice","complete") and previous[0] > now-86400:
                notifications.append({"sid":sid,"label":row["label"],"kind":previous[1],"at":previous[0],
                                      "summary":previous[2],"title":row["title"],
                                      "can_focus":opener_for(row["source"])["kind"]=="url"})
        for sid in _LIVE_TAIL.keys()-active.keys():
            del _LIVE_TAIL[sid]
    notifications.sort(key=lambda e:e["at"],reverse=True)
    return {"events":notifications, "recent":[{k:r[k] for k in ("sid","label","title","project_name","ended")} for r in recent],
            "indexed_at":STATE["last_scan"]}


def browse(con):
    with NO_LOCK:
        rows = con.execute(
            "select sid,label,source,title,project_name,n_msg,started,day from session "
            "order by coalesce(ended,started) desc limit 400").fetchall()
    groups = []
    for r in rows:
        d = r["day"] or "未知日期"
        if not groups or groups[-1]["day"] != d:
            groups.append({"day": d, "items": []})
        groups[-1]["items"].append(dict(r))
    return groups


# --------------------------------------------------------------------------
# 服务
# --------------------------------------------------------------------------

STATE = {"progress": "正在扫描本机 Agent 记录…", "ready": False, "last_scan": 0.0}
REFRESH_MIN_INTERVAL = 45     # 秒。扫描在后台线程循环跑，查询请求永远只读索引，不陪它等


def serve(port, open_browser, initial_query):
    url = f"http://127.0.0.1:{port}/"
    if initial_query:
        url += "?q=" + quote(initial_query)
    if already_running(port):
        print(f"已有 AgentFind 服务在运行 → {url}")
        write_port_file(port)
        if open_browser:
            webbrowser.open(url)
        return

    con = connect()

    def worker():
        first = True
        while True:
            try:
                if first:
                    try:
                        n = retitles(con)
                        if n:
                            print(f"标题规则升级：重命名 {n} 场会话")
                    except Exception as e:
                        print(f"标题回填失败：{type(e).__name__}: {e}")
                # 每轮重新发现一次源：新装/卸载的 Agent 就是这么被跟上的（discover 自带 10 分钟缓存）
                stats = refresh(con, discover_sources(), log=print if first else None)
                STATE["progress"] = (f"索引已就绪：新增 {stats['new']}、更新 {stats['updated']} 个记录文件")
            except Exception as e:
                STATE["progress"] = f"索引失败：{type(e).__name__}: {e}"
                print(STATE["progress"])
            STATE["ready"] = True
            STATE["last_scan"] = time.time()
            first = False
            time.sleep(REFRESH_MIN_INTERVAL)

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):
            pass

        def _send(self, raw, ctype):
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(raw)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(raw)

        def _json(self, obj, code=200):
            self._send(json.dumps(obj, ensure_ascii=False).encode("utf-8"),
                       "application/json; charset=utf-8")

        def do_GET(self):
            u = urlparse(self.path)
            q = {k: v[0] for k, v in parse_qs(u.query).items()}
            try:
                if u.path in ("/", "/index.html"):
                    self._send(PAGE.encode("utf-8"), "text/html; charset=utf-8")
                elif u.path == "/api/desktop-feed":
                    self._json(desktop_feed(read_con()))
                elif u.path == "/api/agentfind":
                    self._json({"service": "AgentFind", "sessions_ui": 1,
                                "script": SCRIPT_FINGERPRINT})
                elif u.path == "/api/meta":
                    ov = overview(read_con())
                    ov.update(progress=STATE["progress"], ready=STATE["ready"], pid=os.getpid(),
                              registry_sources=REGISTRY_MAP)
                    self._json(ov)
                elif u.path == "/api/sessions":
                    self._json({"sessions": session_index(read_con()), "last_scan": STATE["last_scan"],
                                "progress": STATE["progress"], "ready": STATE["ready"]})
                elif u.path == "/api/search":
                    # 只有显式点「刷新索引」才同步扫描；平时后台线程在刷，查询一律只读索引
                    forced = q.get("fresh") == "1"
                    if forced:
                        refresh(con, discover_sources(), force=False)
                        STATE["last_scan"] = time.time()
                    srcs = [s for s in q.get("sources", "").split(",") if s] or None
                    kw = (q.get("q") or "").strip()
                    flt = dict(sources=srcs, day_from=q.get("from") or None,
                               day_to=q.get("to") or None, project=q.get("project") or None,
                               project_query=(q.get("project_query") or "").strip() or None,
                               limit=min(int(q.get("limit", 60)), 300),
                               hide_auto=q.get("auto") == "0")
                    if kw:
                        res = search(read_con(), kw, sort=q.get("sort", "relevance"), **flt)
                        total, browsing = None, False
                    else:
                        res, total = recent_sessions(read_con(), **flt)
                        browsing = True
                    self._json({"results": res, "total": total, "browse": browsing,
                                "progress": STATE["progress"], "scanned": forced,
                                "last_scan": STATE["last_scan"]})
                elif u.path == "/api/session":
                    self._json(session_detail(read_con(), q.get("id", "")) or {"error": "会话不在索引中"})
                elif u.path == "/api/browse":
                    self._json({"days": browse(read_con())})
                else:
                    self._json({"error": "not found"}, 404)
            except Exception as e:
                self._json({"error": f"{type(e).__name__}: {e}"}, 500)

        def do_POST(self):
            """只有 /api/open：会启动本机应用，所以要求自定义头 + 同源，挡掉别的网页跨站调用。"""
            u = urlparse(self.path)
            if u.path != "/api/open":
                return self._json({"error": "not found"}, 404)
            if self.headers.get("X-Requested-With") != "agentfind":
                return self._json({"error": "forbidden"}, 403)
            origin = (urlparse(self.headers.get("Origin") or "").netloc).lower()
            bound = self.server.server_address[1]
            if origin and origin not in (f"127.0.0.1:{bound}", f"localhost:{bound}"):
                return self._json({"error": "forbidden"}, 403)
            try:
                n = min(int(self.headers.get("Content-Length") or 0), 4096)
                sid = str(json.loads(self.rfile.read(n).decode("utf-8") or "{}").get("sid", ""))[:200]
                self._json(open_session(read_con(), sid))
            except Exception as e:
                self._json({"error": f"{type(e).__name__}: {e}"}, 500)

    ThreadingHTTPServer.allow_reuse_address = True
    httpd = None
    for cand in range(port, port + PORT_SPAN):
        try:
            srv = ThreadingHTTPServer(("127.0.0.1", cand), Handler)
        except OSError:
            continue
        # 探活必须在后台扫描启动之前做：首轮 refresh 一边写库一边让人读 /api/meta，
        # 慢到 3s 以上甚至回 500，探活就会把归我们的好端口误判成"没抢到"、白往后漂。
        threading.Thread(target=srv.serve_forever,
                         kwargs={"poll_interval": 0.05}, daemon=True).start()
        if is_me_running(cand):
            httpd, port = srv, cand
            break
        print(f"端口 {cand} 上应答的不是本进程（被别的程序影子绑定了），换下一个")
        srv.shutdown()
        srv.server_close()
    if httpd is None:
        print(f"{port} 往后 {PORT_SPAN} 个端口都没真抢到——多半是被别的程序用 "
              f"SO_REUSEADDR 影子绑了（bind 成功但请求到不了我们）。"
              f"换个端口试试：agentfind --port 9000")
        con.close()
        return
    threading.Thread(target=worker, daemon=True).start()
    url = f"http://127.0.0.1:{port}/"
    if initial_query:
        url += "?q=" + quote(initial_query)
    write_port_file(port)
    print(f"AgentFind → {url}")
    print("只读索引，不改任何原始记录。Ctrl+C 停止服务。")
    if open_browser:
        threading.Timer(1.5, lambda: webbrowser.open(url)).start()
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        print("\n已停止。")
    finally:
        httpd.server_close()


# --------------------------------------------------------------------------
# 前端
# --------------------------------------------------------------------------

# T19 部分迁：728 行内嵌前端 PAGE 挪到独立静态资源 agentfind-page.html（纯搬运，
# 字节不变）。脚本指纹仍按本文件字节算（_script_fingerprint），PAGE 内容不再计入
# 脚本指纹——前端改动不再触发服务重启。加载失败时给一个最小错误页，不静默白屏。
def _load_page():
    try:
        return (Path(__file__).with_name("agentfind-page.html")).read_text(encoding="utf-8")
    except OSError:
        return ("<!doctype html><meta charset=utf-8><title>AgentFind</title>"
                "<body style='font-family:sans-serif;padding:2em'>"
                "会话索引前端资源缺失（agentfind-page.html），请重新连接驾驶舱以修复。</body>")


PAGE = _load_page()


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def _http_json(path, port):
    with local_open(f"http://127.0.0.1:{port}{path}", timeout=180) as r:
        return json.loads(r.read().decode("utf-8"))


def cmd_status(port):
    if already_running(port):
        # 服务在跑就直接问它：再开一个写者会和它抢 SQLite 锁（database is locked）
        ov = _http_json("/api/meta", port)
    else:
        con = connect()
        ov = overview(con)
        con.close()
    counts = {s["source"]: s for s in ov["sources"]}
    print(f"索引文件: {DB_PATH}")
    print(f"{'产品':<22}{'会话数':>8}   最近活跃")
    print("-" * 58)
    for sid, label, _kind, _roots in discover_sources():
        c = counts.get(sid)
        print(f"{label:<20}{(c['n'] if c else 0):>8}   {fmt_time(c['b']) if c else '—'}")
    print("-" * 58)
    print(f"合计 {ov['total']} 场对话、{ov['messages']} 条消息，跨 {ov['days']} 天")
    if ov["last_refresh"]:
        print(f"索引更新于 {fmt_time(ov['last_refresh'])}")


def cmd_cli(q, port):
    if already_running(port):
        res = _http_json("/api/search?q=" + quote(q) + "&limit=25", port)["results"]
    else:
        con = connect()
        refresh(con, discover_sources(), log=lambda *_: None)
        res = search(con, q, limit=25)
        con.close()
    if not res:
        print("没有命中。试试更短的关键词，或只搜产物文件名。")
        return
    for r in res:
        print(f"\n[{r['label']}] {fmt_time(r['started'])}  {r['title']}")
        print(f"    项目   {r['project']}")
        print(f"    记录   {r['src_file']}")
        print(f"    命中   {r['hits']} 处" + (f"，产物 {len(r['artifacts'])} 个" if r["artifacts"] else ""))
        for s in r["snippets"][:2]:
            print(f"      · {re.sub('<[^>]+>', '', s['html'])[:180]}")


def cmd_stop(port):
    """停掉本机所有 AgentFind 服务进程（索引文件保留）。

    Windows 上 allow_reuse_address 允许多个进程绑同一端口，改完代码后旧实例可能仍在服务，
    所以这里按命令行匹配全部清掉，而不是只杀端口上那一个。
    """
    was = already_running(port)
    ps = ("Get-CimInstance Win32_Process | "
          "Where-Object { $_.Name -match '^python' -and $_.CommandLine -like '*agentfind.py*' "
          "-and $_.ProcessId -ne " + str(os.getpid()) + " } | "
          "ForEach-Object { $_.ProcessId }")
    try:
        out = subprocess.run(["powershell", "-NoProfile", "-Command", ps],
                             capture_output=True, text=True,
                             encoding="gbk", errors="replace", timeout=30).stdout
    except (OSError, subprocess.SubprocessError):
        out = ""
    pids = [x.strip() for x in out.splitlines() if x.strip().isdigit()]
    for pid in pids:
        subprocess.run(["taskkill", "/PID", pid, "/F"], capture_output=True)
    if pids:
        print(f"已停止 {len(pids)} 个 AgentFind 服务进程（{', '.join(pids)}）。"
              f"索引保留在 {DB_PATH}，双击桌面图标即可再起。")
    elif was:
        print(f"端口 {port} 上有服务但没找到可结束的进程，请用任务管理器确认。")
    else:
        print(f"没有正在运行的 AgentFind 服务，无需关闭。")


def main(argv=None):
    ap = argparse.ArgumentParser(prog="agentfind", description="本机多 Agent 对话全局检索")
    ap.add_argument("query", nargs="*", help="搜索线索，自动填入网页搜索框")
    ap.add_argument("--status", action="store_true", help="打印索引状态后退出")
    ap.add_argument("--rebuild", action="store_true", help="全量重建索引后退出")
    ap.add_argument("--cli", metavar="Q", help="终端里直接搜索，不起服务")
    ap.add_argument("--stop", action="store_true", help="关闭本机 AgentFind 服务后退出")
    ap.add_argument("--port", type=int, default=None,
                    help=f"服务端口，默认读 ~/.agentfind/running.port，没有则 {DEFAULT_PORT}")
    ap.add_argument("--no-open", action="store_true", help="不自动打开浏览器")
    a = ap.parse_args(argv)
    attach_log()
    # 没显式给端口时跟着实际在跑的那个走：自动换过口之后还按默认口去探，会把
    # "服务活着" 误判成 "没跑"，于是又起一个实例来抢索引库的写锁。
    port = a.port if a.port is not None else (read_port_file() or DEFAULT_PORT)

    if a.status:
        cmd_status(port)
    elif a.cli:
        cmd_cli(a.cli, port)
    elif a.stop:
        cmd_stop(port)
    elif a.rebuild:
        con = connect()
        print("全量重建索引…")
        rebuild(con, discover_sources(), log=print)
        con.close()
        cmd_status(port)
    else:
        serve(port, not a.no_open, " ".join(a.query))
    return 0


if __name__ == "__main__":
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass
    main()
