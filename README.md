<div align="center">

<img src="assets/airmonitor.svg" alt="AirMonitor" width="96" />

# AirMonitor

**极致轻量化的 Windows 11 / Windows 10 原生任务栏硬件监控与无线外设电量监视器**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Platform: Windows 10/11](https://img.shields.io/badge/Platform-Windows%2010%2F11%20x64-0078D4)
![Rust](https://img.shields.io/badge/Built%20with-Rust%20%2B%20Win32%20%2B%20Direct2D-DEA584)

</div>

---

## 核心特性

- **极致轻量化**：纯 Rust 原生 Win32 + Direct2D（Software RT）/ DirectWrite 实现，无 WebView / Electron、无内核驱动、免管理员权限。默认静态链接 MSVC CRT（`+crt-static`，零 `VCRUNTIME140.dll` 运行库依赖）并内嵌应用图标、版本与 PerMonitorV2 Manifest 资源；实测 Release 单文件仅 **~1.19 MB**（`1,250,304` 字节），常驻内存 Working Set 约 **1.4–6 MB**，空闲 CPU 占用 **< 0.2%**。
- **按需硬件采集与紧凑双行监控**：
  - 默认 2 列布局：第 1 列网络上行/下行（`B/K/M/G` 自动切换、固定 5 字符右对齐等宽，消除数字跳动），第 2 列 `CPU` / `MEM` 占用率。
  - 支持全部 **18 项硬件指标**自由勾选与 2 行所见即所得拖拽排布：`CpuUsage`、`CpuFreq`、`CpuTemp`、`CpuPower`、`CpuFan`、`MemUsage`、`NetUp`、`NetDown`、`DiskRead`、`DiskWrite`、`DiskTemp`、`SysBattery`、`GpuUsage`、`GpuTemp`、`GpuHotspot`、`GpuPower`、`GpuVram`、`GpuFan`。
  - **按需初始化、双区独立开关与空闲释放**：支持在设置页、概览页或托盘菜单独立开关「硬件监测」与「无线设备监测」。关闭硬件监测后后台立即释放 PDH/GPU/传感器句柄并停止周期唤醒；关闭无线监测后停止后台轮询与低电量提醒并释放设备缓存；仅在指标被启用或概览浮窗打开时初始化高开销接口，空闲 30 秒后自动释放句柄并收敛内存。
  - **Win11 原生子窗口托管架构**：任务栏浮条通过 `SetParent(..., Shell_TrayWnd)` 托管为任务栏子窗口（`WS_CHILD | WS_CLIPSIBLINGS`），自动跟随系统明暗/高对比度主题与 Monitor DPI；支持 **4 种布局模式**（`Combined` 合并 / `Split` 分体 / `HardwareOnly` 仅硬件 / `DevicesOnly` 仅设备；双区均关闭时自动隐藏任务栏浮条）、左右智能锚定（`Left` / `Right`）、`Ctrl + 左键拖动` 微调水平偏移、全屏自然遮挡与防抖（`GeometryStabilizer`）以及 `TaskbarCreated` 崩溃自恢复。
- **原生三标签浮窗与系统托盘**：
  - 左键点击任务栏浮条或托盘图标弹出 Win11 圆角原生浮窗（支持 `Tab` / `Shift+Tab`、方向键、`Enter` / `Space` 与 `Esc` 完整键盘导航），包含三大标签页：
    - **概览**：全量硬件遥测卡片（含数据源标识、采集无效时诚实展示 `N/A`、独立标注「主板/ACPI 热区」）、无线外设列表（支持单设备显隐与手动刷新）；当硬件或无线区关闭时自动展示轻量的已关闭占位卡与「立即开启」入口。
    - **设置**：三子标签页——**显示指标**（2 行 WYSIWYG 拖拽网格 + 4 分组 18 项指标芯片）、**布局停靠**（4 模式 + 合并顺序 + 左右锚定与偏移步进）、**通用选项**（硬件监测总开关、硬件刷新间隔 0.5–5.0s、外部传感器开关、隐藏无数据指标、无线设备监测总开关、低电量提醒阈值 5–50%、隐藏无电量设备、开机自启动）。
    - **关于**：应用信息、GitHub 仓库/Releases 入口、应用内检查更新与可取消的一键下载替换（HTTPS 白名单 + 强制 `SHA256SUMS.txt` 校验 + 120s 超时/随时取消 + x64 PE 校验 + 原子替换自动回滚；缺少校验文件时禁用自动安装并引导手动下载）及打开配置文件。
  - 托盘与浮条右键菜单支持「启用硬件监测」/「启用无线设备监测」独立开关、4 种布局模式与排序切换、区域锚定、18 项硬件监控项勾选、立即刷新无线设备、开机自启动（`HKCU\...\Run`）、打开配置文件、关于/检查更新与退出。
- **多传输无线外设电量引擎**：
  - **Xbox 手柄**：通过 `LOAD_LIBRARY_SEARCH_SYSTEM32` 动态加载 `xinput1_4.dll` / `xinput1_3.dll` 查询 0..3 号手柄状态与电量。
  - **经典蓝牙 & BLE GATT**：通过 Windows PnP 查询 `DEVPKEY_Bluetooth_Battery` 与连接状态，支持 `BTHLEDEVICE` ↔ `BTHLE` MAC 节点自动关联。
  - **AirPods / Beats BLE 广播**：基于 WinRT `BluetoothLEAdvertisementWatcher` 被动监听 Apple Continuity（Company ID `0x004C`，Type `0x07`）提取左耳 / 右耳 / 充电盒分体电量与充电标志。
  - **2.4G / USB HID 协议引擎**：内置 **108 条协议族设备 + 30 条声明式自定义 HID 规则 + 6 条厂商只读兜底规则（共 144 条**，编译期内嵌 `devices.toml`），并支持外部 `devices.toml` 热扩展；另提供命令行诊断工具 `airmonitor-probe.exe`。

---

## 硬件与无线设备支持矩阵

### PC 硬件监控（纯用户态，免驱动）

| 模块 | 采集方式 | 输出指标 |
| --- | --- | --- |
| **CPU** | `GetSystemTimes` + `CallNtPowerInformation` + 按需 PDH `% Processor Performance` | 总体占用率（%）、实时频率（GHz） |
| **内存** | `GlobalMemoryStatusEx` | 占用率（%）、已用 / 总容量（GB） |
| **网络** | `GetIfTable2`（过滤回环与虚拟网卡，锁定物理硬件网卡） | 实时上行 / 下行速率、活跃网卡名称 |
| **磁盘** | 按需 PDH `PhysicalDisk(_Total)` 读写计数器 | 实时读取 / 写入速率 |
| **本机电池** | `GetSystemPowerStatus`（无电池台式机自动返回 `N/A`） | 笔记本/掌机剩余电量（%）、充电状态 |
| **GPU** | DXGI 优先锁定独立显卡（过滤 Microsoft Basic / Oray / Virtual / Meta / MuMu / Parsec / IDD）；AMD `atiadlxx.dll`（ADL2 PMLog）；NVIDIA `nvml.dll`；Intel `ControlLib.dll` / `IntelControlLib.dll`（IGCL）；PDH `GPU Adapter Memory` & `GPU Engine(*engtype_3D)` 按需兜底 | GPU 占用率（%）、核心温度 / 热点温度（°C）、板卡功耗（W）、风扇转速（RPM / %）、显存已用 / 总量（GB） |
| **外部传感器融合（可选）** | 只读共享内存自动探测（优先级：HWiNFO64 `HWiNFO_SENS_SM2` → AIDA64 `AIDA64_SensorValues` → MSI Afterburner `MAHMSharedMemory`）+ Windows ACPI Thermal Zone（PDH） | CPU 温度（°C）/ 功耗（W）/ 风扇（RPM）、硬盘温度（°C）及 GPU 补充遥测 |

### 无线设备协议支持（已验证与固件边界说明）

| 设备 / 协议族 | 传输方式 | 支持情况与说明 |
| --- | --- | --- |
| **Xbox Wireless Controller** | 2.4G / 蓝牙 / 有线 | 通过 `XInput` 实时读取电量等级与在线状态（蓝牙连接时自动合并系统精确百分比） |
| **Samsung Galaxy Buds / 蓝牙外设** | 经典蓝牙 / BLE GATT | 读取系统 `DEVPKEY_Bluetooth_Battery`（自动合并音频端点与 MAC 父节点） |
| **Apple AirPods / Beats** | BLE 被动广播（`0x004C`） | 解析左耳 / 右耳 / 充电盒电量与充电状态（需本机蓝牙开启） |
| **Rapoo（雷柏）VT 系列（如 VT3 `24AE:1412`）** | 2.4G / 有线 | 支持 Gen2 Nordic 54L 被动推送 `0x07` 报文与 Legacy `0xBB B0` 7 字节报文，实时电量与充电状态 + 休眠缓存 |
| **CompX / Nordic 52840 & 54L15（如 Dareu A950 Pro/Air/Wing、ATK、VXE、VGN、Zaopin、Scyrox 等）** | 2.4G / 有线 | 支持 `FF02:0002` 17 字节 `0x08/0x04`（含 `0x55` 校验和）及 64 字节 `0x72/0x07` 查询；注：部分鼠标在休眠/关机时接收器不响应 |
| **Logitech HID++（`046D`）** | Lightspeed / Unifying / Bolt | 动态发现 `0x1004`（UnifiedBattery）与 `0x1000`（BatteryLevelStatus）多槽位电量特性 |
| **RongYuan（`3151` 等）/ Evision / MCHOSE / Cherry / Akko / Ninjutso / Attack Shark / Razer / SteelSeries / Corsair / HyperX / ASUS ROG** | 2.4G / 有线 | 已在 `devices.toml` 内置对应 `[[device]]` 与声明式 `[[custom_hid]]` 规则（详见 [`docs/protocol_sources.md`](docs/protocol_sources.md)） |
| **SKN 青龙 4.0（`362D:D101`）** | 2.4G 接收器 / 蓝牙 | ⚠️ **固件边界**：实机与上游 QMK/Keychron 源码核实，原厂 2.4G 接收器仅上报链路在线状态，未开放 HID 电量读取指令（`0x20 01` 返回 `0xFF`）；需在 **蓝牙模式** 或刷写开放电量指令的固件后读取百分比 |
| **Dareu A87 经典版（`05AC:024F`）** | 2.4G 接收器 | ⚠️ **固件边界**：实机探测证实其 2.4G 接收器不暴露 HID 电量通道（键盘端通过指示灯显示电量）；程序诚实显示在线占位或 `N/A`，不虚标数值 |

---

## 编译与运行

### 环境要求

- Windows 10 / Windows 11（x64）
- Rust 工具链（`x86_64-pc-windows-msvc`，已验证于 Rust `1.100.0`）

### 常用命令（PowerShell）

```powershell
# 确保 cargo 在 PATH 中
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# 1. 代码格式、静态检查与单元测试
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test

# 2. 编译 Release 二进制（开启 size 优化 + LTO + strip）
cargo build --release --bin AirMonitor --bin airmonitor-probe

# 3. 运行硬件与无线诊断探针（默认对设备 ID / MAC / 序列号及原始报文脱敏，方便安全分享日志）
.\target\release\airmonitor-probe.exe

# 3b. 如需排查原始设备 ID、MAC、序列号与完整 HID/BLE 载荷，传入 -v / --verbose
.\target\release\airmonitor-probe.exe --verbose

# 4. 运行主程序（任务栏常驻 + 系统托盘）
.\target\release\AirMonitor.exe

# 5. 无头自检模式（只读配置，打印内存占用、空闲 CPU、GPU UMD 与探测到的设备数量后退出）
.\target\release\AirMonitor.exe --smoke-test
```

---

## 应用内检查更新与 Release 说明

- 打开浮窗 **「关于」** 标签页（或托盘右键 **「关于 / 检查更新」**）可手动向 [GitHub Releases](https://github.com/AmlitaEnan/AirMonitor/releases) 检查最新版本。
- 更新流程通过系统 `winhttp.dll` 强制 HTTPS 传输，校验下载 URL 白名单与重定向、文件体积区间、x64 可执行 PE 头以及 `SHA256SUMS.txt` / `airmonitor.exe.sha256`（缺失校验文件时禁止应用内自动安装并引导至手动下载；存在时强制使用 Windows CNG `bcrypt.dll` 计算比对）。下载与校验阶段均支持 120 秒总超时控制及点击「取消更新」随时中断，随后执行原子替换并支持启动失败自动回滚（目录级互斥锁串行化保护）；若程序所在目录为只读（如 `Program Files`），将自动引导至浏览器手动下载。
- **发布资产约定**：每次 GitHub Release 自动上传 `AirMonitor.exe`、`airmonitor-probe.exe` 及对应的 `SHA256SUMS.txt` 以启用完整哈希校验（应用内更新器大小写不敏感，兼容 `AirMonitor.exe` 与 `airmonitor.exe`）。
- **SmartScreen 提示**：由于开源个人发行的二进制未购买商业 Authenticode 代码签名证书，首次运行从 GitHub 下载的 `AirMonitor.exe` 时 Windows SmartScreen 可能会弹出未知发布者提示；您也可以按上方命令从源码自行编译。

---

## 配置文件与自定义 HID 规则扩展

### 1. 运行时配置 `config.toml`

首次运行（非 `--smoke-test` 只读模式）会自动生成默认 `config.toml`。配置采用原子写入与双备份恢复机制：每次正常保存前会将上一份有效配置写入 `config.toml.last_good.bak`；若 `config.toml` 损坏或解析失败，程序会将其原样备份为 `config.toml.corrupt.bak`（不覆盖原文件），并自动从 `.last_good.bak` 恢复（均不可用时回退至默认配置）。查找优先级为：
1. `AirMonitor.exe` 同级目录下的 `config.toml`（绿色便携模式）
2. `%APPDATA%\AirMonitor\config.toml`

常用字段包括：
- 双区总开关与布局：`hardware_enabled`（硬件监测总开关）、`wireless_enabled`（无线设备监测总开关）、`layout_mode`（`"combined"` / `"split"` / `"hardware_only"` / `"devices_only"`）、`combined_order`（`"devices_first"` / `"hardware_first"`）、`combined_zone` / `hardware_zone` / `devices_zone`（含 `anchor` / `offset_x` / `visible`）。
- 硬件采集与显示：`hardware_items`、`hw_poll_interval_ms`（`500..=5000`）、`enable_external_sensors`、`hide_unavailable_metrics`。
- 无线与系统：`low_battery_threshold`（`5..=50`）、`hide_unknown_battery_devices`、`hid_poll_interval_secs`（`10..=600`）、`autostart`、`device_aliases`、`hidden_devices`。

### 2. 外部 `devices.toml` 热扩展

`airmonitor.exe` 编译期已内嵌仓库根目录的 [`devices.toml`](devices.toml)。如需在不重新编译的情况下追加或覆盖设备规则，可在 **可执行文件同级目录** 或 `%APPDATA%\AirMonitor\` 放置外部 `devices.toml`（外部规则按 VID/PID 优先级覆盖内嵌规则）。

支持两种规则形式：
- **协议族映射 `[[device]]`**：指定 `vid`、`pid`、`name`、`kind`、`protocol`（`rapoo` / `sonix` / `compx` / `rongyuan` / `hidpp` / `standard`）及可选 `min_poll_secs`。
- **声明式自定义 HID `[[custom_hid]]`**：无需编写 Rust 代码，直接通过 TOML 声明 `usage_page`、`usage`、`interface`、`report_type`（`output_input` / `feature` / `get_feature` / `set_feature` / `passive_input`）、`tx_hex`、`rx_prefix_hex`、`rx_match`、`battery_offset`、`battery_max`、`charging_offset`、`charging_mask`、`min_poll_secs` 等字段即可适配新外设。
  - **安全闸门**：外部 `devices.toml` 默认仅允许只读规则（`passive_input` / `get_feature` 且 `tx_hex` 为空）；若需启用主动查询帧，必须显式设置 `allow_write = true`，并且报文须通过报告长度边界检查与危险操作码黑名单（拦截 VIA/QMK 写/刷写指令及 HID++ 写寄存器/DFU 子命令）。

---

## 许可与致谢

- 本项目采用 **[MIT License](LICENSE)** 开源，源码仓库：[AmlitaEnan/AirMonitor](https://github.com/AmlitaEnan/AirMonitor)。
- 无线协议格式与字节偏移信息基于多个开源硬件项目的互操作性协议文档整理与实机校验，完整上游仓库列表、文件路径与许可证归属请参阅 [`docs/protocol_sources.md`](docs/protocol_sources.md) 及 [`docs/protocol_catalog.toml`](docs/protocol_catalog.toml)。
