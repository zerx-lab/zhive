---
topic: 基于 zhive 引擎的下一步产品方向与基础营收路径(中国大陆)
date: 2026-06-09
status: active
---

# 产品营收调研:zhive 下一步做什么产品能迎来基础营收流水

> 调研方法:5 个并行调研角度(大陆 AI 编码工具市场 / 国产模型 API 生态 / 合规与变现路径 / CLI 形态接受度与开源引擎变现 / 可复制产品案例),共 50+ 次中英文搜索、25+ 次关键来源抓取,关键论断经一轮独立对抗性核查(5 条核心论断:2 条确认、3 条部分确认并修正)。时间基准 2026-06。
>
> 目标定义:**基础营收流水 = 月入数千到数万人民币、可接受收支持平**,非 VC 增长路径。

---

## 一、结论先行:推荐方向(按达到基础营收的可能性排序)

### 方向 1(首推):企业/集成商侧的私有化 agent 引擎 + 定制交付

**做什么**:把 zhive 定位为"可私有化部署的内网编码/办公 agent 引擎",以**定制交付单**为现金流起点:给大陆企业(或给中标大模型项目的集成商做二包)交付内网 agent 系统,zhive 引擎是交付底座,每单沉淀为引擎功能与模板。

**为什么最可能先有收入**:
- 大陆 B 端预算真实且巨大:2025 年全国大模型中标项目 7,539 个、披露金额 295.2 亿元(项目数 +396% YoY),中位数单项目 86 万元;私有化部署是金融行业主流交付模式([smartcity.team 年度盘点](https://www.smartcity.team/news/2025%E5%B9%B4ai%E5%A4%A7%E6%A8%A1%E5%9E%8B%E6%8B%9B%E6%8A%95%E6%A0%87%E6%80%BB%E7%BB%93/))。AI 编程助手有真实银行中标先例(通义灵码中标工行、建行项目;文心快码中标国开行)。
- 个人可切入的不是直接投标(被阿里/百度/腾讯模型捆绑通吃),而是**轻量定制单与集成商二包**:经对抗核查,轻量集成单 3-8 万元属市场最低档,主流水位为单场景 MVP 5-15 万、项目制买断 10-30 万([搜狐行业文](https://www.sohu.com/a/1024691054_122547685)、[知乎](https://zhuanlan.zhihu.com/p/1970177206434136243)、[数商云](https://www.shushangyun.com/article-30394.html))。**每月 1 单即超额达成目标**。
- zhive 的技术特性恰好是私有化场景的卖点:Rust 单二进制、无运行时依赖、daemon + JSON-RPC 协议边界(内网审计友好)、四态权限 reducer + hooks(企业管控刚需)、JSONL 持久化(留痕合规)、多 provider 数据驱动(对接已备案国产模型/私有化 DeepSeek)。
- 开源同行的变现路径全部验证了"钱在企业治理层":OpenHands 企业版(VPC 自托管/SSO/Agent Control Plane,2026-05 GA)、Tabby 企业版、Cline Teams $20/席([openhands.dev/pricing](https://www.openhands.dev/pricing)、[tabbyml.com/pricing](https://www.tabbyml.com/pricing)、[Sacra/Cline](https://sacra.com/c/cline/))。

**获客**:不走猪八戒/平台竞价(单价 5-100 元,无意义),而是在小红书/公众号/V2EX 持续发布可复现的实战案例引流到私域;同时盯住属地大模型中标公告,主动联系中标集成商做实施分包。

**关键风险**:① 获客冷启动周期(预计 1-3 个月无收入);② 接企业单需开票 → 必须先注册个体工商户或一人有限公司;③ 交付是重活,会挤占引擎开发时间——缓解办法是只接能沉淀为 zhive 通用能力的单子。

### 方向 2(并行做):BYOK 多会话编排 + 远程控制客户端(国内外双市场)

**做什么**:基于 zhive 的 daemon + 协议架构(这正是它与 Omnara/Happy 等外挂方案的本质区别——它们是给别人的 agent 套壳,zhive 远程客户端是引擎原生能力),做"手机/Web 随时续接、多 agent 并行编排、跨国产模型"的付费客户端。国内卖给 GLM/Kimi/火山 coding plan 用户(他们没有任何官方远程方案),海外走 Paddle 收美元。

**为什么可行**:
- 付费已被验证:Omnara(YC S25)现价免费 10 会话/月 + $20/月不限量(经核查,$9 是旧价),CodeRemote $49/月([Launch HN](https://news.ycombinator.com/item?id=46991591)、[omnara.com/pricing](https://www.omnara.com/pricing))。
- 但纯"远程控制"单点已死:Anthropic 2026-02 推出官方 Remote Control(订阅内含、server 模式支持 32 并发会话、worktree 隔离)([官方文档](https://code.claude.com/docs/en/remote-control))。**幸存空间在官方不覆盖处**:跨 provider(国产模型)、机器离线后的云端接管、多 agent 统一面板、团队审计、自托管。
- 大陆独有生态位:Anthropic 2025-09 起对中资控股实体全球封锁([官方公告](https://www.anthropic.com/news/updating-restrictions-of-sales-to-unsupported-regions)),而 DeepSeek/Kimi/GLM/Qwen 全部官方提供 Anthropic 兼容端点,¥49-199/月 coding plan 已把大陆开发者教育成"愿意为编码 agent 包月"([GLM Coding Plan](https://docs.bigmodel.cn/cn/coding-plan/overview)、[Kimi Code](https://www.kimi.com/code/docs/)、[awesome-coding-plan](https://github.com/mahonzhan/awesome-coding-plan))。zhive 可以做成**"国产模型优先的 Claude Code + 官方级远程体验"**,这是 Claude 官方永远不会服务的市场。
- 商业模式必须 BYOK:用户自带国产模型 key 或 coding plan(厂商明示套餐可被第三方工具复用),产品只收软件钱。自己捆绑 token 打不过厂商补贴价(¥9.9-49/月),且 2026 年已进入套餐涨价周期(智谱年内三连涨、阿里云算力 +34%),捆绑推理毛利模型脆弱([科学网](https://news.sciencenet.cn/htmlnews/2026/4/562401.shtm))。

**定价参考**:国内 ¥15-30/月或买断 license(卡在 coding plan 几十元/月与 JetBrains 千元/年之间的空白带);海外 $10-15/月(Omnara 用户对 $20 已有明显抵触,支付意愿上限在 $10-20)。

**关键风险**:① 官方功能持续下压(Remote Control 还在 research preview,会继续扩张);② 客单价低,到 $1k MRR 预计需 6 个月以上;③ 国内网页版 SaaS 形态有被属地网信办要求补"大模型登记"的风险——**本地客户端 + BYOK 形态暴露面最小**(见第三节)。

### 方向 3(复利层,不单独做):开源引擎 + 内容获客

把 zhive 引擎本体开源(Apache/MIT)作为获客资产而非收入来源,叠加实战内容(公众号/小红书/B站/知识星球),内容同时反哺方向 1 的私域获客与方向 2 的用户漏斗。有真实经验背书的 AI 课程/星球 8 个月可累计 15 万+([提效录](https://www.tixiaolu.com/posts/ai-knowledge-payment-2026),低-中置信度),但单独做内容不可靠——它是方向 1/2 的放大器。

**为什么引擎必须开源、必须不指望它收钱**:经对抗核查无反例——开源编码 agent 靠工具本体收到显著收入的案例为零。Cline 500 万安装(2026-01)对应 ARR 仅约 $5M(2025-08 Sacra 估算),折合每安装 $1-2/年;Aider 零变现;ccusage 等监控工具只有 GitHub Sponsors;协议层(Zed 的 ACP)明确零抽成([sacra.com/c/cline](https://sacra.com/c/cline/)、[zed.dev/acp](https://zed.dev/acp))。

### 推荐组合

**方向 1 出现金流(1-3 个月起量)+ 方向 2 出产品(3-6 个月first dollar)+ 方向 3 做漏斗**。V2EX 真实样本(大陆独立开发者年到手 21 万已属前 15%,[V2EX 年终总结](https://v2ex.com/t/1183856))表明:目标量级现实可达,但大概率来自"外包现金流 + 一个 $500-2k MRR 产品"的组合,而非单一爆款。

---

## 二、市场事实基础(支撑上述判断的核心论断)

### 2.1 大陆 AI 编码工具市场:付费意愿存在,但锚点被打到 ¥20-50/月

- 大厂 IDE/CLI 核心功能基本免费,2025 底起集体转向 Coding Plan 订阅:CodeBuddy Lite ¥40/Pro ¥200(首购 ¥7.9/¥39.9),字节 Doubao-Seed-Code 首月 ¥9.9、标准 ¥40,阿里百炼 ¥7.9 首月引爆 2026 年 2-3 月价格战(9 平台 28 套餐,入门 ¥19-49)。【置信度:高】([CodeBuddy 定价](https://www.codebuddy.ai/docs/zh/ide/Account/pricing)、[SCMP](https://www.scmp.com/tech/big-tech/article/3332365/bytedance-unveils-chinas-most-affordable-ai-coding-agent-just-us130-month)、[2026-03 横评](https://blog.lightnote.com.cn/china-ai-coding-plan-benchmark/))
- **价格战已现回调**:智谱 2026 年内多次涨价(Coding Plan 整体 +30% 起)、阿里下线 ¥40 Lite 档——"白菜价不可持续"信号明确,这对独立产品是窗口:用户已被教育成愿意包月,且开始警惕大厂砍套餐。【置信度:中高】([BuyGLM 指南](https://buyglm.com/guides/china-ai-coding-plan-pricing-routes-2026))
- **标志性事件**:阿里 iFlow CLI"免费不限量"运营约一年后,2026-04-17 停服并导流到收费的 Qoder——连大厂都认为纯免费 CLI 不可持续。通义灵码 2026-05-20 更名 Qoder CN,转 Credits 制收费。【置信度:高】([qwen-code Discussion #836](https://github.com/QwenLM/qwen-code/discussions/836)、[阿里云帮助中心](https://help.aliyun.com/zh/lingma/product-overview/introduction-of-lingma))
- 存在高付费意愿客群:Claude Code 拼车/合租 ¥299-999/月、Cursor 代充产业链成熟——证明一批大陆开发者愿为顶级 agent 体验付 ¥100+/月,这批"被封号折磨"的用户是独立产品的种子客群(但拼车本身违反 ToS,不是可做的生意)。【置信度:存在性高、规模无统计】([v5site](https://www.v5site.com/claude-code/))
- JetBrains 人民币计价全家桶 ¥1400+/年有市场——大陆专业开发者存在千元/年级工具付费习惯。【置信度:高】

### 2.2 国产模型生态:BYOK 摩擦趋近于零,这是 2024 年不存在的红利

- DeepSeek/Kimi/GLM/Qwen 均**官方**提供 Anthropic 协议兼容端点,`ANTHROPIC_BASE_URL` 一行切换;GLM 官方宣称套餐支持 Claude Code/Cline/OpenCode 等 10+ 工具。"国产模型跑开源 agent"已是大陆主流常规操作。【置信度:高】([DeepSeek 官方文档](https://api-docs.deepseek.com/guides/anthropic_api)、[Kimi Code 文档](https://www.kimi.com/code/docs/))
- **对 zhive 的直接含义**:坚持 Anthropic 协议兼容 = 零成本接入全部国产官方端点;llmsdk 的多 provider 架构是正确押注。
- token 价格从单边下跌转为**分化**:DeepSeek 类低成本架构仍降价(v4-flash 输出 $0.28/M),但旗舰与套餐涨价(全国日均 token 调用量两年涨 1000 倍,算力供给吃紧)。任何捆绑推理的定价都需留调价条款——再次指向 BYOK。【置信度:高】([DeepSeek 定价](https://api-docs.deepseek.com/quick_start/pricing)、[科学网 2026-04](https://news.sciencenet.cn/htmlnews/2026/4/562401.shtm))
- Anthropic 对中资控股实体全球封锁(2025-09 起,股权穿透 >50%),大陆产品永远不能把 Claude 官方模型写进默认路径——反而固化了"协议兼容 Claude、模型用国产"的本地生态位。【置信度:高】([官方公告](https://www.anthropic.com/news/updating-restrictions-of-sales-to-unsupported-regions))

### 2.3 合规:BYOK 本地客户端暴露面最小,出海个人即可起步

- 生成式 AI 备案的法规分水岭只有两条:是否"向境内公众提供生成式 AI 服务"、是否"具有舆论属性或社会动员能力"。纯开发者编程工具文义上大概率落在豁免区,但**无官方明文豁免**,属地网信办对"有公开网页 + 对话框"倾向从宽认定需要手续。调用第三方已备案模型的应用走"大模型登记"(省级,1-3 个月,流程简化)。【置信度:法条高、属地执行中】([网信办法规原文](https://www.cac.gov.cn/2023-07/13/c_1690898327029107.htm)、[汉坤律所](https://www.hankunlaw.com/portal/article/index/cid/8/id/13552.html))
- **BYOK + 本地客户端(无服务器中转内容)最接近豁免**;BYOK 是降险手段而非法定安全港,不要当成合规结论宣传。网页版 SaaS 需预留登记预算。【置信度:低-中,合理推演无判例】
- 内销最小路径:个体工商户/一人公司 → 企业 ICP 备案 → 只默认接入已备案国产模型 → App Store 苹果代收(个人最干净通道)或商户号;买断 license key + 平台代售是国内独立开发者主流做法。【置信度:中高】
- 出海最小路径:**Paddle 以个人身份可申请**(护照 + 自有域名,审核 20+ 工作日),SWIFT 提现大陆卡约 $15/笔,经核查 2025-2026 无收紧迹象;Lemon Squeezy 已不支持大陆银行收款。收入稳定过 $2-3k/月再上香港公司 + Stripe(年维护约 1 万港币)。服务贸易申报结汇不占 5 万美元个人额度。【置信度:高】([Paddle 支持国家](https://www.paddle.com/help/start/intro-to-paddle/which-countries-are-supported-by-paddle)、[万里汇 2026 指南](https://www.worldfirst.com.cn/static/contenthub/blog/2026-developer-overseas-payment-guide))

### 2.4 哪些路被证伪(不要做)

| 方向 | 证据 | 结论 |
|---|---|---|
| 开源工具本体收费/赞助 | Cline 500 万安装 ≈ $5M ARR;Aider 零变现;ccusage 仅 Sponsors;对抗核查无反例 | 引擎只能当获客资产 |
| 捆绑 token 的订阅 | 厂商套餐是亏本获客品(社区测算折合 API 0.1 折)且在涨价周期 | 打不过补贴,毛利脆弱 |
| API 中转/拼车 | 单月盈利可达百万级但属灰色地带、封号与合规风险高 | 不可作主业 |
| 纯用量监控/小工具 | 高流量品类但全部免费开源,无付费先例 | 有流量无收入 |
| 协议/引擎授权费 | ACP 零抽成;"harness 即平台"无成立案例,钱都收在网关或企业授权 | 协议是分发渠道不是收入 |
| 平台智能体分成(Coze/文心) | 个人激励 7 千元级,内卷严重 | 量级不及目标 1/10 |

---

## 三、对 zhive 路线图的具体含义

1. **协议边界是商业资产**:`zhive-proto` 的"引擎与客户端分离"在方向 1(内网部署引擎,客户端按需定制)和方向 2(远程/移动客户端是原生能力而非外挂)中都是核心差异化,继续坚持。
2. **优先补齐企业管控面**:审计日志导出、会话留痕、权限策略集中配置、离线部署文档——这些直接服务方向 1 的交付,且是 OpenHands/Tabby 企业版验证过的付费点。
3. **Anthropic 兼容端点是第一公民**:确保对 DeepSeek/Kimi/GLM/Qwen 官方 Anthropic 端点的开箱即用配置与文档(llmsdk 层),这是大陆获客的第一入口。
4. **远程客户端(方向 2)按"官方不覆盖处"设计**:跨 provider、离线接管、多 agent 面板、自托管,不要复制官方 Remote Control 已有的单人多设备续接。
5. **主体与收款先行**:注册个体工商户/一人公司(接企业单开票 + 企业 ICP)、提前申请 Paddle(审核 20+ 工作日),两者都有周期,应在产品就绪前启动。

## 四、主要不确定性

- 大陆企业单的实际获客转化率无任何公开数据,方向 1 的"1-3 个月起量"是基于社区经验的估计,需用 2-4 周实地询价/发案例验证。
- 个人小单行情(3000-15000 元/个)经核查缺乏系统性数据源,仅闲鱼个案佐证。
- 官方 Remote Control 仍在 research preview,其扩张速度直接决定方向 2 的窗口期。
- 属地网信办对开发者工具的认定口径因地而异,内销 SaaS 形态的登记风险无法事前完全消除(缓冲期内补办即可,提前公司化基本消除个人备案风险)。

## Sources

完整来源散见各论断行内链接。最关键的一手来源:

- [Anthropic 不支持地区限制公告(2025-09-04)](https://www.anthropic.com/news/updating-restrictions-of-sales-to-unsupported-regions) / [Claude Code Remote Control 官方文档](https://code.claude.com/docs/en/remote-control)
- [DeepSeek API 定价](https://api-docs.deepseek.com/quick_start/pricing) / [Anthropic 兼容文档](https://api-docs.deepseek.com/guides/anthropic_api) · [GLM Coding Plan](https://docs.bigmodel.cn/cn/coding-plan/overview) · [Kimi Code](https://www.kimi.com/code/docs/)
- [《生成式人工智能服务管理暂行办法》原文](https://www.cac.gov.cn/2023-07/13/c_1690898327029107.htm) · [汉坤律所解读](https://www.hankunlaw.com/portal/article/index/cid/8/id/13552.html)
- [Sacra: Cline](https://sacra.com/c/cline/) · [OpenHands 定价](https://www.openhands.dev/pricing) · [opencode Zen](https://opencode.ai/docs/zen/) · [Omnara Launch HN(2026-02)](https://news.ycombinator.com/item?id=46991591)
- [2025 大模型招投标盘点](https://www.smartcity.team/news/2025%E5%B9%B4ai%E5%A4%A7%E6%A8%A1%E5%9E%8B%E6%8B%9B%E6%8A%95%E6%A0%87%E6%80%BB%E7%BB%93/) · [国内 Coding Plan 横评(2026-03)](https://blog.lightnote.com.cn/china-ai-coding-plan-benchmark/) · [awesome-coding-plan](https://github.com/mahonzhan/awesome-coding-plan)
- [Paddle 支持国家清单](https://www.paddle.com/help/start/intro-to-paddle/which-countries-are-supported-by-paddle) · [V2EX 独立开发者年终总结](https://v2ex.com/t/1183856)
