# agent-cockpit（Agent 驾驶舱）v2.5.1 综合评估报告
## —— 九维度绝对体检（模式 A，无历史评分基线）

> 评估口径：`software-health-review` skill（九维等权均值 − 验证盲区折让，折让为 0 / −0.20 二值开关）
> 取证方式：skill `measure.py` 实盘 + 针对 Rust/Node 栈的人工补充取证 + 测试真实复跑（数字一律实测，禁止估算）
> 评估时点：**2026-11-18**；代码状态：HEAD `e49201c`（main）+ 7 个未提交改动文件 + 未跟踪 `dev_ui.rs`（评估不改任何源码）
> 配套留档：`取证快照.md`（原始取证）、`score_input.json`（打分入参，下次复评从它改起）、`评分表.md`（score.py 生成）；复跑日志与结构化数字在 `D:\Codebase\agent-cockpit-health-review\`

---

## 一、结论先行

**一句话**：这是一个工程纪律明显高于平均水平的个人维护桌面工具——并发锁策略、降级契约、隐私发布链都成体系且有事故复盘沉淀；综合分 **7.7**（九维均值 7.89 − 盲区折让 0.20），最大隐忧是「验证面看着很宽，真能在这台机器上复跑绿的只有一半」，以及 `ui/app.js` 10376 行巨文件对架构维度的拖累。

| 项 | 结论 |
|---|---|
| 综合分 | **7.7**（首次体检，无基线可比；均值 7.89，折让 −0.20） |
| 最强项 | 稳定性 / 可读性 / 并发（各 8.5）：降级契约入测试、注释带事故日期与红线、锁策略单份成文 |
| 最大隐忧 | 测试与打包 7.5：1 项环境依赖红 + 19 套浏览器测试本机跑不了 + 真机 e2e 未复跑 + 无 CI |
| 最该做的一件事 | 把测试套件变成「任何开发机一条命令可复跑全绿」（详见第七节） |

---

## 二、项目基线盘点（模式 A 的「基线」= 现状事实）

- **形态**：Tauri 2 桌面应用（Rust 后端 + WebView2 原生 JS 前端，无框架无打包器），Windows x64，MIT。
- **版本**：package.json / Cargo.toml / CHANGELOG 三处一致 = **2.5.1**（2026-11-11 发布）；git 最近提交均为 2.5.1 修复批次。
- **历史评分**：仓库内无既往评估报告 / 评分表 → 走模式 A；本报告与 `score_input.json` 构成下次模式 B 复评的基线。
- **协作约束**：AGENTS.md / DEVELOPMENT.md 要求编译测试走 `agsync build`。实测本机 **`.agent-sync/` 不存在、agsync 不在 PATH、`~/.agents/skills/agent-sync/bin/agsync.py` 不存在**——协调机制的前置条件不成立；且检测到另一会话的 `cargo run`（PID 23440，当日 15:06 起）正占用共享 target。故本次验证采用**隔离 `CARGO_TARGET_DIR` 的只读复跑**（不触碰共享缓存、不部署、不改源码），偏离协调入口的原因与方式在此明示。
- **未提交 WIP**：registry.rs / main.rs / local_quota.rs / vendor/agenthub.py / docs 等（新增豆包工作、CodeArts 登记与 dev_ui 调试入口）。评估按工作区现状取证；唯一红项已核实与该 WIP 无关（见第五节短板 #1）。
- **评估期间检测到并行会话活动**：另一会话的 `cargo run` 占用共享 target；评估中途新增 `docs/PERFORMANCE_PLAN.md` 与 `docs/RUST_MIGRATION_PLAN.md`（2026-11-18 产出的性能审查与 Rust 化计划，含实测数据：67.8 MB 用量缓存整读整写、json.loads 4.48 s 等）。**计划文档不等于基准回归机制**，本报告「无性能基准机制」的结论不变；但第七节第三步可直接承接该计划的「先测量、后优化」原则。另注：该计划称 agenthub.py 为 10,793 行，与本评估实测 11,856 行（HEAD 11,842）不一致，口径待对齐——本报告数字以实测为准。

---

## 三、量化取证（全部实测，2026-11-18）

### 3.1 规模与分区

skill `measure.py` 原始口径：181 文件 / 99,834 行（含 vendored 与归档，**不能直接当自有规模**）。人工分区修正后：

| 分区 | 文件数 | 行数 | 说明 |
|---|---:|---:|---|
| Rust 后端（src-tauri/src + build.rs） | 70 | 51,850 | 其中生产代码 37,499 行、内嵌测试 14,351 行 |
| 前端 ui/（js/css/html） | 28 | 19,326 | app.js 独占 10,376 行 |
| 内置脚本 src-tauri/scripts/ | 11 | 5,998 | agentfind.py 3,217 行等，随程序分发（含 1 个 67 行测试脚本） |
| 工具 tools/ | 9 | 2,401 | 含第三方 l10n 引擎 1,984 行 |
| 根目录脚本 | 4 | 210 | deploy/pre_deploy.ps1、启动 bat、_sort_smoke.cjs |
| **核心合计** | **122** | **79,785** | |
| 减：第三方 l10n（引擎+10 个词典） | −11 | −6,026 | NOTICE 声明未修改的上游代码 |
| **自有代码（除 l10n）** | **111** | **73,759** | 架构/可读性维度的真实分母 |
| 测试 tests/ | 53 | 5,921 | 28 cjs + 13 py + 10 ps1 + 1 js + 1 mjs |
| vendored（agenthub.py + console.html） | 2 | 12,323 | 来自 agent-memory-suite，**带本地改动**（WIP +21/−7） |
| 归档 legacy-python/ | 2 | 1,297 | 旧 Python 版，不计入当前架构 |
| docs 原型（js/css/html） | 3 | 264 | 设计原型，非产品代码 |

- 测试/自有源码行比：**8.0%**（5,921 / 73,759）；若把 Rust 内嵌测试算进测试面，测试代码合计 20,272 行。
- 自有最大文件 TOP5：`ui/app.js` 10,376 ｜ `commands.rs` 3,798 ｜ `wbproxy.rs` 3,232 ｜ `scripts/agentfind.py` 3,217 ｜ `registry.rs` 2,810。
- **>500 行自有文件 46 个**；内置登记表 curated 条目 37 个。
- measure.py 的两处尺子盲区已人工修正：嵌套 `src-tauri/vendor/` 未被自动排除（11,856 行假巨文件）；`codexcatalog.rs` 因词尾 "log" 被误判为日志模块。

### 3.2 依赖面

| 项 | 实测 |
|---|---|
| Cargo.lock | **510 个包**精确锁定；toml `=0.8.2`、toml_edit `=0.20.2` 精确钉版（注释记录 schemars/indexmap 构建修复缘由） |
| package-lock.json | v3，15 个包条目，带 integrity；devDeps 仅 3 个（tauri CLI ×2 + playwright） |
| 内置运行时 | python-embed.zip 12.6 MB（CPython 3.14.10 官方原版，SHA-256 登记）+ agenthub-libs.zip 4.0 MB（cryptography 49.0.0 / cffi 2.1.0 / pycparser 3.0）；解压逐文件哈希校验、fail-closed、zip-slip 防护 |
| 许可治理 | license_report.py 按 Cargo.lock 精确版本读本地缓存生成索引 + 2 项回归测试；THIRD_PARTY_NOTICES + docs/third-party 原许可 |
| 第三方缺口 | tools/antigravity-l10n（引擎 1,984 行 + 词典 4,042 行）**上游无 LICENSE**，NOTICE 自认「不构成再分发授权」 |

### 3.3 验证面（存在性 ≠ 可复跑性，两列分开）

| 设施 | 存在 | 本次复跑结果（2026-11-18） |
|---|---|---|
| Rust 单测（内嵌 #[test]） | 79 个 cfg(test) 模块 | **全量 426 项：392 过 / 1 红 / 33 忽略**（隔离 target，两次复跑结果一致）。主二进制 423 项 = 389 过 / 1 红 / 33 忽略（40.45s / 49.22s）；第二二进制 codex-shared-resume 3/3 绿（主 harness 失败后 cargo 默认不跑它，单独 `--bin` 复跑）。另有 4 次子进程隔离复跑各 1 项，系 gateway/usage 测试用 `current_exe --exact` 防环境污染的有意设计，不计入总数 |
| node:test 离线前端测试 | 9 套 cjs | **86/86 全绿**（0.9s） |
| npm test（粘贴解析） | 1 套 | 通过（7 key 正确识别） |
| Python unittest | 10 套 93 方法 | 3 套全绿（privacy 7 / bundle 6 / zcode-auth 4）；3 套共 21 方法**断言全过**、tearDown 清理被评估环境文件保留钩子拦截（license 2 / usage-local 15 / agentfind 4）；chatgpt-history 首轮 25 跑 / 1 跳过 / **0 断言失败** / 40 errors = 16 缺依赖（compression/zstandard）+ 24 清理拦截，补装 zstandard 0.25.0 后复跑挂起于环境审计钩子（chatgpt-history.py:487），本环境无法完整复跑；shared 14 / sync 13 方法同样挂起于该钩子（faulthandler 栈留档，**非项目缺陷**）；workbuddy-quota 3 方法硬编码邻仓 `D:\Codebase\agent-memory-suite\agenthub.py` 缺失无法跑 |
| Playwright 浏览器 UI 测试 | **19 套 cjs** | **0 套可复跑**：硬编码 `channel:'chrome'`，本机无 Chrome（仅 Edge） |
| 真机 e2e（ps1） | restart-tray-smoke（原生托盘菜单 Win32 调用）、chatgpt-desktop-smoke、restart-login 等 10 个 ps1 | 未复跑：需安装版实例 / 真实客户端，本环境不具备 |
| 批次闸门 | check-gateway.ps1 等 check-*.ps1（cargo test + node + privacy/license 串联） | 未整体复跑（内含 cargo test 已用隔离方式单独复跑） |
| 隐私门禁 | tools/privacy_check.py + 7 项回归 | 回归 7/7 绿；**负向探针实测能红**（伪造 key→credential-like-literal、嵌入应用密钥→embedded-application-key、中性样本放行） |
| 性能基准 | 无基准文件（全仓 *bench*/*baseline* 搜索为空） | 性能数字仅存于注释级实测（300ms+ / 250–300ms），无回归机制 |
| CI | .github / gitlab-ci 等 | **无任何 CI 配置** |
| 发布链 | privacy_check → private_build 路径脱敏 → 二进制扫描 → NSIS → export_public_source 无历史快照 | 未执行发布（评估不打包）；pre_deploy.ps1 含实例竞态 + 8788 端口双硬检查 |

**本次复跑确认绿项合计 496**（Rust 392 + node 86 + paste 1 + privacy 7 + bundle 6 + zcode-auth 4）；另有 21 个 Python 方法断言通过但清理被环境拦截（chatgpt-history 首轮 0 断言失败，但 16 方法因缺依赖未执行、补依赖后复跑挂起，不计入「断言通过」）。33 个 Rust ignored 均为显式真机/网络副作用测试（qproxy 5、wbproxy 6、local_quota 3 等），日常构建不跑是合理设计。

---

## 四、九维度评分（模式 A 直接打分）

> 由 `scripts/score.py` 从 `score_input.json` 生成，原文见 `评分表.md`。锚点对照见每维依据栏。

| 维度 | 分数 | 依据（摘要） |
|---|:---:|---|
| 架构与模块化 | **6.5** | 单向分层成文、Rust 70 模块职责单一、代理按家族拆分；但 app.js 10,376 行巨文件 + commands.rs 3,798 行集中分发 + 46 个 >500 行文件 + 无分层护栏测试 |
| 可读性与命名 | **8.5** | docstring 写「为什么 + 红线」且带事故日期（lifecycle 2026-10-20、commands 死锁 2026-10-18）；命名跨语言一致；巨文件导航成本与绝对化注释措辞扣分 |
| 稳定性与异常处理 | **8.5** | 降级契约成文入测试（None≠0、空流显式报错）；catch_unwind 桥接；原子写带 rollback 与专测；坏数据自愈（明文 Key sweep）；生产 unwrap 1.60/千行 |
| 性能与资源占用 | **7.5** | get_state 异步、refresh_all 后台化、锁外探测、hidden 跳轮询、日志有界尾读、release LTO/strip；无基准文件与回归机制 |
| UI 组织 | **8** | S 集中状态逐字段注释、事件驱动落位 + 下一帧复核、matchMedia 响应式、el() 构建 DOM（innerHTML 仅 9 处静态 SVG）；单文件 10,376 行扣分 |
| 线程与并发安全 | **8.5** | CMD_LOCK 单份 + 三段式锁策略成文（两次事故复盘）；共享态全 Mutex/OnceLock；每请求一 worker 保回环；锁中毒降级；纪律未用护栏测试钉死 |
| 配置与日志 | **8** | 默认值深合并 + 安全读 + save_config 原子写 + last_good；lifecycle.log 落盘 / 512KB 裁剪 / panic 钩子 / 「start 无 exit=被强杀」取证语义；7 处 append 日志点；无分级开关 |
| 测试与打包 | **7.5** | 426 项 Rust 实跑（392 绿）+ 86 node + 隐私门禁能红 + 发布链完整；1 环境依赖红、19 套浏览器测试不可移植、邻仓硬编码、无 CI、真机 e2e 未复跑 |
| 依赖管理 | **8** | 双锁文件精确锁 + 钉版注释 + 内置运行时哈希 fail-closed + 许可索引回归；l10n 无 LICENSE 缺口、vendored agenthub 带本地改动 |

### 综合分（固定口径）

| 版本 | 九维均值 | 盲区折让 | 综合分 |
|---|:---:|:---:|:---:|
| v2.5.1（2026-11-18） | 7.89 | −0.20 | **7.7** |

折让判定（二值，不微调）：① 测试与打包 7.5 < 8，触发；② 托盘菜单 / 单实例 / junction 换号 / WebView2 cookie 罐 / DPAPI 真机解密等核心能力，只有需安装版的 ps1 e2e 且本次未复跑，19 套浏览器测试本机全灭——「测不到」本身就是风险，计入总分而非只写备注。

### 4.1 打分性质说明（比数字更重要）

- **8.5 的三维（稳定/可读/并发）是「体系已成」**：不是某次修得好，而是契约、复盘、红线注释形成了可传承的纪律。继续投入边际收益低。
- **6.5 的架构是「结构对、粒度欠」**：分层方向完全正确（前端只拿聚合脱敏快照、副作用全在 Rust），欠的是巨文件拆分与钉住分层的护栏——这是唯一低于 7 的维度，也是改动面最大的风险源。
- **7.5 的测试是「面宽、根浅」**：423+86+93 的账面规模不小，但可移植性差（Chrome 硬编码、邻仓路径、真机状态依赖），且无 CI 兜底——分数反映的是「验证的可复现性」，不是「断言的数量」。

---

## 五、当前真实短板（严重度排序，含打自己脸的）

| # | 短板 | 严重度 | 证据与说明 |
|---|---|---|---|
| 1 | **测试套件不可移植：换台机器就红/跑不了** | 高 | ① 唯一红项 `registry::tests::appdata_template_is_expanded_for_qoder_profiles`（registry.rs:2583）断言硬编码新壳目录，而本机旧壳 `%APPDATA%\Qoder` 留有登录标志（Local State + state.vscdb）、新壳无 auth.v1.dat——产品逻辑按「真有登录数据」正确选中旧目录，**测试绑死真机状态**（skill 坑 #13 活例）；② 19 套 Playwright 硬编码 `channel:'chrome'`，无 Chrome 机器全灭；③ workbuddy-quota.test.py:7 硬编码邻仓 agent-memory-suite 路径，而项目自己已 vendored 同一文件 |
| 2 | **核心能力验证盲区 + 无 CI** | 高 | 托盘/单实例/junction/DPAPI/WebView2 cookie 罐只有需安装版的 ps1 e2e（本环境未复跑，不声称通过）；无任何 CI 配置，全绿依赖开发者自觉。折让 −0.20 由此而来 |
| 3 | **ui/app.js 10,376 行巨文件** | 中高 | 占前端行数 53.7%；状态、渲染、向导、签到、网关全在一文件；46 个 >500 行文件无行数护栏。架构 6.5 的主因，也是后续每次改动的回归风险放大器 |
| 4 | **打自己脸：本报告的「496 项绿」≠ 真机可靠** | 中 | 496 项全部是离线/进程内断言；真机托盘菜单、换号 junction、安装版隔离验收本次**一项都没跑**。qsearch 的教训（687 项全绿当天真机闪退）在此同样适用——本报告拒绝把「离线绿」写成「产品可靠」 |
| 5 | **第三方 l10n 无 LICENSE 却随安装包分发** | 中 | tools/antigravity-l10n/NOTICE.md 自认上游无许可、「不构成再分发授权」，但 README 的 Release 安装包包含该面板——法律缺口已知未闭 |
| 6 | **性能无基准机制** | 低中 | 注释里的 300ms+/250–300ms 是历史实测，无基准文件、无回归消费——「有数据、没机制」，性能退化不会被任何闸门拦住 |
| 7 | **评估环境自身的坑（记录在案，防下次误诊）** | 低 | 本会话受管 Python 带全局文件保留审计钩子：任何 shutil.rmtree 被拦（3 套 Python 测试 tearDown 报 PermissionError，chatgpt-history 24 处）、文件操作审计时 socket read 挂起（shared/sync 恒挂；chatgpt-history 补装 zstandard 后亦挂于 chatgpt-history.py:487）。另首轮 chatgpt-history 16 errors 系受管 Python 3.12 缺 compression/zstandard（测试声明的双通道依赖），补装后不再缺依赖。**这些都不是项目缺陷**；faulthandler 栈已留档（sync-faulthandler.log、chatgpt-history-faulthandler.log）。下次复评若见同类红，先排除环境再谈代码 |

---

## 六、口径自检（三问）

| 自检项 | 做法 | 结果 |
|---|---|---|
| 分数有证据吗？ | 每维依据栏挂源码行号 / 测试名 / 实跑输出；红项追到产品逻辑与真机目录状态双重验证 | ✅ 九维全部可复核；红项归因经「目录内容 + has_login_data 逻辑 + git diff 排除 WIP」三方交叉 |
| 数字是实测的吗？ | measure.py 实盘 + 人工分区脚本 + cargo/node/python 真跑；426/86/496/510/93 等全部来自实跑输出或计数脚本，存于 summary.json | ✅ 无估算；measure.py 的 vendored 漏排与日志误判已人工修正并留痕 |
| 有没有粉饰？ | 短板 #4 直接打本报告自己的脸（离线绿≠真机可靠）；#1 指出唯一红项是测试缺陷而非产品 bug，但不因此抬分；未复跑设施一律写「未复跑」 | ✅ 折让照扣；19 套浏览器测试 0 复跑如实计入 |

补充纪律执行记录：
- **坑 #6（没跑别写通过）**：所有「已通过」均带 2026-11-18 实跑日期；ps1 真机 e2e、发布链、批次闸门整体 = 未复跑。
- **坑 #7（断言能不能红）**：隐私门禁用项目自带负向 fixture 实测能红（不修改任何项目文件）；Rust 红项本身证明测试体系能红。
- **坑 #8（静态≠实跑）**：Rust 静态 #[test] 426 个 = 主二进制 423 + 第二二进制 codex-shared-resume 3；首轮只跑主 harness（失败后 cargo 默认不跑第二目标），补跑后全量 426 项与静态数吻合。报告一律采用**实跑打印总数**，不数源码。
- **坑 #9/#10（归因与漏检）**：Python 测试红/挂先排除环境（审计钩子栈证据）再下结论；measure.py 报「验证设施全无」经人工回看修正为 ps1/cjs 设施存在。
- **坑 #13（全绿是时间炸弹）**：唯一红项正是绑真机状态的断言，已作为短板 #1 上报；本报告所有「绿」均注明运行日期。

---

## 七、最该做的一件事（只给一件）

**把测试套件变成「任何开发机一条命令可复跑全绿」——先消除验证不可复现的风险，再谈提分。**

性价比 = 消除的风险（换机即红、真机盲区无兜底、巨文件改动无护栏）× 工作量倒数（三处小改 + 一个 CI 文件，远小于拆 app.js）。三步：

1. **修可移植性（半天级）**：① registry 那个红测试改为注入临时目录 fixture（项目里 `resolve_profile_in(spec, instances)` 已是现成接缝，不必新造）；② Playwright 启动改 `channel: process.env.COCKPIT_BROWSER_CHANNEL || 'chrome'` 并支持 playwright 自带 chromium 兜底；③ workbuddy-quota.test.py 的 SOURCE 改指 vendored 副本（保留邻仓可选覆盖）。
2. **立一条 CI（一天级）**：GitHub Actions 跑 `cargo test` + 9 套离线 node + 可离线 Python 套 + privacy/license 门禁；真机 ps1 与浏览器套标注「需真机/需 Chrome」单独手动触发。让「全绿」从自觉变成机制。
3. **补一个基准落盘（半天级）**：把注释里的 get_state 耗时实测做成 `baseline_perf.json` + 一条消费它的回归断言（超阈值红），性能从「注释级实测」升级为「闸门级实测」。

> 拆 ui/app.js 是架构维度的正解，但工作量大、回归面广，应排在验证可复现之后——**没有可信闸门之前拆巨文件，等于蒙眼开刀**。当「提分」与「消除风险」冲突时，优先消除风险。

---

## 附：一页速查

| 问题 | 答案 |
|---|---|
| 这项目什么水平？ | 综合 7.7 / 10：工程纪律成体系（并发/降级/隐私发布链），个人维护项目里少见 |
| 强在哪？ | 稳定性、可读性、并发各 8.5——契约入测试、注释带事故复盘、锁策略单份成文 |
| 弱在哪？ | 架构 6.5（app.js 万行巨文件）；测试 7.5（可移植性差 + 真机盲区 + 无 CI） |
| 测试到底多少？ | 实跑确认绿 496 项（Rust 392/426，1 红为环境依赖断言，33 忽略为真机测试）；另有 19 套浏览器测试与真机 e2e 本机未复跑 |
| 先改哪？ | 修三处测试可移植性 + 立 CI + 基准落盘；拆 app.js 排在其后 |