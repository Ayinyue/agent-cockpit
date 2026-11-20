# 运行效率优化计划（含 Python 运行时 Rust 化）

> 范围：`agent-cockpit`（Tauri 2 + Rust 后端 + `ui/` 纯静态前端 + 内置 Python 运行时）。
> 目标：降低空闲时的 CPU / 磁盘占用与界面卡顿，缩短"点一下到出结果"的等待；并把**自研逻辑**从 Python 迁到 Rust、砍掉运行时与进程开销。
> 原则：**先测量、后优化**；每条改动都要有可复现的验收指标；不牺牲正确性与隐私（密钥不进日志、不落地明文）；dev / release 分开评估。
>
> 本文件由 2026-11-18 的代码审查产出，审查方式见附录 A。**标「实测」的数字是本机跑出来的，标「推断」的尚未验证。**

---

## 1. 现状基线

### 1.1 已确认的实测数据

| 项 | 数值 | 来源 |
|---|---|---|
| 用量缓存总量 | **141.4 MB / 10 个文件** | `~/.agent-cockpit/usage-cache/`，实测 |
| 其中最大单文件 | 67.8 MB / 841 个条目 / 约 15.3 万条记录 | 同上 |
| 该文件一次 `read` | 0.08 s | 内置 python 3.14 实测 |
| 该文件一次 `json.loads` | **4.48 s** | 同上 |
| 该文件一次 `json.dumps` | **3.10 s** | 同上 |
| `agenthub.py --help`（冷启） | 0.75 ~ 1.2 s（冷启更久） | 审查实测 |
| `powershell -NoProfile -NonInteractive` 冷启 | 本机 3.2 ~ 5.1 s/次 | 实测（**机器高负载下**，见 1.3） |
| `cryptography` 的 import | 约 0.49 s | 审查实测 |
| `config.json` | 1.7 KB | 实测 |

### 1.2 结构性事实（代码确认）

- 前端默认 **每 4 秒** `invoke('get_state')` 一次并整页重渲染（`ui/app.js:10318`）。
- 后端 `state.rs:103-209` 每次 `get_state` 都会：读配置 → 读设置 → `all_apps()` → 逐产品 `load_instances` → 逐账号 `authpack::stored`。
- 用量采集（`scripts/usage-local.py`）每轮**整读**缓存（`:531`），任一文件指纹变化就**整写**（`:585`）。
- 反代（`proxy.rs`）每请求都要解析路由（`pick_upstreams`），没配上游组时还会走 `legacy_pool` 遍历全部产品/账号/密钥。
- 保管箱每次 `flush()` 起一次 PowerShell 做 DPAPI（`secrets.rs:217 → :227 → :161 → :50`）。
- 后台有 4 条独立循环：额度自动取数（默认 30 min）、签到（5 s 轮询 + 5 min 全量）、用量监测（15 s 门限）、网关清理（20 s）。
- Rust 侧目前**没有任何加密 / SQLite / zstd 依赖**（`Cargo.toml`）；SHA-256 是手写的（`wbproxy.rs:133`），只用于算运行时 hash。

### 1.3 测量口径说明

- 「PowerShell 冷启 3~5 s」是**本机当前高负载**（驾驶舱 + 监视器 + 7 个 WorkBuddy 进程 + 其他客户端同时跑）下的数字，不能直接外推。**动手前先在一台空闲机器上复测**，命令见附录 B。
- 所有"每轮耗时"统一口径：进程空闲 5 分钟、关掉其余客户端，取 3 次中位数。

### 1.4 执行前复核（2026-11-18，本轮实测，修正 §1.1 与 P1）

动手前按附录 B 口径重测，**两条原判断需要更正**——记在这里，避免下一轮继续引用错的：

| 项 | 原文判断 | 复核结果 | 证据 |
|---|---|---|---|
| 缓存规模 | 141.4 MB / 10 文件，最大 67.8 MB | **148.3 MB / 10 文件，最大 71.1 MB**（仍在长） | `tools/perf_baseline.py` 实测 |
| 最大文件 loads / dumps | 4.48 s / 3.10 s | **2.71 s / 2.69 s**（read 39.8 ms） | 同上，3 次中位数 |
| P1「`compute` 与 `compute_trend` 各采一次（=两份）」 | 认定读两遍 | **不成立**：`usage.rs:69 cached_source` 有 15 s TTL + 同源串行锁，连续两次调用第二次命中缓存；且 `usage-local.py:580` 的 `changed` 判定确实跳过了未变源的写回（实测三次 collect 写回均为「否」） | 真实源 workbuddy 连续 3 次 collect：1299.7 / 1143.6 / 1188.3 ms，无一次写回 |
| P1 真正的浪费点 | 未识别 | **`_records` 字段**：整包 7.04 MB 里 6.96 MB 是它（**98.9%**）；序列化 213.3 ms → 去掉后 2.5 ms。账号卡片（`compute`）完全不需要逐条记录，只有 dashboard 跨账号去重用 | 同一源实测：19890 条记录 |

**结论**：T1（合并调用）收益有限——TTL 已经挡住了重复读；**T2（`_records` 按需取）才是这一档的主收益**，
且它同时省下 Rust 侧 `serde_json::from_slice` 的解析与 IPC 传输。故本轮**先做 T2，T1 降级为顺手清理**。
一次 collect ≈ 1188 ms 的构成：缓存 loads 416 ms + aggregate 887 ms（含首次重算），二者有重叠计时。

---

## 2. 问题清单

影响档：🔥 每轮/每请求都触发，用户可感知；🟡 周期性触发，中等；🟢 偶发或影响很小。

| # | 位置 | 现象 | 证据 | 档 | 改动成本 |
|---|---|---|---|---|---|
| P1 | `scripts/usage-local.py:531,585`；`usage.rs:356,366` | 缓存每轮整读；**`_records` 占返回包 98.9% 体积**（账号卡片用不到它） | 实测见 §1.4（2026-11-18 复核，**原「各采一次=两份」判断已修正**） | 🔥 | 中 |
| P2 | `ui/app.js:10318`、`app.js:513-595` | 4 s 轮询整页重渲染，无 diff 守卫；侧栏/卡片逐项重建 + 逐项绑监听 | 代码确认 | 🔥 | 中 |
| P3 | `secrets.rs:217→227→161→50`；`registry.rs:1882,1887` | 每次 `flush()` 起一个 PowerShell 做 DPAPI | 实测 3~5 s/次（高负载） | 🔥 | 中 |
| P4 | `proxy.rs:115,116,130,147` | 每请求重解析路由；未配上游组时走 `legacy_pool` 遍历全部产品/账号/密钥 | 代码确认（`config.json` 仅 1.7 KB，**解析不是瓶颈，贵的是 legacy_pool 的 N 次文件读**） | 🔥 | 中 |
| P5 | `proxy.rs:715`（全文件 7 处） | `ureq::request()` 自由函数每次新建 Agent，连接池丢弃，每请求重新 TCP+TLS | 推断（ureq 2.x 行为），**未实测握手占比** | 🔥 | 小 |
| P6 | `main.rs:519-520` → `checkin.rs:1346-1390`；`main.rs:521-524` | 签到 5 s 轮询逐个起 python；5 min 全量 1+N 次 python | 代码确认 | 🟡 | 中 |
| P7 | `local_quota.rs:335`、`checkin.rs:98`、`traeproxy.rs:49`、`doubao.rs:69` | `.output()` **无超时**；agenthub 握手先白等 15 s 才降 TLS1.2 | 代码确认 | 🟡 | 小 |
| P8 | `gateway_cleanup.rs:416-417` | 每 20 s 强制全量进程快照，绕过 `procs.rs:45` 的 10 s 缓存 | 代码确认 | 🟡 | 小 |
| P9 | `state.rs:698` → `identity.rs:56` | 账号档案目录 TTL 30 s，过期即全量 walk 数文件 | 代码确认 | 🟡 | 小 |
| P10 | `ui/usage-dashboard.js:609-641` | 用量页每 4 s 重建全部 SVG 图表，无签名守卫，另叠 30 s/2 s 轮询 | 代码确认 | 🟡 | 小 |
| P11 | `main.rs:532-633`（autofetch） | 17 产品 × N 账号**串行**取数，单线程跑完几十次 spawn | 代码确认 | 🟡 | 大 |
| P12 | `styles.css:1990` | 运行中卡片无限 CSS 动画，每个一个 SVG 持续重绘 | 代码确认 | 🟢 | 小 |
| P13 | `main.rs:209` | 启动 `sleep(8)` 才清理 | 代码确认 | 🟢 | 小 |

---

## 3. Python 运行时现状

### 3.1 构成（`src-tauri/src/usage.rs:15-34`）

| 载荷 | 大小 | 说明 |
|---|---|---|
| `vendor/python-embed.zip` | 12.6 MB | 官方 CPython **3.14.10 embeddable**（x64，带 SHA-256 校验） |
| `vendor/agenthub-libs.zip` | 4.0 MB | `cryptography` + `cffi` + `pycparser`（147 条目） |
| `vendor/agenthub.py` | 585 KB / 11856 行 | 第三方 agent-memory-suite（MIT；HEAD 为 11842 行，当前工作区含未提交改动） |
| 解压后落地 | 34.5 MB | `~/.agent-cockpit/usage-runtime/python-3.14.10-<hash>/` |

脚本正文 `include_str!` 进 exe，运行时经 stdin 注入（`usage.rs:35` 的 `BOOTSTRAP`），**不落盘**；目录名带 4 个载荷的 hash（`usage.rs:27-34`），升级自动换目录。

### 3.2 谁在用、跑什么

| 脚本 | 大小 | 调用点 | 干什么 |
|---|---|---|---|
| `agenthub.py`（vendor） | 585 KB | `agenthub.rs:32`、`local_quota.rs:335`、`checkin.rs:98`、`authpack.rs:694`、`traeproxy.rs:49`、`doubao.rs:69` | 各客户端登录态解密 + 官方接口（quota/checkin/cookie…） |
| `usage-local.py` | 28 KB | `usage.rs:516` | 10+ 产品本地会话日志聚合（**67.8 MB 缓存就是它读的**） |
| `agentfind.py` | 159 KB | `agentfind.rs:187` | 客户端安装发现（长驻，带端口） |
| `chatgpt-history*.py` | 82 KB | `chatgpt_history.rs:26-35` | ChatGPT 历史与同步 |
| `zcode-auth.py` | 6.6 KB | `zcode.rs:36` | ZCode 登录态解密 |
| `trae-gateway-clean.py` | 5.7 KB | `gateway_cleanup.rs:98` | TRAE 网关清理 |
| （桥）`mmproxy` / `qwbridge` | — | `mmproxy.rs:155`、`qwbridge.rs:453,495` | 本地 socks / 桥进程 |

### 3.3 关键事实

- `cryptography` 在本项目里**只用于 AES-GCM / AES-CBC 解密**（`agenthub.py:4587/4639/4849/6426/7702/8140` 等），没有别的用途。
- 每次 python 冷启 0.75~1.2 s；`cryptography` 的 import 约占 0.49 s。
- 只要 `agenthub.py` 还留着，CPython（12.6 MB）就删不掉。

---

## 4. 优化方案

### 阶段一：快赢与编排（1~2 天，零迁移风险）

| 任务 | 对应 | 位置 | 做法 | 验收 |
|---|---|---|---|---|
| **T1** 合并用量采集 | P1 | `main.rs:617,624`、`usage.rs:356,366` | 加 `compute_with_trend(app_id, data_dir)`，一次 `collect()` 同时产出汇总与按天序列 | 单轮采集耗时下降 ≥ 40%；`usage.rs` 单测全绿 |
| **T2** `_records` 按需取 | P1 | `usage-local.py:505-506`、`usage.rs:365-370` | `collect()` 加 `with_records: bool`，默认 `false`；只有 dashboard 跨账号去重传 `true`（`usage_dashboard.rs:270,328`） | 账号卡片读取变快，dashboard 明细仍完整 |
| **T3** 前端签名守卫 | P2 | `ui/app.js:10318-10321`、`app.js:513-528`、`app.js:589` | 后端给 `get_state` 加轻量 `rev`；签名未变就只更新数字、不重建 DOM；`renderDetail()` 收紧重建条件 | 连续 60 s 无操作时 DOM 重建次数为 0 |
| **T4** python 统一超时 | P7 | `checkin.rs:98`、`local_quota.rs:335`、`traeproxy.rs:49`、`doubao.rs:69` | 照 `usage.rs:335-342` 封装 `run_python(args, timeout)` | 故意卡死时循环超时后继续，不堆积 |
| **T5** agenthub 长驻/批量 | P6/P11 | `agenthub.rs:32` | 常驻子进程 + stdin/stdout 分行 JSON，或加 `--batch` 一次处理多账号 | 单轮 autofetch 的 python 启动次数从 N 降到 1 |

**回滚**：均为局部改动，恢复调用即可。

---

### 阶段二：缓存化（2~3 天）

| 任务 | 对应 | 位置 | 做法 | 验收 |
|---|---|---|---|---|
| **T6** 保管箱批量落盘 | P3 | `secrets.rs:217 flush`；调用方 `registry.rs:1882,1887` | 加 `dirty: AtomicBool`，`put/remove` 只置脏，后台每 ≤5 s 统一 `flush()` 一次；进程退出钩子强制 flush | 连续写 10 个账号只产生 1~2 次 PowerShell；单测全绿 |
| **T7** 路由解析缓存 | P4 | `gateway.rs:192,200,228,1092,1117`、`registry.rs:1944`、`proxy.rs:115-147` | `load_config()` 结果按 mtime/大小缓存；**`legacy_pool()` 结果一并缓存**；所有写配置处失效 | 压 1000 次 `pick_upstreams` 耗时下降；配置改动仍即时生效 |
| **T8** 进程快照复用 | P8 | `gateway_cleanup.rs:416-417` → `procs.rs:45` | 改用带 TTL 的 `snapshot()`，仅台账非空且距上次 >10 s 才真扫 | 每 20 s 的 CPU 时间下降；进程数统计不变 |
| **T9** 档案 walk 降频 | P9 | `identity.rs:56`（TTL 30 s）→ `state.rs:698` | TTL 提到 5 min，或改监听目录变化 | 多账号下 `get_state` 耗时不随账号数线性上涨 |
| **T10** 图表重建守卫 | P10 | `ui/usage-dashboard.js:609-641`、`:256-257,351` | 数据签名未变不 `replaceChildren`；页面不可见时跳过 paint | 切走标签页后用量页不再产生渲染 |

**风险**：T6 延迟落盘 → 崩溃可能丢最近写入（缓解：退出钩子 + ≤5 s）；T7 失效漏写 → 改配置不生效（**必须在所有写函数里失效**）。

---

### 阶段三：结构性（1~2 周，需先埋点）

| 任务 | 对应 | 位置 | 做法 | 前置 |
|---|---|---|---|---|
| **T11** 上游连接复用 | P5 | `proxy.rs:715`（及 `:301/:951/:1102/:1263/:1815/:2436`） | 进程级 `OnceLock<ureq::Agent>` 全局复用 | **先实测 TLS 握手占比**，收益不明确就搁置 |
| **T12** 取数编排 | P6/P11 | `local_quota.rs:335`、`main.rs:532-633` | 一次 python 批量处理多账号；autofetch 串行改受控并发（3~4） | 先量清单轮 autofetch 总耗时 |
| **T13** 收尾 | P12/P13 | `styles.css:1990`、`main.rs:209` | 动画加 `prefers-reduced-motion`；`sleep(8)` 改事件驱动 | 无 |

---

### 阶段四：Python → Rust（3~7 周）

#### 4.4.1 分层结论

判断标准：**自研 + 边界清楚 + 有性能证据 → 迁；第三方 + 协议易变 + 无性能问题 → 留。**

| 层 | 迁 Rust？ | 理由 |
|---|---|---|
| 加密原语（AES-GCM/CBC、PBKDF2、SHA-256） | ✅ 做 | Rust 有 `aes-gcm`；顺带干掉 `cryptography` 与 PowerShell |
| SQLite / zstd / HTTP / base64 | ✅ 做 | `rusqlite` / `zstd` / 已有 `ureq` / 小 crate |
| 自研脚本（usage-local、zcode-auth、trae-gateway-clean、chatgpt-history×3） | ✅ 做 | 共约 122 KB，逻辑自有、边界清楚，收益直接 |
| `agentfind.py` | 🟡 评估 | 是长驻服务，逻辑独立；可迁可留 |
| `agenthub.py`（11856 行） | ❌ 不迁 | 第三方协议逆向，含 cookie 格式 / 官方接口 / 活动规则，且跟上游更新；重写＝重新逆向 + 双份维护 |

#### 4.4.2 任务

| 任务 | 做法 | 验收 |
|---|---|---|
| **T14** Rust 实现 AES-GCM / AES-CBC / PBKDF2 | 加 `aes-gcm = "0.5"`（+ `aes`、`pbkdf2`、`hmac`），在 `wbproxy.rs` 加解密函数 | 用真实登录态样本与 python 逐字节一致（golden 测试） |
| **T15** DPAPI 去 PowerShell | 用 `windows` crate 的 `DataProtection`（`ProtectedData::Protect/Unprotect`）替换 `run_ps`（`secrets.rs:50,161`） | 单测全绿；**能解开升级前的旧 `vault.json`**；不再产生 powershell 进程 |
| **T16** `agentcrypt` 替代 `cryptography` | 编译一个 Rust 小工具（或主 exe 隐藏子命令）：stdin 收 JSON、stdout 出明文；`agenthub.py` 里 8 处 `from cryptography…` 改为调它 | `agenthub-libs.zip` 缩到接近 0；冷启 import 时间 −0.5 s |
| **T17** `usage-local.py` → Rust | **不要 1:1 翻译**：借迁移把缓存换成 SQLite（按文件指纹建索引，只重算变化条目）或按产品分片 + 增量写；日志解析用 `serde_json` + `zstd` + `rusqlite` | 67.8 MB 单轮采集从 ~7.7 s 降到 <1 s；抽样 5 个产品数字与迁移前一致 |
| **T18** `zcode-auth` / `trae-gateway-clean` / `chatgpt-history*.py` → Rust | 按 T17 之后的余量推进；chatgpt 三个脚本共享逻辑一起迁 | 各脚本对应单测/手工冒烟通过 |
| **T19** `agentfind.py` 评估 | 先评估再定（长驻服务，可能保留） | 评估报告 |
| **T20** agenthub 瘦身（远期） | 把"读本地登录态"部分也走 Rust；**这是删掉 `python-embed.zip` 的前提** | 视情况 |

#### 4.4.3 依赖替代对照

| 现在（Python） | Rust 替代 | 备注 |
|---|---|---|
| `cryptography` AESGCM | `aes-gcm` | 需自己处理 nonce/aad 切分 |
| `cryptography` Cipher/AES-CBC | `aes` + `cbc` | `agenthub.py:6426` |
| hashlib sha256 | 已有 `wbproxy::sha256`（`wbproxy.rs:133`） | 手写实现 |
| hashlib pbkdf2 | `pbkdf2` + `hmac` | os_crypt v10 |
| PowerShell + DPAPI | `windows` crate DataProtection | 去掉进程 spawn |
| `sqlite3` | `rusqlite` | 登录态 / 用量库 |
| `zstandard` / `compression` | `zstd` | 压缩会话日志 |
| `urllib` / `ssl` | 已有 `ureq` | HTTP |
| `http.server`（agentfind 长驻） | `tiny_http` | 项目已用 |

---

## 5. 验收与回归

### 5.1 每条改动必须附的指标
- 改动前/后各测 3 次取中位数，记录：单次耗时、每轮总耗时、CPU 时间、内存峰值。
- 统一在"空闲基线"下测（附录 B）。

### 5.2 必跑回归
- `cd src-tauri && cargo test`（当前 422 通过）。
- `python tests/agenthub-bundle.test.py`、`python tools/privacy_check.py`（涉及密钥/缓存时）。
- 手工冒烟：打开驾驶舱 → 切每个侧栏页 → 反代开关一次 → 签到刷一次 → 用量页停留 30 s。
- **解密类改动**：必须先用真实登录态样本做 golden 比对，再改代码。

### 5.3 灰度与回滚
- 缓存类改动（T7）先加开关，异常可一键退回实时读。
- 每条改动独立提交，提交信息写清"指标前/后"，便于回滚与复盘。

---

## 5.4 本轮执行结果（2026-11-18）

按「先立验证闸门，再做可测量优化」的顺序执行。**先做闸门**的理由：§5.2 要求每条改动都
`cargo test` 全绿，而当时存在 1 项环境依赖红 + 19 套浏览器测试本机跑不起来，闸门本身不可信。

### 已完成

| 项 | 内容 | 实测验收 |
|---|---|---|
| **闸门 G1** | `registry` 环境依赖红测试改注入 fixture（复用现成接缝 `resolve_profile_in`） | Rust 全量 **396 通过 / 0 失败 / 33 忽略**（此前 392 过 1 红） |
| **闸门 G2** | `workbuddy-quota.test.py` 默认改测 vendored 副本，`AGENTHUB_SOURCE` 可覆盖 | 3/3 通过（此前 FileNotFoundError 整档跑不起来） |
| **闸门 G3** | 新增 `tests/browser-launch.cjs`：`COCKPIT_BROWSER_CHANNEL` 可配 + 自带 chromium 兜底；17 处硬编码统一接入 | 浏览器套件从 **0 套可复跑 → 全部可跑**；另修 4 套既有缺陷（见下） |
| **闸门 G4** | 新增 `tools/perf_baseline.py` + `baseline_perf.json` + `tests/perf-baseline.test.py` | 基准回归 5/5；**变异验证**：把基准改快 1000× → 当场红；口径改 min → 当场红 |
| **T2** | `_records` 按需取：`collect(with_records)` / `collect_with_records()`，缓存键含该标志 | 体积 **7.04 MB → 0.08 MB（−98.9%）**，端到端 **1979.5 → 1505.5 ms（−23.9%）**；汇总字段逐字段一致；默认仍全包（旧契约未破） |
| **T4** | `agenthub.rs` 新增共享 `run_with_timeout`（边跑边抽 stdout/stderr、超时 kill 收尸），接入 checkin / local_quota / traeproxy / doubao 四处裸 `.output()` | 新增 3 条单测全绿：**故意卡死 30s 的任务 1s 超时返回**、2 MB 输出不死锁、非 0 退出码保留 stderr |
| **T3** | `load()` 轮询签名守卫（整份 state + view + cur），交互调 `render()` 即失效 | 新增 `poll-guard.test.cjs` 5 条断言全绿；**双向变异**：永远跳过 → 红；守卫失效 → 红 |
| **T10** | 用量看板 `paint()` 签名守卫（覆盖整份 state，剔除瞬态字段） | 新增 `usage-paint-guard.test.cjs` 4 条断言全绿；变异（守卫失效）→ 红 |
| **T8（收窄版）** | `retry_pending` 同一轮 sweep 复用一份 `snapshot_fresh()`，不再每项扫一遍进程表 | 保留 fresh 语义（不放宽「客户端在跑就不改配置」这条正确性护栏） |
| **T13（部分）** | 启动清理 `sleep(8)` 改有界轮询：网关一旦起来立刻继续，上限仍 8s | 反代本就不自动恢复，原固定等待多数时候在等不会发生的事件 |
| **T11**（2026-11-19，用户授权一次隔离验证） | `proxy.rs` 进程级 `OnceLock<ureq::Agent>`（每 host 空闲上限 1→4），6 处上游/回环调用点接入；其余配置与自由函数默认逐项一致，每请求 timeout 语义不变 | 前置埋点：无凭据探针实测握手占冷连接 **34~42%**（openai 438ms/41.6%、anthropic 383ms/34.2%，`t11_tls_result*.json`）→ 收益明确不搁置。新增 2 条单测绿（单例 + 本地计数服务 3 请求 1 连接）；**双向变异均红**（改回每次新建 Agent：单例红、连接数 3≠1 红）。全量 **431 项 = 398 过 / 0 失败 / 33 忽略**（主 harness 428 = 395 过 + 33 忽略，36.8s；第二二进制 3/3；另 4 次子进程隔离各 1 项系既有设计）。privacy_check proxy.rs 0 findings。隔离 CARGO_TARGET_DIR，未部署未提交 |
| **T6**（2026-11-19，用户授权继续推进） | `secrets.rs` put/remove 只置脏（DIRTY）；后台线程 ≤1s 巡检、距上次写 ≥5s 才 flush（连续写合并）；`lifecycle::exit` 退出钩子强制收口；`registry.rs save_instance_checked` 两处逐条 flush 移除（批量保存不再逐条起 PowerShell）；set_password/remove_password/load_map 同步清脏防重复加密；关键路径（authpack/antigravity 等）仍直接 flush 语义不变 | 新增 1 条机制单测绿（置脏/锁定拒绝不置脏/flush 进入即清脏，三段串行防全局态竞争）；**变异红**（删 mark_dirty 即红）。崩溃丢写窗口 ≤5s + 退出钩子兜底。全量 **433 项 = 400 过 / 0 失败 / 33 忽略**（含 T9 新增）。privacy 改动 4 文件 0 findings。未部署未提交 |
| **T9**（2026-11-19，用户授权继续推进） | `identity.rs inspect_cached` TTL 30s→300s；前提已核实：缓存仅供 state.rs 展示路径，换号判空（swap.rs:441）与 commands.rs:3484 走 inspect() 本体不受影响 | 新增 1 条单测绿（首查后改名目录，TTL 内仍命中缓存；不经缓存的 inspect 立即见目录消失作对照）；**变异红**（TTL=0 即红）。多账号 get_state 的档案 walk 由每 30s 一轮降为每 5min 一轮。未部署未提交 |
| **T7（完整）**（2026-11-19，用户授权继续推进） | ① `registry.rs load_config` mtime+大小缓存（71 调用点热路径）+ save_config 双路径失效；② `proxy.rs legacy_pool` 结果缓存：键=(global_upstream, config mtime+大小) + **2s TTL 兜底** + 4 收口写点显式失效（save_instance_checked / keys::save_sites / secrets::put+remove / commands 删账号）；未加新依赖 | 新增 3 条单测绿（config 键纯函数 + config 子进程隔离 + legacy_pool 子进程隔离：同键命中/写点立即失效/config 键失效/TTL 兜底）；**变异均红**（忽略 mtime 红、禁用缓存命中红）。全量 **436 项 = 403 过 / 0 失败 / 33 忽略**。privacy 5 文件 0 findings。多源陈旧风险用「短 TTL≤2s + 显式失效」双保险控制，2s 远小于前端 4s 轮询。未部署未提交 |
| **T12**（2026-11-19，用户授权 T5/T12 编排批次） | `main.rs autofetch_once` local_quota 分支：产品内账号取数改**受控并发**（分批 spawn，每批≤3，join 完再下一批，零新依赖）；抽出 `run_local_quota_batch`（可注入 mock 单测）+ `autofetch_local_account`（保留 web 明细回退/防串号/usage_only 整产品停护栏） | 真实基线：串行单轮中位 **5133ms** → 3 并发中位 **2582ms，约 2× 加速**（`t12-speedup-real.json`，只读不写不签到）。新增 4 条单测绿（未绑定跳过/usage_only 停后续/并发上限≤3/全绑定各一次）；**变异红**（chunks 改 9 → 峰值 9>3 红）。全量 **440 项 = 407 过 / 0 失败 / 33 忽略**。privacy main.rs 0 findings。未部署未提交 |
| **T5（评估结论，已被后续推翻）**（2026-11-19 上午） | agenthub 长驻/批量可行性评估 | 当时判断：`--profile` 单目录无法按账号区分、长驻模式不存在，实施须改 vendored 触红线，建议搁置。**此结论已被同日「T5 地基/集成」推翻**：实测 agenthub 可干净导入且 quota 走 con=None，用外部 wrapper 循环调 cmd_quota 即可一次冷启多账号，vendored 字节不变、不触红线。保留本行作决策轨迹，以「T5 集成」行为准 |
| **T1**（2026-11-19，用户授权恢复，decision_d5ae3d19523e4e42832dc150e5400cb8） | `usage.rs` 抽出 `summary_from(&Value)`/`trend_from(Value)` 纯函数 + `compute_pair` 单采集派生两视图；`compute`/`compute_trend` 改薄包装输出逐字节不变；`main.rs` 后台 codex/claude 分支双调用改一次 `compute_pair` 解构，usage_fetched_at/changed 语义逐点保持 | **collect 进入次数 2→1**（compute_pair 结构上单次 collect，省第二次缓存克隆）。新增 4 条单测绿（新旧视图逐字段相等/独立判空/_records 不泄漏/所有权独立）；**变异红**（删 remove(_records) 即红）。全量 **444 项 = 411 过 / 0 失败 / 33 忽略**。privacy usage/main 0 findings。收益毫秒级顺手清理（不承诺 ≥40%，前提已被 §1.4 推翻）。commands.rs 手动刷新路径保持原 API。未部署未提交 |
| **T14 地基**（2026-11-19，用户授权推进阶段四） | 新增 `agentcrypt.rs` 独立模块：AES-256-GCM 解密 / os_crypt v10 整包 / AES-256-CBC(PKCS7) / PBKDF2-HMAC-SHA512；加 aes-gcm/aes/cbc/cipher/pbkdf2/hmac/sha2 依赖。**纯增量，不接任何生产加解密路径**（secrets/wbproxy 生产逻辑零改动） | **跨语言 golden 测试通过**：python cryptography 加密合成向量 → Rust 解密/派生逐字节一致（`tests/agentcrypt-golden.test.py`，含 150000 轮 PBKDF2）；5 条单测绿；**变异红**（os_crypt nonce 切分改坏即红）。全量 **449 项 = 416 过 / 0 失败 / 33 忽略**。privacy 新文件 0 findings。为 T15/T16 迁移铺路；切换生产属数据丢失级，需真实样本+单独授权。未部署未提交 |
| **T5 地基**（2026-11-19，用户授权推进 T5） | 新增 `scripts/agenthub-batch.py` 批量 wrapper：一次进程冷启导入 agenthub 循环调 cmd_quota 处理多账号，**vendored agenthub.py 字节不变**（编排优化，非重写） | 可行性实测：agenthub 可干净导入（~536ms 无副作用）、quota 走 con=None 不用库；3 账号批量 **2306ms**（对比串行 5133ms / 3 并发 2582ms）。与 T12 并发互斥，集成到 autofetch 需单独批次，本轮不替换已验证的 T12。未部署未提交 |
| **T5 集成**（2026-11-19，用户「全力推进」） | `local_quota.rs` 抽出 `parse_quota_output` 纯解析 + 新增 `fetch_batch_raw`（一次冷启批量取多账号原始 JSON）；`main.rs autofetch` 加 `prefetch_local_quotas` 预取缓存，标准直读账号走批量、逐账号复用同一解析+校验；zcode/workbuddy/trae 特殊路径与批量失败回退逐账号（T12 并发仍在） | 护栏全保留（绑定/防串号/usage_only/web 明细/zcode 登录变化守卫）；新增 3 条单测绿（解析字段/批量往返一致/错误如实报）；批量失败回退有测试。真实批量中位 3322ms（vs 串行 1.54×；与并发互有胜负，受最慢账号限制）。全量 **455 项 = 422 过 / 0 失败 / 33 忽略**。privacy 0 findings。vendored 字节不变。未部署未提交 |
| **T15 地基**（2026-11-19，用户「全力推进」） | 新增 `dpapi_native.rs`：原生 CryptProtectData/CryptUnprotectData（CurrentUser + UI_FORBIDDEN，与 secrets.rs PowerShell 通道同口径），windows crate 加 Win32_Security_Cryptography feature。**纯增量，不接生产保管箱** | **跨实现 golden 通过**：原生 DPAPI 加密 → PowerShell DPAPI 解密逐字节一致（`tests/dpapi-native-golden.test.py`），证明迁原生后旧密文仍可解；3 条单测绿（回环/entropy 敏感/golden）；**变异红**（忽略 entropy 即红）。全量 455 项 0 失败。privacy 0 findings。切生产 vault 属数据丢失级，需真实样本+单独授权。未部署未提交 |
| **T19 评估**（2026-11-19，并行子代理） | agentfind.py 迁移评估（只读，报告 `t19-agentfind-evaluation.md`） | **建议「留」**：唯一带端口+后台刷新+自证探活+进程生命周期的长驻服务（3217 行，728 行内嵌前端），体积收益≈0（运行时非独占）、冷启瓶颈在扫描+SQLite 非解释器；协议/指纹/平台补丁三层事故驱动耦合，重写回归面大。可选部分迁：728 行 PAGE 挪静态资源、cmd_stop 去 PowerShell。关键数字经 wc -l/grep 复核 |
| **基准缺口修复 + T5/T12 取舍复测**（2026-11-19，用户「授权改进」） | ① `tools/perf_baseline.py` 的 usage_cache.dir 改为相对描述 `<数据根>/usage-cache`（原绝对路径含家目录被 privacy 拦、致 baseline_perf.json 无法入库而测试硬依赖它）；重新生成基准入库。② 空闲口径三路线各 3 轮取中位复测 | ① 新基准 privacy 0 findings；perf-baseline.test.py **5/5 绿**；缺文件时 4 红 1 跳过（提示先跑生成工具，不假装通过）；**变异红**（基准改快 1000× → 回归闸门红），还原后绿。② 复测中位：串行 4299.7ms / **并发3 2516.0ms** / 批量 2918.0ms（`t5-t12-tradeoff-retest.json`）——**并发略优于批量**（批量单进程内 cmd_quota 仍串行，只省冷启；并发把最慢账号外的等待也压掉）。**结论：保留现架构**（批量预取优先+失败回退并发）不动：批量对多账号（N>3）收益随 N 增长，3 账号场景并发占优但差距 ~400ms 在负载波动内；双路线互为回退比单路线稳 |
| **T17 第一阶段**（2026-11-19，用户「授权开工」） | codex/chatgpt 解析迁 Rust：新增 `usage_local.rs`（codex 累计终值差分/重复帧跳过/last 兜底 + aggregate 去重 + **SQLite 按文件指纹增量缓存**，rusqlite bundled）；`usage.rs collect_inner` 接线 codex→Rust、失败回退 python；其余产品仍 python 后续批次。**不做 1:1 翻译**（计划红线）：整包 json 缓存→SQLite 只重算变化文件 | **跨语言 golden 双重验证**：合成 fixture（差分/重复帧/last兜底/跨时区/坏行）+ 真实 ~/.codex（3 文件/total=6442910）python↔Rust 逐字段与事件 id 集合一致；4 条单测绿（含缓存复用/prune/指纹重算）；**变异红**（累计回退分支改坏即红）。全量 **459 项 = 426 过 / 0 失败 / 33 忽略**。真实 1.3MB：Rust warm 42.6ms vs python 74.9ms（另免 ~1s python 冷启）；67.8MB 大缓存场景待真实数据补测。privacy 0 findings。identity 口径两处漂移已修（timestamp %s 原样 / f64 比对）。未部署 |
| **T17 第二阶段**（2026-11-20，用户「继续推进」） | messages 系 12 产品解析迁 Rust（claude/qoder/qwenwork/workbuddy/codebuddy/minimax 各系）：`message_file`（session:messageId 分组/fragment 去重/user 输入估算累计/usage 取组内最大帧/claude·minimax·standard 三 style）+ `collect_messages` 共享 SQLite 增量缓存（parser_version=rust-messages/1）；`usage.rs` 接线 messages→Rust 失败回退 python。**本会话另修 4 处口径漂移**：aggregate 浮点累加改首现序（HashMap 随机序末位漂移）/ py_lines universal newlines（lone \r 切行）/ index 兜底只数解析成功行 / BOM 只剥文件头一次 | **跨语言 golden**：合成 4/4（codex/claude/workbuddy/minimax）+ **真实 4 目录全量逐事件一致**（claude 17 文件/qoder-cn **841 文件 2.66GB**/qwenworkcn 349/workbuddy 58，with_records=True）。5 条新护栏单测 + **4 条变异全红**还原零残留。全量 **467 项 = 434 过 / 0 失败 / 33 忽略**（459→467 +8）。性能真实：claude 172MB warm **1.24s vs python 11.6s（9.4×）**、workbuddy 362MB warm 2.26s（5.2×）、qwenworkcn 126MB warm 0.56s（16.2×），totals 全一致；首采≈python（解析主导），收益在 warm 增量+免 python 冷启。qwenworkcn 首轮一次不可复现失败已 4/4 复跑排除+顺序隐患修复。privacy 0 findings。临时探针已删（grep=0）。**已提交 73f7a53** |
| **T17 第三阶段**（2026-11-20，用户「推进 T17 第三阶段」） | DB 系/dsh/gemini 解析迁 Rust：`zcode_db`（model_usage 请求库，provider/computed total 兜底+failure+计时）/`opencode_db`（新表 session_message 覆盖旧表同 id，opencode style reasoning 单列）/`marvis_db`（llm_token_usage 逐行）/`astudio_db`（拆 report_json.model_usages 子 turn）/`dsh_file`（zstd 解压+assistant/message 过滤）/`gemini_file`（整包 JSON messages+user 累计估算）；`collect_other` 统一入口+`is_other_app`；`usage.rs` 接线→Rust 失败回退 python。EventOpts 加 duration/ttft，event() 补计时字段。Cargo.toml 加 zstd 0.13 | **真实 zcode 1.23GB/27197 行/total=7507353979 + marvis 76 行 python↔Rust 逐事件一致**；4 条合成单测（zcode total 兜底/opencode 新表覆盖/dsh zstd/gemini 估算）；**4 条变异红**还原零残留。全量 **472 项 = 439 过 / 0 失败 / 33 忽略**（467→472 +5）。dsh 真实 173 文件全为 session 类型无 usage（python 亦 None），合成 fixture 验证口径。privacy 0 findings。**已提交 399d7af**。至此 usage-local.py 全部产品（codex/messages/DB/dsh/gemini）Rust 解析完成，python 仅作回退保底 |
| **编译部署**（2026-11-20，用户「先编译部署」） | `private_build.py build --release` 路径脱敏 release 构建（18m17s，316 crates）→ 二进制 privacy 0 findings → `pre_deploy.ps1` 两条硬检查通过（无多实例、8788 无监听不杀自托管模型）→ `deploy.ps1` 部署 | **deployed bin/AgentCockpit.exe（pid 55656）**，lifecycle.log 确认 start；codex-shared-resume.exe 同步到 bin/ 与 ~/.agent-cockpit/bin/。8788 反代按设计不自动启动（用户显式点开）。license-report 测试 2 ERROR 系沙箱 retention 拦截 tempfile 清理（CHAT_REFERENCE_PROTECTED），非代码回归 |
| **T18 第一批**（2026-11-20，用户「继续推进」） | `trae-gateway-clean.py` 迁 Rust：新增 `trae_clean.rs`（clean_db：ItemTable model_list_map 网关条目识别+删除+选择记录剪枝，is_gateway/has_token 整标识符边界/prune_selection）；`gateway_cleanup.rs run_script` 接线 Rust 优先失败回退 python | **跨语言 golden**：合成 state.vscdb python run vs Rust clean_db 输出+写回后库内容逐字段一致（removed=3 keys=3）。6 条单测绿；**2 条变异红**（has_token 去 ./ 边界/is_gateway 恒假）还原零残留。全量 **478 项 = 445 过 / 0 失败 / 33 忽略**（472→478 +6）。privacy 0 findings。**已提交 c49df32** |
| **T18 第二批**（2026-11-20，用户「继续推进」） | `zcode-auth.py` 迁 Rust：新增 `zcode_auth.rs`（identity/snapshot/proxy_credential/merge/run，enc:v1: AES-GCM 解密用 T14 agentcrypt）；`zcode.rs helper` 接线 Rust 优先失败回退 python | **跨语言加密 golden**：python cryptography 加密合成 record → Rust 解密，identity/snapshot/merge/merge_logout 四模式输出逐字段一致。9 条单测绿（含加密 round-trip）；**2 条变异红**（merge 去前缀过滤/identity 去 access 判空）还原零残留。全量 **487 项 = 454 过 / 0 失败 / 33 忽略**（478→487 +9）。privacy 0 findings。**已提交 dcf0e55** |
| **T18 第三批**（2026-11-20，用户「继续推进 T18 第三批」） | `chatgpt-history.py` 的**项目修复路径**迁 Rust：新增 `chatgpt_history_repair.rs`（project_repair_plan/repair_projects，ordinary 路径校验/key normcase/project_visibility_state/database_paths/primary_state_db/project_rows/计划识别/apply 备份+事务删除+失败回滚）；`chatgpt_history.rs run_script` 接线 repair_projects mode Rust 优先失败回退 python。**范围红线**：copy_history/shared-history/sync 涉及目录 junction/整库复制/WAL 退役/凭据隔离，属数据丢失级，**仍走 python 不触碰**。rusqlite 加 backup feature | **跨语言 golden**：合成 source/target（state_5.sqlite+state JSON+marker）python plan/apply vs Rust 逐字段一致 + apply 后 state/db 内容比对。12 条单测绿（dry_run/apply 备份/expected_plan 校验/所有权/重叠/has_thread 守卫/key/visibility）；**3 条变异红**（时间窗收紧/所有权反转/has_thread 禁用）还原零残留。全量 **499 项 = 466 过 / 0 失败 / 33 忽略**（487→499 +12）。privacy 0 findings。**已提交 eb285ae** |
| **T19 部分迁①**（2026-11-20，用户「暂不部署，先完成项目任务推进」） | `agentfind.py cmd_stop` 去 PowerShell：`stop_legacy` 优先 Rust 原生（sysinfo 枚举命令行匹配 python+agentfind.py 排除自身 + `procs::kill_pids` 整树结束），免起 python --stop 子进程再调 PowerShell+taskkill；失败回退原 python --stop。`is_agentfind_service` 谓词抽独立函数单测钉住口径 | 与 python cmd_stop 同口径（按命令行匹配全部服务进程，不只端口上那一个）。新增 1 单测；**2 条变异红**（去 agentfind.py 过滤/去自身排除）还原零残留。全量 **500 项 = 467 过 / 0 失败 / 33 忽略**（499→500 +1）。privacy 0 findings。**已提交 6f3d232** |
| **T19 部分迁②**（2026-11-20，用户「暂不部署，先完成项目任务推进」） | `agentfind.py` 728 行内嵌前端 PAGE 挪到独立静态资源 `agentfind-page.html`（纯搬运字节不变，46819 字节）；python `_load_page()` 同目录读取，缺失给最小错误页不静默白屏；`agentfind.rs` include_str! 编进 PAGE_HTML，script_file_at 同目录释放+字节复用+损坏修复。**脚本指纹不再含 PAGE**——前端改动不再触发服务重启 | python 验证 PAGE==html 字节一致 + 缺失回退错误页；新增 1 单测 + 扩展释放修复测试。全量 **501 项 = 468 过 / 0 失败 / 33 忽略**（500→501 +1）。privacy 0 findings。agentfind.py 3217→2502 行（−715）。**已提交 f3ead67** |

**顺带修掉的 4 套既有测试缺陷**（都不是本轮引入，在 Chrome 上同样会红）：
- `add-app-picker` / `external-checkin` / `checkin-pending-day` / `workbuddy-accounts`：未屏蔽新手引导，
  `.tourmask` 600ms 后拦截点击 —— 此前靠「抢在引导出现前点完」的时序运气偶尔通过。
- `gateway-capabilities`：详情面板是 `<dialog>`，由 `document.body.append` 挂到 body，
  测试却在 `#suppliers` 子树里找「设置模型」→ 永远超时；另有 modal 未关就操作 `#pricing`。
- `perk-layout`：`waitUntil:'load'` 不等于 mock 数据就位，`S.data` 为 null 就 seed。
- `reminders-design`：「贴边提示条」默认关闭时 tab 不赋 `data-edge`，测试未开启该开关；
  另有 `waitForTimeout(260)` 赌 240ms 动画（余量仅 20ms）。

### 判断修正（原文过时，勿再引用）

| 项 | 原文 | 复核 |
|---|---|---|
| P1「各采一次=两份」 | 认定读两遍 | **不成立**，见 §1.4：15s TTL 已挡住；真正浪费是 `_records` |
| P12 CSS 无限动画 | 待加 `prefers-reduced-motion` | **已实现**：`styles.css:1662` 有全局 `* { animation: none !important }`，1996 行另有针对性覆盖。本轮未做重复改动 |
| P8「每 20s 强制全量快照」 | 建议改 TTL 缓存 | **收窄执行**：那些 `snapshot_fresh()` 是「改配置前确认客户端没在跑」的正确性护栏，改 TTL 会误判。只消除同一轮内的重复扫描 |

### 未执行（明确留待后续，不在本轮声称完成）

- **阶段四 T16/T20**（agentcrypt 接生产 / agenthub 瘦身）：**数据丢失级风险**，须真实登录态样本 + 单独授权。
  T14 加密原语 + T15 原生 DPAPI 地基已建（见上表），但**未接任何生产加解密路径**；PowerShell 与 Python 冷启开销本轮**不会消失**。
  T16 需改 vendored agenthub.py（触「不整体重写 agenthub」红线）。
  （T1/T6/T9/T11/T7/T12/T14地基/T15地基/T5集成/T19评估 已于 2026-11-19 完成；**T17 第一/二/三阶段**已于 11-19/11-20 完成并提交，
  usage-local.py 全部产品 Rust 解析完成，python 仅作回退保底。**T18 第一批 trae-gateway-clean / 第二批 zcode-auth / 第三批 chatgpt-history 项目修复** 已于 11-20 完成并提交。
  **T19 部分迁①cmd_stop 去 PowerShell / ②728 行 PAGE 挪静态资源** 已于 11-20 完成并提交（长驻服务主体按评估「留」不迁）。
  **T18 剩余 copy_history/shared-history/sync 三条路径暂缓**：涉及目录 junction/整库复制/SQLite WAL 退役/凭据隔离，属数据丢失级，
  须真实登录态 golden 样本 + 单独授权，不在本轮擅自迁移。）

---

## 6. 排期总表

| 阶段 | 内容 | 预估 | 前置 |
|---|---|---|---|
| 一 | T1、T2、T3、T4、T5 | 1~2 天 | 无 |
| 二 | T6、T7、T8、T9、T10 | 2~3 天 | 阶段一的埋点 |
| 三 | T11、T12、T13 | 1~2 周 | 先完成耗时埋点 |
| 四 | T14→T17（加密 + usage-local） | 1~2 周 | 阶段一基线 |
| 四 | T18（其余自研脚本） | 3~4 天 | T14~T16 |
| 四 | T19、T20 | 视情况 | T14~T18 |

**建议起手**：阶段一 + T14/T15（加密原语 + 去 PowerShell），合计 3~5 天，同时拿到"冷启编排优化"和"PowerShell 消失"两份收益，风险可控、可独立回滚。

---

## 7. 风险与红线

1. **解密一致性**：所有 AES/PBKDF2 改动必须先建 golden 测试（真实登录态样本比对 python 与 Rust 输出），不一致不能合。
2. **DPAPI 兼容**：熵（entropy）与 scope 必须与现状一致，否则**旧保管箱解不开——数据丢失级风险**。升级前先备份 `%USERPROFILE%\.agent-cockpit\vault.json`。
3. **隐私**：迁移不得把明文登录态写进日志或明文缓存；`tools/privacy_check.py` 必须继续 0 findings。
4. **契约不变**：agenthub 的 stdout 协议、secrets 落盘格式、反代的上游选择语义保持兼容。
5. **端口占用**：`dev_ui` 之类的调试兜底不得进 release。
6. **不做无测量的优化**：T11、T12 必须先埋点，收益不明确就搁置。

---

## 8. 约束与不做的事

- **不牺牲隐私**：不得为提速把密钥/登录态写进日志或明文缓存。
- **不破坏现有契约**：见第 7 节第 3、4 条。
- **不整体重写 agenthub**：第三方协议层以编排优化为主（T5），不重写。
- **dev / release 分开**：调试兜底只准存在于 dev 构建。

---

## 附录 A：审查方式

- 三路并行审查：Rust 热路径、前端渲染与轮询、Python 侧与进程开销；报告汇总后**逐条回读代码复核**，关键数字在本机复测。
- 本文件中的行号基于 2026-11-18 的工作区状态。

## 附录 B：测量命令（可复现）

```powershell
# 1) 用量缓存单文件读/解析/序列化耗时（内置 python）
$py = (Get-ChildItem "$env:USERPROFILE\.agent-cockpit\usage-runtime\python-3.14.10-*\python.exe").FullName
$cache = Get-ChildItem "$env:USERPROFILE\.agent-cockpit\usage-cache\*.json" |
         Sort-Object Length -Descending | Select-Object -First 1
@'
import json,sys,time
p=sys.argv[1]
t=time.perf_counter(); raw=open(p,'rb').read(); t1=time.perf_counter()
obj=json.loads(raw); t2=time.perf_counter()
s=json.dumps(obj); t3=time.perf_counter()
print(f"read={t1-t:.3f}s loads={t2-t1:.3f}s dumps={t3-t2:.3f}s records={len(obj.get('files',[]))}")
'@ | & $py - $cache.FullName

# 2) PowerShell 冷启（保管箱 flush 的成本）
1..5 | ForEach-Object {
  $sw=[Diagnostics.Stopwatch]::StartNew()
  (Start-Process powershell -ArgumentList '-NoProfile','-NonInteractive','-Command','exit' -PassThru).WaitForExit()
  $sw.Stop(); "{0:N0} ms" -f $sw.ElapsedMilliseconds
}

# 3) agenthub 单次冷启
$rt = (Get-ChildItem "$env:USERPROFILE\.agent-cockpit\usage-runtime\python-3.14.10-*\python.exe").FullName
1..3 | ForEach-Object { $sw=[Diagnostics.Stopwatch]::StartNew(); & $rt -I -B -X utf8 $rt.Replace('python.exe','agenthub.py') --help > $null; $sw.Stop(); "{0:N0} ms" -f $sw.ElapsedMilliseconds }

# 4) 内置运行时体积
"{0:N1} MB" -f ((Get-ChildItem "$env:USERPROFILE\.agent-cockpit\usage-runtime\python-3.14.10-*\*" -Recurse -File | Measure-Object Length -Sum).Sum/1MB)

# 5) 空闲基线：先关掉其余客户端，静置 5 分钟再测；每项测 3 次取中位数
```

## 附录 C：Rust 化验证命令

```powershell
# 解密一致性：同一份加密 blob，python 与 Rust 两侧解出的明文应逐字节相同
# DPAPI 兼容：升级前备份 vault.json，升级后应能原样解开
Copy-Item "$env:USERPROFILE\.agent-cockpit\vault.json" "$env:USERPROFILE\.agent-cockpit\backups\vault.pre-dpapi.json" -Force

# 回归
cd D:\Codebase\agent-cockpit\src-tauri; cargo test
python D:\Codebase\agent-cockpit\tests\agenthub-bundle.test.py
python D:\Codebase\agent-cockpit\tools\privacy_check.py
```
