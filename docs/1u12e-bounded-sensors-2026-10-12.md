# 1U12E 有界传感器分页整改（2026-10-12）

## 真实故障证据与边界

ReadyIDC `CPU-Server-02` / `10.1.40.51` / `asrockrack_1u12e` 的 v0.2.3 自然采集为 `up=1`、`scrape_complete=0`、`scrape_errors=1`，CPU 2、DIMM 4；NVMe 数量没有值。此前经授权的只读发现确认 Sensors 目录共 136 条、首屏 50 条并带 nextLink。当前实现 `fetch_members(...,64)` 明确拒绝分页，这与失败吻合，不是电源硬件故障的证据。PowerSupplies 数组 22 条也不能直接认作 22 个物理 PSU。

本轮代码与测试没有新建 BMC 会话、没有更改 UFM 或中心，也没有在聊天、日志、Git 或文档复制 BMC 凭据。生产版本仍由独立授权决定。

## 候选 v0.2.4 的采集策略

1. 仅 1U12E profile 的 Sensors 走新适配器；其他 CPU、B300 与慢盘策略保持原路径。
2. 每轮发现完整目录，最多 8 页、256 个唯一成员；成员必须在原 Chassis Sensors 目录下。nextLink 仅允许同一相对资源的数字 `$skip` / `$top`，offset 严格增加；拒绝跳地址/资源、重复成员、分页循环、`$expand`、错误总数和超限。
3. 每轮最多 48 个详情 GET，同时服从原有整轮 96 请求、单 BMC 串行与 140 秒总时限；没有增大预算。136 个详情在三个成功的自然轮次中覆盖，600s 周期下约 20 分钟预热。不能承诺首轮完整。
4. 缓存只保存有界指标，不保存整份可能很大的响应。目录改变清空旧缓存；超过 1800s 的观测不输出。失败成员不会沿用旧成功值掩盖故障；403 仍停止后续 GET 并退避。
5. 保留每个传感器原始 Prometheus 样本时间和源时间指标，`sensor_id` 区分重名。power 组成功时间为其最老的传感器观测，不是 HTTP 响应生成时间。
6. `redfish_sensor_inventory_count` 为实测目录大小，不是员工声明的期望基线；cached_count / snapshot_complete 为覆盖诊断。预热、过期、失败或目录不完整仍是 partial。硬件健康与采集成功继续分开。

时间戳消费者必须保持 `honor_timestamps: true`；NeoStor baseline 当前未覆盖此默认值，慢组已明确设为 true。不得用“HTTP 200”或“收到新上行包”替代传感器源时间新鲜度。

## 离线验证

本地模拟 BMC 先复现原版失败，再验证 136 / 50 条分页的三轮覆盖为 48、96、136，每轮请求数不超过 96、详情 GET 不超过 48，原始时间戳在重放中不变。另覆盖不安全 nextLink、重复/截断/超限目录、8 页上限、过期清除、目录改变、剩余整轮预算、403 当前及下一轮停止、power 组最老源时间。

运行命令（只访问本地 mock，不访问 IDC）：

```powershell
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

本机终态为 36 passed、0 failed、0 ignored，fmt 与严格 Clippy 成功。GitHub Linux / Docker CI 终态另行登记。离线通过不能替代实机验收；当前 v0.2.4 未发布或部署。

## 必须保留的实机门禁

- 固定源码 SHA 构建新 `v0.2.4` 镜像，不覆盖 v0.2.3；通过 DockerHub 由 UFM 拉取，校验 digest / OCI revision / `--version`。
- 单独批准后只升级本工程 ReadyIDC exporter，保留原私有配置、Agent UUID/token、队列、备份；旧重叠 Python 采集继续停止，其他监控不动。
- 仅保留原三台灰度，等待 CPU-Server-02 三个自然轮次；确认 source、覆盖、分组成功、队列及业务/资产保全。失败不能用扩大预算、填零或隐去指标处理。
- 核对真实 BMC nextLink 格式是否在已实现的数字分页范围内；不支持的格式保留 partial 并补适配，不猜测其他 URL。
- NVMe 无数据仍须独立判定 BMC 能力，不能随着 Sensors 修复宣称完整磁盘资产验收。
- 真实 Web 图表显示及两地全量接入仍为独立未完成项。
